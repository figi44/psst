"""The engine's request loop, independent of the model behind it."""

from __future__ import annotations

import time
from array import array
from dataclasses import dataclass, field
from typing import Any, BinaryIO, Callable, Protocol

from . import __version__
from .protocol import (
    PROTOCOL_VERSION,
    BadHeader,
    max_payload_bytes,
    parse_header,
    read_frame,
    samples_from_payload,
    write_frame,
)

ProgressFn = Callable[[str, "int | None", "int | None"], None]
"""Reports load progress: ``(phase, bytes, total)`` with phase ``download`` or ``load``."""


class OutOfMemory(Exception):
    """The GPU ran out of memory. The engine replies ``oom`` and keeps running."""


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


class Model(Protocol):
    def load(self, request: LoadRequest, progress: ProgressFn) -> str:
        """Loads the model and returns its resolved revision. Raises ``LoadFailed``."""
        ...

    def transcribe(self, request: TranscribeRequest, samples: array) -> Transcription:
        """Transcribes one recording. Raises ``OutOfMemory``; any other error ends the engine."""
        ...


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

    def serve(self, inp: BinaryIO) -> None:
        """Raises ``CorruptStream`` if the input is corrupt."""
        self._send(
            {
                "type": "hello",
                "protocol": PROTOCOL_VERSION,
                "engine": self.name,
                "version": self.version,
                "capabilities": {"bias": self.bias, "max_audio_s": self.max_audio_s},
            }
        )
        max_payload = max_payload_bytes(self.max_audio_s)
        while (frame := read_frame(inp, max_payload)) is not None:
            if not self._handle(frame.header, frame.payload):
                return

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
            else:
                self._error(request_id, "unsupported", f"unsupported message type: {kind}")
        except InvalidRequest as err:
            self._error(request_id, "bad_request", str(err))
        return True

    def _load(self, request: LoadRequest) -> None:
        self._loaded = None
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
        started = self.clock()
        try:
            transcription = self.model.transcribe(request, samples)
        except OutOfMemory as err:
            self._error(request.id, "oom", str(err), retryable=True)
            return
        decode_ms = round((self.clock() - started) * 1000)
        model, revision = self._loaded
        self._send(
            {
                "type": "result",
                "id": request.id,
                "trace": {
                    "engine": self.name,
                    "model": model,
                    "revision": revision,
                    "path": transcription.path,
                    "options": transcription.options,
                    "prompt": transcription.prompt,
                    "result": transcription.result,
                },
                "timings": {
                    "decode_ms": decode_ms,
                    "prompt_tokens": transcription.prompt_tokens,
                    "bias_terms_used": transcription.bias_terms_used,
                    "prompt_truncated": transcription.prompt_truncated,
                },
            }
        )

    def _error(self, request_id: int | None, code: str, message: str, *, retryable: bool = False) -> None:
        reply: dict[str, Any] = {"type": "error"}
        if request_id is not None:
            reply["id"] = request_id
        reply.update(code=code, message=message, retryable=retryable)
        self._send(reply)

    def _send(self, message: dict[str, Any]) -> None:
        write_frame(self.out, message)
