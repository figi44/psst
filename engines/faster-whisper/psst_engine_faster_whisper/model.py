"""faster-whisper, huggingface_hub and numpy are imported only at the first ``load``, after the
engine has claimed stdout."""

from __future__ import annotations

import dataclasses
import gc
import os
import sys
import traceback
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
"""``BatchedInferencePipeline``: the engine benchmarks chose it for every length."""

DEFAULT_BATCH_SIZE = 8

RECOVERY_RUNS = 2


@dataclass(frozen=True)
class Options:
    """Defaults match ``[model.options]`` in ``psst-config``."""

    path: str = PATH
    beam_size: int = 5
    word_timestamps: bool = True
    vad_filter: bool = True
    terms_as: str = bias.PROMPT
    batch_size: int = DEFAULT_BATCH_SIZE


def parse_options(raw: Mapping[str, Any]) -> Options:
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
    """CTranslate2 raises GPU errors as ``RuntimeError``, with messages such as ``CUDA failed with
    error out of memory`` or ``cuBLAS failed with status CUBLAS_STATUS_ALLOC_FAILED``."""
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
    whisper_model: Callable[..., Any]
    batched_pipeline: Callable[[Any], Any]
    snapshot: Callable[..., str]
    aliases: Mapping[str, str]
    languages: Collection[str]
    audio: Callable[[array], Any]
    warm_up: Callable[[Any], None]
    stream: Callable[[Any, dict[str, Any]], Any] | None = None

    @classmethod
    def faster_whisper(cls) -> Backend:
        import faster_whisper
        import numpy
        from faster_whisper.tokenizer import _LANGUAGE_CODES
        from faster_whisper.utils import _MODELS

        from . import stream

        return cls(
            whisper_model=faster_whisper.WhisperModel,
            batched_pipeline=faster_whisper.BatchedInferencePipeline,
            snapshot=hub.snapshot,
            aliases=_MODELS,
            languages=_LANGUAGE_CODES,
            audio=lambda samples: numpy.frombuffer(samples, dtype=numpy.float32),
            warm_up=_warm_up,
            stream=stream.Stream,
        )


def _warm_up(model: Any) -> None:
    """Without VAD, which would skip the encoder and decoder. Moves about 300 ms of CUDA's one-off
    setup from turbo's first request into its load."""
    import numpy

    silence = numpy.zeros(SAMPLE_RATE, dtype=numpy.float32)
    segments, _ = model.transcribe(silence, language="en", beam_size=5, vad_filter=False, condition_on_previous_text=False)
    for _ in segments:
        pass


def log(message: str) -> None:
    print(f"psst-engine-faster-whisper: {message}", file=sys.stderr, flush=True)


@dataclass
class _Stream:
    request: TranscribeRequest
    options: Options
    rendered: bias.RenderedBias
    live: Any
    buffer: array


class FasterWhisperModel:
    def __init__(self, backend: Backend | None = None) -> None:
        self._backend = backend
        self._model: Any = None
        self._streams: dict[int, _Stream] = {}

    def load(self, request: LoadRequest, progress: ProgressFn) -> str:
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
        backend, model, options, rendered, args = self._prepare(request)

        def run() -> tuple[list[Any], Any]:
            segments, info = backend.batched_pipeline(model).transcribe(backend.audio(samples), **args)
            return list(segments), info

        segments, info = self._on_gpu(run)
        return _transcription(options, rendered, segments, info)

    def open_stream(self, request: TranscribeRequest) -> None:
        backend, model, options, rendered, args = self._prepare(request)
        live = backend.stream(model, args) if backend.stream is not None else None
        self._streams[request.id] = _Stream(request, options, rendered, live, array("f"))

    def append_stream(self, stream_id: int, samples: array) -> None:
        stream = self._streams[stream_id]
        if stream.live is None:
            stream.buffer.extend(samples)
        else:
            stream.live.append(samples)

    def close_stream(self, stream_id: int) -> Transcription:
        stream = self._streams.pop(stream_id)
        if stream.live is None:
            return self.transcribe(stream.request, stream.buffer)
        segments, info, chunks, reused = self._on_gpu(stream.live.close)
        transcription = _transcription(stream.options, stream.rendered, segments, info)
        transcription.chunks, transcription.chunks_reused = chunks, reused
        return transcription

    def discard_stream(self, stream_id: int) -> None:
        self._streams.pop(stream_id, None)

    def background(self) -> bool:
        for stream_id, stream in self._streams.items():
            if stream.live is not None and stream.live.has_work():
                break
        else:
            return False
        # To the back of the dict, so the longest-waiting stream goes first next time.
        del self._streams[stream_id]
        self._streams[stream_id] = stream
        try:
            stream.live.work()
        except Exception as err:
            if isinstance(err, RuntimeError):
                kind = gpu_error(err)
                if kind == "gpu":
                    raise Fatal(f"CUDA error: {err}") from None
                if kind == "oom":
                    self._recover(self._backend, self._model, err)
            traceback.print_exc(file=sys.stderr)
            log(f"decoding stream {stream_id} ahead failed; it's decoded whole at close")
            stream.live.stopped = True
        return True

    def _prepare(
        self, request: TranscribeRequest
    ) -> tuple[Backend, Any, Options, bias.RenderedBias, dict[str, Any]]:
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
        args = {
            "language": language,
            "beam_size": options.beam_size,
            "word_timestamps": options.word_timestamps,
            "vad_filter": options.vad_filter,
            "initial_prompt": rendered.initial_prompt,
            "hotwords": rendered.hotwords,
            "batch_size": options.batch_size,
        }
        return backend, model, options, rendered, args

    def _on_gpu(self, work: Callable[[], Any]) -> Any:
        try:
            return work()
        except RuntimeError as err:
            kind = gpu_error(err)
            if kind == "oom":
                self._recover(self._backend, self._model, err)
                raise OutOfMemory(str(err)) from None
            if kind == "gpu":
                raise Fatal(f"CUDA error: {err}") from None
            raise

    def _recover(self, backend: Backend, model: Any, oom: RuntimeError) -> None:
        """After CTranslate2 runs out of memory, its next call fails with ``cudaErrorInvalidDevice``
        and the one after works. Throwaway warm-ups take those calls, so the server's retry (with
        ``batch_size = 1``) doesn't fail for this run's sake."""
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
        model = backend.whisper_model(snapshot, device=request.device, compute_type=request.compute_type)
        backend.warm_up(model)
        self._model = model
        # A snapshot directory is named after its commit.
        return os.path.basename(os.path.normpath(snapshot))

    def _release(self) -> None:
        self._streams.clear()
        if self._model is not None:
            self._model = None
            gc.collect()


def _transcription(options: Options, rendered: bias.RenderedBias, segments: list[Any], info: Any) -> Transcription:
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
