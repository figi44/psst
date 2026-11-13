"""Streams for faster-whisper: a live recording decoded while it's still coming in (the
"Streams" section of the ``psst-engine-proto`` crate docs).

``close`` answers as ``transcribe`` would for the whole recording, because it is the same call:
faster-whisper's batched pipeline over the whole recording. Three of the pure steps inside it are
answered from what was computed while the recording came in:

- **The speech detector's probabilities.** Silero carries its state from window to window, so the
  probabilities of a recording's first windows don't depend on what follows; they're computed as
  the audio arrives, and only the last, zero-padded window is new at ``close``.
- **Each chunk's decoder output**, keyed by the chunk's features.
- **Each chunk's word alignment**, keyed by its features, tokens and length.

While recording, the pipeline runs over the audio so far whenever a chunk can no longer change:
once the next chunk has begun, or once the last speech segment has closed and its padding fits in
the audio so far. Its result is thrown away, but the chunks it decoded stay cached. A chunk that
changes after all (the speaker went on) has different features, and is simply decoded again.

A recording of one chunk (up to 30 s of speech) gets exactly ``transcribe``'s answer. With more
chunks, those decoded ahead were decoded on their own instead of in one batch, which can move
word timings and probabilities slightly, as another ``batch_size`` does.
"""

from __future__ import annotations

import copy
import hashlib
from typing import Any

import faster_whisper.vad as fw_vad
import numpy as np
from faster_whisper import BatchedInferencePipeline
from faster_whisper.vad import VadOptions, get_speech_timestamps

SAMPLE_RATE = 16_000
WINDOW = 512
"""Samples per speech detector window."""
CONTEXT = 64
"""Samples of the previous window the detector sees with each window."""
LOOK_EVERY = 5 * WINDOW
"""New audio between looks for work: 160 ms."""

_active: _Detector | None = None
"""The stream whose probabilities faster-whisper's speech detector answers with, while set."""
_real_get_vad_model: Any = None


def _install(model: Any) -> None:
    """Routes faster-whisper's speech detector and word alignment through the streams' caches.
    Both pass through unchanged when no stream is being worked on."""
    global _real_get_vad_model
    if _real_get_vad_model is None:
        _real_get_vad_model = fw_vad.get_vad_model

        def get_vad_model() -> Any:
            return _real_get_vad_model() if _active is None else _active

        fw_vad.get_vad_model = get_vad_model
    if not getattr(model, "_psst_streams", False):
        original = model.find_alignment

        def find_alignment(tokenizer: Any, text_tokens: Any, encoder_output: Any, num_frames: Any, *args: Any, **kwargs: Any) -> Any:
            if isinstance(encoder_output, _Batch):
                return encoder_output.align(original, tokenizer, text_tokens, num_frames, *args, **kwargs)
            return original(tokenizer, text_tokens, encoder_output, num_frames, *args, **kwargs)

        model.find_alignment = find_alignment
        model._psst_streams = True


class _Detector:
    """Silero's speech probabilities for a growing recording, as ``SileroVADModel`` computes them for
    the whole of it."""

    def __init__(self) -> None:
        self._session = _real_get_vad_model().session
        self._h = np.zeros((1, 1, 128), dtype=np.float32)
        self._c = np.zeros((1, 1, 128), dtype=np.float32)
        self._context = np.zeros(CONTEXT, dtype=np.float32)
        self._probs: list[np.ndarray] = []
        self.windows = 0

    def advance(self, audio: np.ndarray) -> None:
        """Computes the probabilities of ``audio``'s whole windows that aren't computed yet."""
        windows = len(audio) // WINDOW
        if windows <= self.windows:
            return
        batch = audio[self.windows * WINDOW : windows * WINDOW].reshape(-1, WINDOW)
        contexts = np.concatenate([self._context[None], batch[:-1, -CONTEXT:]])
        probs, self._h, self._c = self._session.run(
            None, {"input": np.concatenate([contexts, batch], axis=1), "h": self._h, "c": self._c}
        )
        self._probs.append(probs)
        self._context = batch[-1, -CONTEXT:].copy()
        self.windows = windows

    def __call__(self, padded: np.ndarray) -> np.ndarray:
        """What ``SileroVADModel`` answers for ``padded``, the audio so far padded to whole windows
        by ``get_speech_timestamps``: the cached windows, and the last one computed now."""
        if len(padded) // WINDOW != self.windows + 1:
            raise RuntimeError(f"the detector holds {self.windows} windows, not {len(padded) // WINDOW - 1}")
        last = padded[-WINDOW:].copy()
        last[-CONTEXT:] = 0  # as SileroVADModel does to the last window's context source
        probs, _, _ = self._session.run(
            None, {"input": np.concatenate([self._context, last])[None], "h": self._h, "c": self._c}
        )
        return np.concatenate([*self._probs, probs], axis=0)


class Stream:
    """One live recording, decoded ahead while it comes in."""

    def __init__(self, model: Any, transcribe_args: dict[str, Any]) -> None:
        _install(model)
        self._model = model
        self._args = transcribe_args
        self._audio = np.zeros(10 * SAMPLE_RATE, dtype=np.float32)
        self._samples = 0
        self._detector = _Detector()
        chunk_length = model.feature_extractor.chunk_length
        # The batched pipeline's own speech detector options and chunk length.
        self._vad_options = VadOptions(max_speech_duration_s=chunk_length, min_silence_duration_ms=160)
        self._chunk_samples = chunk_length * SAMPLE_RATE
        self.decoded: dict[bytes, dict[str, Any]] = {}
        self.aligned: dict[tuple[bytes, tuple[int, ...], int], list[dict[str, Any]]] = {}
        self._looked_at = 0
        self._closed_ahead = 0
        self._paused_ahead: tuple[tuple[int, int], ...] = ()
        self.stopped = False
        """Set after decoding ahead failed: the stream no longer looks for work."""

    def append(self, samples: Any) -> None:
        new = np.frombuffer(samples, dtype=np.float32)
        end = self._samples + len(new)
        if end > len(self._audio):
            grown = np.zeros(max(end, 2 * len(self._audio)), dtype=np.float32)
            grown[: self._samples] = self._audio[: self._samples]
            self._audio = grown
        self._audio[self._samples : end] = new
        self._samples = end

    def has_work(self) -> bool:
        """Whether enough new audio came in to look for work."""
        return not self.stopped and self._samples - self._looked_at >= LOOK_EVERY

    def work(self) -> bool:
        """Decodes ahead the chunks of the audio so far that can no longer change, if any
        aren't yet; whether it did."""
        self._looked_at = self._samples
        audio = self._audio[: self._samples]
        self._detector.advance(audio)
        segments = self._speech(audio)
        if not segments:
            return False
        groups = _chunks(segments, self._chunk_samples)
        closed = len(groups) - 1
        # The last segment's end is final once speech has stopped for long enough that the
        # segment closed, and its padding didn't have to stop at the end of the audio.
        paused = segments[-1]["end"] + WINDOW <= self._samples
        last = tuple((segment["start"], segment["end"]) for segment in groups[-1])
        if closed <= self._closed_ahead and not (paused and last != self._paused_ahead):
            return False
        self._run(audio)
        self._closed_ahead = closed
        if paused:
            self._paused_ahead = last
        return True

    def close(self) -> tuple[list[Any], Any, int, int]:
        """The transcription of the whole recording: segments, info, chunks, and chunks reused."""
        audio = self._audio[: self._samples]
        self._detector.advance(audio)
        return self._run(audio)

    def _speech(self, audio: np.ndarray) -> list[dict[str, int]]:
        global _active
        _active = self._detector
        try:
            return get_speech_timestamps(audio, self._vad_options)
        finally:
            _active = None

    def _run(self, audio: np.ndarray) -> tuple[list[Any], Any, int, int]:
        global _active
        pipeline = _CachingPipeline(self._model, self)
        _active = self._detector
        try:
            segments, info = pipeline.transcribe(audio, **self._args)
            segments = list(segments)
        finally:
            _active = None
        return segments, info, pipeline.chunks, pipeline.reused


def _chunks(segments: list[dict[str, int]], max_samples: int) -> list[list[dict[str, int]]]:
    """The speech segments of each chunk, as ``faster_whisper.vad.collect_chunks`` groups them."""
    groups: list[list[dict[str, int]]] = []
    current: list[dict[str, int]] = []
    length = 0
    for segment in segments:
        size = segment["end"] - segment["start"]
        if length + size > max_samples:
            groups.append(current)
            current, length = [segment], size
        else:
            current.append(segment)
            length += size
    groups.append(current)
    return groups


class _CachingPipeline(BatchedInferencePipeline):
    """The batched pipeline, decoding only the chunks the stream hasn't decoded yet."""

    def __init__(self, model: Any, stream: Stream) -> None:
        super().__init__(model)
        self._stream = stream
        self.chunks = 0
        self.reused = 0

    def generate_segment_batched(self, features: np.ndarray, tokenizer: Any, options: Any) -> tuple[Any, list[dict]]:
        keys = [hashlib.blake2b(row.tobytes(), digest_size=16).digest() for row in features]
        decoded = self._stream.decoded
        missing = [i for i, key in enumerate(keys) if key not in decoded]
        encoder_output = None
        if missing:
            encoder_output, outputs = super().generate_segment_batched(features[missing], tokenizer, options)
            for i, output in zip(missing, outputs):
                decoded[keys[i]] = copy.deepcopy(output)
        self.chunks += len(keys)
        self.reused += len(keys) - len(missing)
        batch = _Batch(self.model, self._stream, keys, missing, encoder_output, features)
        return batch, [copy.deepcopy(decoded[key]) for key in keys]


class _Batch:
    """A batch's encoder output, where only the chunks decoded now have one."""

    def __init__(
        self, model: Any, stream: Stream, keys: list[bytes], missing: list[int], encoder_output: Any, features: np.ndarray
    ) -> None:
        self._model = model
        self._stream = stream
        self._keys = keys
        self._missing = missing
        self._encoder_output = encoder_output
        self._features = features

    def align(self, original: Any, tokenizer: Any, text_tokens: list[list[int]], num_frames: list[int], *args: Any, **kwargs: Any) -> list[Any]:
        aligned = self._stream.aligned
        keys = [(key, tuple(tokens), frames) for key, tokens, frames in zip(self._keys, text_tokens, num_frames)]
        missing = [i for i, key in enumerate(keys) if key not in aligned]
        if missing:
            if missing == self._missing:
                encoder_output = self._encoder_output
            else:
                encoder_output = self._model.encode(self._features[missing])
            results = original(
                tokenizer, [text_tokens[i] for i in missing], encoder_output, [num_frames[i] for i in missing], *args, **kwargs
            )
            for i, result in zip(missing, results):
                aligned[keys[i]] = copy.deepcopy(result)
        # add_word_timestamps edits the alignments in place.
        return [copy.deepcopy(aligned[key]) for key in keys]
