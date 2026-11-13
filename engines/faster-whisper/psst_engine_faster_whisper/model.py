"""faster-whisper 1.2.1 behind the engine's ``Model`` interface.

faster-whisper, huggingface_hub and numpy are imported at the first ``load``, after the engine has
claimed stdout and said hello. ``Backend`` is everything used from them.
"""

from __future__ import annotations

import dataclasses
import gc
import os
import sys
from array import array
from collections.abc import Callable, Collection, Mapping
from dataclasses import dataclass
from typing import Any

from . import bias, hub
from .engine import (
    Fatal,
    InvalidRequest,
    LoadFailed,
    LoadRequest,
    OutOfMemory,
    ProgressFn,
    TranscribeRequest,
    Transcription,
)

SAMPLE_RATE = 16_000

PATH = "batched"
"""The one decoding path: faster-whisper's ``BatchedInferencePipeline``, which the engine benchmarks
chose for every length."""

DEFAULT_BATCH_SIZE = 8
"""Batch size when ``options.batch_size`` is unset, as in ``psst-config``."""

RECOVERY_RUNS = 2
"""Throwaway runs after the GPU ran out of memory, until one works (``FasterWhisperModel._recover``)."""


@dataclass(frozen=True)
class Options:
    """``transcribe.options``, from ``[model.options]``, with the defaults in ``psst-config``."""

    path: str = PATH
    beam_size: int = 5
    word_timestamps: bool = True
    vad_filter: bool = True
    terms_as: str = bias.PROMPT
    batch_size: int = DEFAULT_BATCH_SIZE


def parse_options(raw: Mapping[str, Any]) -> Options:
    """Validates ``transcribe.options``, ignoring keys it doesn't know. Raises ``InvalidRequest``."""

    def get(name: str, valid: Callable[[Any], bool], expected: str) -> Any:
        value = raw.get(name)
        if value is None:
            return getattr(Options, name)
        if not valid(value):
            raise InvalidRequest(f"`options.{name}` must be {expected}, not {value!r}")
        return value

    def positive(value: Any) -> bool:
        return isinstance(value, int) and not isinstance(value, bool) and value >= 1

    def boolean(value: Any) -> bool:
        return isinstance(value, bool)

    options = Options(
        path=get("path", lambda v: v == PATH, PATH),
        beam_size=get("beam_size", positive, "an integer of at least 1"),
        word_timestamps=get("word_timestamps", boolean, "true or false"),
        vad_filter=get("vad_filter", boolean, "true or false"),
        terms_as=get("terms_as", lambda v: v in (bias.PROMPT, bias.HOTWORDS), "prompt or hotwords"),
        batch_size=get("batch_size", positive, "an integer of at least 1"),
    )
    if not options.vad_filter:
        # faster-whisper's batched path refuses audio of 30 s or more without VAD.
        raise InvalidRequest(f"`options.path = {PATH}` needs `vad_filter = true`")
    return options


_OOM_MARKERS = ("out of memory", "alloc_failed", "cudaerrormemoryallocation")
_GPU_MARKERS = ("cuda", "cublas", "cudnn", "cufft", "curand", "cusparse", "nccl", "nvrtc")


def gpu_error(err: BaseException) -> str | None:
    """``"oom"`` for a GPU out-of-memory error, ``"gpu"`` for any other CUDA error, else ``None``.

    CTranslate2 raises both as ``RuntimeError``, with messages such as ``CUDA failed with error out
    of memory`` or ``cuBLAS failed with status CUBLAS_STATUS_ALLOC_FAILED``.
    """
    if not isinstance(err, RuntimeError):
        return None
    text = str(err).lower()
    if any(marker in text for marker in _OOM_MARKERS):
        return "oom"
    if any(marker in text for marker in _GPU_MARKERS):
        return "gpu"
    return None


@dataclass(frozen=True)
class Backend:
    """What the model uses from faster-whisper, huggingface_hub and numpy."""

    whisper_model: Callable[..., Any]
    """``faster_whisper.WhisperModel``."""
    batched_pipeline: Callable[[Any], Any]
    """``faster_whisper.BatchedInferencePipeline``."""
    snapshot: Callable[..., str]
    """``hub.snapshot``: a model's snapshot directory, from the cache or downloaded."""
    aliases: Mapping[str, str]
    """Model names faster-whisper resolves to Hub repositories."""
    languages: Collection[str]
    """Language codes Whisper knows."""
    audio: Callable[[array], Any]
    """Turns f32 samples into what ``transcribe`` takes."""
    warm_up: Callable[[Any], None]
    """Runs a loaded model once, so the first dictation doesn't pay for CUDA's one-off setup."""

    @classmethod
    def faster_whisper(cls) -> Backend:
        import faster_whisper
        import numpy
        from faster_whisper.tokenizer import _LANGUAGE_CODES
        from faster_whisper.utils import _MODELS

        return cls(
            whisper_model=faster_whisper.WhisperModel,
            batched_pipeline=faster_whisper.BatchedInferencePipeline,
            snapshot=hub.snapshot,
            aliases=_MODELS,
            languages=_LANGUAGE_CODES,
            audio=lambda samples: numpy.frombuffer(samples, dtype=numpy.float32),
            warm_up=_warm_up,
        )


def _warm_up(model: Any) -> None:
    """One second of silence through the encoder and decoder, without VAD (which would skip both).

    On the RTX 3060 Ti this moves about 300 ms from turbo's first request into its load.
    """
    import numpy

    silence = numpy.zeros(SAMPLE_RATE, dtype=numpy.float32)
    segments, _ = model.transcribe(silence, language="en", beam_size=5, vad_filter=False, condition_on_previous_text=False)
    for _ in segments:
        pass


def log(message: str) -> None:
    print(f"psst-engine-faster-whisper: {message}", file=sys.stderr, flush=True)


class FasterWhisperModel:
    def __init__(self, backend: Backend | None = None) -> None:
        self._backend = backend
        self._model: Any = None

    def load(self, request: LoadRequest, progress: ProgressFn) -> str:
        """Loads from the local cache, or downloads once if the model is missing or fails to load."""
        self._release()
        backend = self._get_backend()
        try:
            repo = hub.repo_id(request.model, backend.aliases)
        except ValueError as err:
            raise LoadFailed(str(err)) from None

        force = False
        try:
            snapshot: str | None = backend.snapshot(repo, request.revision, local_files_only=True)
        except Exception as err:
            log(f"{repo} is not in the cache ({type(err).__name__}); downloading it")
            snapshot = None
        if snapshot is not None:
            progress("load", None, None)
            try:
                return self._open(backend, snapshot, request)
            except Exception as err:
                kind = gpu_error(err)
                if kind == "gpu":
                    raise Fatal(f"CUDA error while loading {repo}: {err}") from None
                # A bad device or compute type, or no GPU memory left: downloading again won't help.
                if kind == "oom" or isinstance(err, ValueError):
                    raise LoadFailed(f"loading {repo} failed: {err}") from None
                log(f"the cached {repo} failed to load ({type(err).__name__}: {err}); downloading it again")
                force = True

        progress("download", None, None)
        transfer = hub.TransferProgress(lambda done, total: progress("download", done, total))
        try:
            snapshot = backend.snapshot(repo, request.revision, force_download=force, transfer=transfer)
        except Exception as err:
            raise LoadFailed(f"downloading {repo} failed: {type(err).__name__}: {err}") from None
        finally:
            transfer.close()
        progress("load", None, None)
        try:
            return self._open(backend, snapshot, request)
        except Exception as err:
            if gpu_error(err) == "gpu":
                raise Fatal(f"CUDA error while loading {repo}: {err}") from None
            raise LoadFailed(f"loading {repo} failed: {type(err).__name__}: {err}") from None

    def transcribe(self, request: TranscribeRequest, samples: array) -> Transcription:
        backend, model = self._backend, self._model
        if backend is None or model is None:
            raise RuntimeError("transcribe before a successful load")
        options = parse_options(request.options)
        language = request.language or None
        if language is not None and language not in backend.languages:
            raise InvalidRequest(f"`language` {language!r} is not a Whisper language code")

        rendered = bias.render(
            request.bias.style,
            request.bias.terms,
            terms_as=options.terms_as,
            encode=lambda text: model.hf_tokenizer.encode(text, add_special_tokens=False).ids,
        )
        try:
            segments, info = backend.batched_pipeline(model).transcribe(
                backend.audio(samples),
                language=language,
                beam_size=options.beam_size,
                word_timestamps=options.word_timestamps,
                vad_filter=options.vad_filter,
                initial_prompt=rendered.initial_prompt,
                hotwords=rendered.hotwords,
                batch_size=options.batch_size,
            )
            segments = list(segments)
        except RuntimeError as err:
            kind = gpu_error(err)
            if kind == "oom":
                self._recover(backend, model, err)
                raise OutOfMemory(str(err)) from None
            if kind == "gpu":
                raise Fatal(f"CUDA error: {err}") from None
            raise

        return Transcription(
            result={
                "language": info.language,
                "speech_ms": _ms(info.duration_after_vad),
                "segments": [_segment(segment) for segment in segments],
            },
            path=PATH,
            options=dataclasses.asdict(options),
            prompt=rendered.text,
            prompt_tokens=rendered.tokens,
            bias_terms_used=rendered.terms_used,
            prompt_truncated=rendered.truncated,
        )

    def _recover(self, backend: Backend, model: Any, oom: RuntimeError) -> None:
        """Makes sure the model can run again after the GPU ran out of memory, so that the server's
        retry (with ``batch_size = 1``) doesn't fail for this run's sake.

        After CTranslate2 runs out of memory, its next call fails with ``cudaErrorInvalidDevice``
        and the one after works. The warm-up's second of silence is
        that next call: it runs until it works, at most ``RECOVERY_RUNS`` times. Raises ``Fatal``
        when none works, so the engine exits as for any other CUDA error.
        """
        failures = []
        for _ in range(RECOVERY_RUNS):
            try:
                backend.warm_up(model)
                return
            except Exception as err:
                failures.append(f"{type(err).__name__}: {err}")
        log(f"ran out of GPU memory ({oom}), and the throwaway runs after it failed: {'; '.join(failures)}")
        raise Fatal(f"the model doesn't run after running out of GPU memory: {failures[-1]}")

    def _get_backend(self) -> Backend:
        if self._backend is None:
            try:
                self._backend = Backend.faster_whisper()
            except ImportError as err:
                raise LoadFailed(f"faster-whisper is not installed: {err}") from None
        return self._backend

    def _open(self, backend: Backend, snapshot: str, request: LoadRequest) -> str:
        """Loads a snapshot directory, warms the model up, and returns the snapshot's revision (the
        commit it's named after)."""
        model = backend.whisper_model(snapshot, device=request.device, compute_type=request.compute_type)
        backend.warm_up(model)
        self._model = model
        return os.path.basename(os.path.normpath(snapshot))

    def _release(self) -> None:
        if self._model is not None:
            self._model = None
            gc.collect()


def _segment(segment: Any) -> dict[str, Any]:
    out: dict[str, Any] = {
        "text": segment.text,
        "start_ms": _ms(segment.start),
        "end_ms": _ms(segment.end),
        "no_speech_prob": _prob(segment.no_speech_prob),
    }
    if segment.words is not None:
        out["words"] = [
            {"text": word.word, "start_ms": _ms(word.start), "end_ms": _ms(word.end), "prob": _prob(word.probability)}
            for word in segment.words
        ]
    return out


def _ms(seconds: float) -> int:
    return max(0, round(float(seconds) * 1000))


def _prob(value: float) -> float:
    return round(float(value), 4)
