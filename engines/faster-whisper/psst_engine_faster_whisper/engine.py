"""The engine's request loop, independent of the model behind it.

A reader thread queues the frames as they arrive, so that between requests the main thread can do
the background work a model offers: a model with streams decodes what it can while a live
recording is still coming in. All model calls happen on the main thread.
"""

from __future__ import annotations

import queue
import sys
import threading
import time
import traceback
from array import array
from dataclasses import dataclass, field
from typing import Any, BinaryIO, Callable, Protocol

from . import __version__
from .protocol import (
    PROTOCOL_VERSION,
    BadHeader,
    Frame,
    max_payload_bytes,
    parse_header,
    read_frame,
    samples_from_payload,
    write_frame,
)

SAMPLE_RATE = 16_000

ProgressFn = Callable[[str, "int | None", "int | None"], None]
"""Reports load progress: ``(phase, bytes, total)`` with phase ``download`` or ``load``."""


class OutOfMemory(Exception):
    """The GPU ran out of memory. The engine replies ``oom`` and keeps running."""


class Fatal(Exception):
    """The engine can't go on, for example after a CUDA error other than running out of memory.

    It exits without replying, so the server sees a crash, restarts it and retries.
    """


class LoadFailed(Exception):
    """Loading or downloading the model failed. The engine replies ``load_failed``."""


class InvalidRequest(Exception):
    """A known request type with missing or invalid fields."""


@dataclass(frozen=True)
class LoadRequest:
    model: str
    revision: str | None
    device: str
    compute_type: str


@dataclass(frozen=True)
class Bias:
    style: str = ""
    terms: tuple[str, ...] = ()


@dataclass(frozen=True)
class TranscribeRequest:
    id: int
    language: str
    bias: Bias
    options: dict[str, Any]


@dataclass
class Transcription:
    """What a model returns for one request."""

    result: dict[str, Any]
    """An ``EngineResult``: ``{language, speech_ms?, segments: [...]}``."""
    path: str
    """Decoding path actually taken."""
    options: dict[str, Any]
    """Effective options."""
    prompt: str
    """Prompt and hotwords actually used."""
    prompt_tokens: int = 0
    bias_terms_used: int = 0
    prompt_truncated: bool = False
    chunks: int = 0
    """For a stream: the chunks of up to 30 s the recording was decoded in…"""
    chunks_reused: int = 0
    """…and how many of them were decoded before ``close``."""


class Model(Protocol):
    def load(self, request: LoadRequest, progress: ProgressFn) -> str:
        """Loads the model and returns its resolved revision. Raises ``LoadFailed``."""
        ...

    def transcribe(self, request: TranscribeRequest, samples: array) -> Transcription:
        """Transcribes one recording.

        Raises ``InvalidRequest`` for bad options, ``OutOfMemory`` when the GPU runs out of memory,
        and ``Fatal`` when the engine must exit. Any other error is answered with ``internal``.
        """
        ...


class StreamingModel(Model, Protocol):
    """A model that works on streams while their audio comes in. The engine buffers the streams of
    models without these methods, and transcribes them at ``close``."""

    def open_stream(self, request: TranscribeRequest) -> None:
        """Starts a stream. Raises ``InvalidRequest`` as ``transcribe`` would."""
        ...

    def append_stream(self, stream_id: int, samples: array) -> None: ...

    def close_stream(self, stream_id: int) -> Transcription:
        """The stream's transcription, exactly as ``transcribe`` gives it for the whole recording;
        raises as ``transcribe`` does. The stream is gone afterwards."""
        ...

    def discard_stream(self, stream_id: int) -> None: ...

    def background(self) -> bool:
        """Does one piece of the streams' work, if there is any; whether it did."""
        ...


@dataclass
class _Stream:
    request: TranscribeRequest
    samples: int = 0
    buffer: array | None = None
    """The audio so far, for a model without streams; ``None`` when the model has the stream."""


def _field(message: dict[str, Any], name: str, kind: type | tuple[type, ...], *, optional: bool = False) -> Any:
    value = message.get(name)
    if value is None and optional:
        return None
    if not isinstance(value, kind) or (isinstance(value, bool) and kind is int):
        raise InvalidRequest(f"`{message['type']}.{name}` is missing or invalid")
    return value


def parse_load(message: dict[str, Any]) -> LoadRequest:
    return LoadRequest(
        model=_field(message, "model", str),
        revision=_field(message, "revision", str, optional=True),
        device=_field(message, "device", str),
        compute_type=_field(message, "compute_type", str),
    )


def _is_shutdown(header: bytes) -> bool:
    try:
        return parse_header(header)["type"] == "shutdown"
    except BadHeader:
        return False


def parse_stream_id(message: dict[str, Any]) -> int:
    """The ``id`` of an ``append``, ``close`` or ``discard``."""
    return _field(message, "id", int)


def parse_transcribe(message: dict[str, Any]) -> TranscribeRequest:
    bias = _field(message, "bias", dict)
    style = bias.get("style")
    terms = bias.get("terms")
    if not isinstance(style, str) or not isinstance(terms, list) or not all(isinstance(t, str) for t in terms):
        raise InvalidRequest("`transcribe.bias` must be {style: string, terms: [string]}")
    return TranscribeRequest(
        id=_field(message, "id", int),
        language=_field(message, "language", str),
        bias=Bias(style=style, terms=tuple(terms)),
        options=_field(message, "options", dict),
    )


@dataclass
class Engine:
    """Serves one server: says hello, then answers requests until ``shutdown`` or end of input."""

    model: Model
    out: BinaryIO
    name: str = "faster-whisper"
    version: str = __version__
    max_audio_s: int = 900
    bias: bool = True
    clock: Callable[[], float] = time.monotonic
    _loaded: tuple[str, str] | None = field(default=None, init=False)
    _send_lock: threading.Lock = field(default_factory=threading.Lock, init=False, repr=False)
    _streams: dict[int, _Stream] = field(default_factory=dict, init=False, repr=False)

    def serve(self, inp: BinaryIO) -> None:
        """Raises ``CorruptStream`` if the input is corrupt, and ``Fatal`` if the engine must exit."""
        self._send(
            {
                "type": "hello",
                "protocol": PROTOCOL_VERSION,
                "engine": self.name,
                "version": self.version,
                "capabilities": {"bias": self.bias, "max_audio_s": self.max_audio_s, "stream": True},
            }
        )
        max_payload = max_payload_bytes(self.max_audio_s)
        frames: queue.SimpleQueue[Frame | BaseException | None] = queue.SimpleQueue()

        def read() -> None:
            try:
                while (frame := read_frame(inp, max_payload)) is not None:
                    frames.put(frame)
                    # Stop reading: a thread blocked in a read of stdin, which the server keeps
                    # open, would make the interpreter abort at exit.
                    if _is_shutdown(frame.header):
                        return
                frames.put(None)
            except BaseException as err:  # noqa: BLE001 - handed to the main thread
                frames.put(err)

        threading.Thread(target=read, name="psst-engine-reader", daemon=True).start()
        while True:
            try:
                item = frames.get_nowait()
            except queue.Empty:
                if self._background():
                    continue
                item = frames.get()
            if item is None:
                return
            if isinstance(item, BaseException):
                raise item
            if not self._handle(item.header, item.payload):
                return

    def _background(self) -> bool:
        background = getattr(self.model, "background", None)
        return bool(self._loaded is not None and self._streams and background is not None and background())

    def _handle(self, header: bytes, payload: bytes) -> bool:
        try:
            message = parse_header(header)
        except BadHeader as err:
            self._error(None, "bad_request", str(err))
            return True
        kind = message["type"]
        request_id = message.get("id") if isinstance(message.get("id"), int) else None
        try:
            if kind == "shutdown":
                return False
            if kind == "load":
                self._load(parse_load(message))
            elif kind == "transcribe":
                self._transcribe(parse_transcribe(message), payload)
            elif kind == "open":
                self._open(parse_transcribe(message))
            elif kind == "append":
                self._append(parse_stream_id(message), payload)
            elif kind == "close":
                self._close(parse_stream_id(message))
            elif kind == "discard":
                self._discard(parse_stream_id(message))
            else:
                self._error(request_id, "unsupported", f"unsupported message type: {kind}")
        except InvalidRequest as err:
            self._error(request_id, "bad_request", str(err))
        return True

    def _load(self, request: LoadRequest) -> None:
        self._loaded = None
        self._streams.clear()
        started = self.clock()

        def progress(phase: str, done: int | None = None, total: int | None = None) -> None:
            message: dict[str, Any] = {"type": "progress", "phase": phase}
            if done is not None:
                message["bytes"] = done
            if total is not None:
                message["total"] = total
            self._send(message)

        try:
            revision = self.model.load(request, progress)
        except LoadFailed as err:
            self._error(None, "load_failed", str(err))
            return
        except (InvalidRequest, Fatal):
            raise
        except Exception as err:
            traceback.print_exc(file=sys.stderr)
            self._error(None, "load_failed", f"{type(err).__name__}: {err}")
            return
        self._loaded = (request.model, revision)
        load_ms = round((self.clock() - started) * 1000)
        self._send({"type": "loaded", "model": request.model, "revision": revision, "load_ms": load_ms})

    def _transcribe(self, request: TranscribeRequest, payload: bytes) -> None:
        if self._loaded is None:
            self._error(request.id, "not_loaded", "transcribe before a successful load")
            return
        try:
            samples = samples_from_payload(payload)
        except ValueError as err:
            self._error(request.id, "bad_request", str(err))
            return
        self._answer(request.id, lambda: self.model.transcribe(request, samples))

    def _open(self, request: TranscribeRequest) -> None:
        if self._loaded is None:
            self._error(request.id, "not_loaded", "open before a successful load")
            return
        if request.id in self._streams:
            raise InvalidRequest(f"stream {request.id} is already open")
        open_stream = getattr(self.model, "open_stream", None)
        if open_stream is None:
            self._streams[request.id] = _Stream(request, buffer=array("f"))
        else:
            open_stream(request)
            self._streams[request.id] = _Stream(request)

    def _append(self, stream_id: int, payload: bytes) -> None:
        stream = self._streams.get(stream_id)
        if stream is None:
            raise InvalidRequest(f"no open stream {stream_id}")
        try:
            samples = samples_from_payload(payload)
        except ValueError as err:
            raise InvalidRequest(str(err)) from None
        stream.samples += len(samples)
        if stream.samples > self.max_audio_s * SAMPLE_RATE:
            self._discard(stream_id)
            raise InvalidRequest(f"stream {stream_id} is longer than max_audio_s = {self.max_audio_s} s")
        if stream.buffer is None:
            self.model.append_stream(stream_id, samples)  # type: ignore[attr-defined]
        else:
            stream.buffer.extend(samples)

    def _close(self, stream_id: int) -> None:
        stream = self._streams.pop(stream_id, None)
        if stream is None:
            raise InvalidRequest(f"no open stream {stream_id}")
        if stream.buffer is None:
            self._answer(stream_id, lambda: self.model.close_stream(stream_id), stream=True)  # type: ignore[attr-defined]
        else:
            buffered = stream.buffer
            self._answer(stream_id, lambda: self.model.transcribe(stream.request, buffered), stream=True)

    def _discard(self, stream_id: int) -> None:
        stream = self._streams.pop(stream_id, None)
        if stream is not None and stream.buffer is None:
            self.model.discard_stream(stream_id)  # type: ignore[attr-defined]

    def _answer(self, request_id: int, transcribe: Callable[[], Transcription], *, stream: bool = False) -> None:
        """Runs a transcription and replies with its ``result`` or ``error``."""
        started = self.clock()
        try:
            transcription = transcribe()
        except OutOfMemory as err:
            self._error(request_id, "oom", str(err), retryable=True)
            return
        except (InvalidRequest, Fatal):
            raise
        except Exception as err:
            traceback.print_exc(file=sys.stderr)
            self._error(request_id, "internal", f"{type(err).__name__}: {err}")
            return
        decode_ms = round((self.clock() - started) * 1000)
        assert self._loaded is not None
        model, revision = self._loaded
        timings: dict[str, Any] = {
            "decode_ms": decode_ms,
            "prompt_tokens": transcription.prompt_tokens,
            "bias_terms_used": transcription.bias_terms_used,
            "prompt_truncated": transcription.prompt_truncated,
        }
        if stream:
            timings.update(chunks=transcription.chunks, chunks_reused=transcription.chunks_reused)
        self._send(
            {
                "type": "result",
                "id": request_id,
                "trace": {
                    "engine": self.name,
                    "model": model,
                    "revision": revision,
                    "path": transcription.path,
                    "options": transcription.options,
                    "prompt": transcription.prompt,
                    "result": transcription.result,
                },
                "timings": timings,
            }
        )

    def _error(self, request_id: int | None, code: str, message: str, *, retryable: bool = False) -> None:
        reply: dict[str, Any] = {"type": "error"}
        if request_id is not None:
            reply["id"] = request_id
        reply.update(code=code, message=message, retryable=retryable)
        self._send(reply)

    def _send(self, message: dict[str, Any]) -> None:
        # Download progress is reported from the downloader's threads.
        with self._send_lock:
            write_frame(self.out, message)
