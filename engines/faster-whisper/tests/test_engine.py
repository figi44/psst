"""The request loop, driven through its input and output streams with a stub model."""

from __future__ import annotations

import io
import os
import subprocess
import sys
import threading
from pathlib import Path
from array import array
from typing import Any

import pytest

from psst_engine_faster_whisper.engine import Engine, Fatal, TranscribeRequest, Transcription, parse_transcribe
from psst_engine_faster_whisper.protocol import parse_header, payload_from_samples, read_frame, write_frame
from stub_model import FakeClock, StubModel

LOAD = {"type": "load", "model": "large-v3-turbo", "device": "cpu", "compute_type": "int8"}


def transcribe(request_id: int) -> dict[str, Any]:
    return {
        "type": "transcribe",
        "id": request_id,
        "language": "en",
        "bias": {"style": "", "terms": []},
        "options": {"path": "batched"},
    }


AUDIO = payload_from_samples([0.0] * 1600)


def serve(
    *requests: tuple[dict[str, Any] | bytes, bytes],
    oom_ids: frozenset[int] = frozenset(),
    errors: dict[int, Exception] | None = None,
    replies: list[dict] | None = None,
    model: StubModel | None = None,
) -> list[dict]:
    inp = io.BytesIO()
    for header, payload in requests:
        if isinstance(header, bytes):
            inp.write(len(header).to_bytes(4, "little") + len(payload).to_bytes(4, "little") + header + payload)
        else:
            write_frame(inp, header, payload)
    inp.seek(0)
    out = io.BytesIO()
    clock = FakeClock()
    replies = [] if replies is None else replies
    model = model or StubModel(clock, oom_ids, errors)
    try:
        Engine(model, out, clock=model.clock).serve(inp)
    finally:
        out.seek(0)
        while (frame := read_frame(out, 1 << 20)) is not None:
            replies.append(parse_header(frame.header))
    return replies


def test_says_hello_first_then_loads_and_transcribes():
    replies = serve((LOAD, b""), (transcribe(1), AUDIO))
    assert [r["type"] for r in replies] == ["hello", "progress", "progress", "loaded", "result"]
    assert replies[0]["capabilities"] == {"bias": True, "max_audio_s": 900, "stream": True}
    assert replies[3]["load_ms"] == 2400
    assert replies[4]["id"] == 1
    assert replies[4]["timings"]["decode_ms"] == 180
    assert replies[4]["trace"]["model"] == "large-v3-turbo"


def test_stops_at_shutdown():
    replies = serve((LOAD, b""), ({"type": "shutdown"}, b""), (transcribe(1), AUDIO))
    assert [r["type"] for r in replies] == ["hello", "progress", "progress", "loaded"]


def test_answers_bad_requests_with_errors_and_keeps_serving():
    replies = serve(
        (transcribe(1), AUDIO),
        ({"type": "frobnicate", "id": 2}, b""),
        (b"not json", b""),
        ({"type": "load", "model": "x"}, b""),
        (LOAD, b""),
        ({**transcribe(3), "bias": {"style": "", "terms": [1]}}, AUDIO),
        (transcribe(4), b"\x00\x00\x00"),
        (transcribe(5), AUDIO),
    )
    errors = [(r.get("id"), r["code"], r["retryable"]) for r in replies if r["type"] == "error"]
    assert errors == [
        (1, "not_loaded", False),
        (2, "unsupported", False),
        (None, "bad_request", False),
        (None, "bad_request", False),
        (3, "bad_request", False),
        (4, "bad_request", False),
    ]
    assert replies[-1]["type"] == "result" and replies[-1]["id"] == 5


def test_out_of_memory_is_an_error_reply_not_an_exit():
    replies = serve((LOAD, b""), (transcribe(1), AUDIO), (transcribe(2), AUDIO), oom_ids=frozenset({1}))
    assert replies[-2] == {"type": "error", "id": 1, "code": "oom", "message": "CUDA out of memory (stub)", "retryable": True}
    assert replies[-1]["type"] == "result" and replies[-1]["id"] == 2


def test_other_model_errors_are_internal_errors_and_the_engine_keeps_serving():
    replies = serve((LOAD, b""), (transcribe(1), AUDIO), (transcribe(2), AUDIO), errors={1: KeyError("segments")})
    assert replies[-2] == {"type": "error", "id": 1, "code": "internal", "message": "KeyError: 'segments'", "retryable": False}
    assert replies[-1]["type"] == "result" and replies[-1]["id"] == 2


def test_a_fatal_model_error_ends_the_engine_without_a_reply():
    replies: list[dict] = []
    with pytest.raises(Fatal):
        serve((LOAD, b""), (transcribe(1), AUDIO), (transcribe(2), AUDIO), errors={1: Fatal("CUDA error")}, replies=replies)
    assert [r["type"] for r in replies] == ["hello", "progress", "progress", "loaded"]


def open_stream(stream_id: int) -> dict[str, Any]:
    return {**transcribe(stream_id), "type": "open"}


def stream(kind: str, stream_id: int) -> dict[str, Any]:
    return {"type": kind, "id": stream_id}


def test_a_stream_is_answered_at_close_as_a_transcription_of_everything_appended():
    model = StubModel(FakeClock())
    replies = serve(
        (LOAD, b""),
        (open_stream(7), b""),
        (stream("append", 7), AUDIO),
        (transcribe(8), AUDIO),
        (stream("append", 7), AUDIO),
        (stream("append", 7), payload_from_samples([0.0] * 320)),
        (stream("close", 7), b""),
        model=model,
    )
    assert [r["type"] for r in replies[4:]] == ["result", "result"]
    assert replies[-1]["id"] == 7
    assert replies[-1]["timings"] == {
        "decode_ms": 180,
        "prompt_tokens": 19,
        "bias_terms_used": 0,
        "prompt_truncated": False,
        "chunks": 0,
        "chunks_reused": 0,
    }
    assert "chunks" not in replies[-2]["timings"]
    assert model.transcribed == [1600, 3520]


def test_a_discarded_stream_is_gone():
    replies = serve(
        (LOAD, b""),
        (open_stream(7), b""),
        (stream("append", 7), AUDIO),
        (stream("discard", 7), b""),
        (stream("close", 7), b""),
        (stream("discard", 7), b""),
    )
    assert replies[4:] == [
        {"type": "error", "id": 7, "code": "bad_request", "message": "no open stream 7", "retryable": False}
    ]


def test_streams_are_checked_like_transcriptions():
    long_append = payload_from_samples([0.0] * (900 * 16_000 + 1))
    replies = serve(
        (open_stream(1), b""),
        (LOAD, b""),
        (stream("append", 2), AUDIO),
        (open_stream(3), b""),
        (open_stream(3), b""),
        (stream("append", 3), b"\x00\x00\x00"),
        (stream("append", 3), long_append),
        (stream("close", 3), b""),
        ({"type": "close"}, b""),
    )
    errors = [(r.get("id"), r["code"]) for r in replies if r["type"] == "error"]
    assert errors == [
        (1, "not_loaded"),
        (2, "bad_request"),
        (3, "bad_request"),
        (3, "bad_request"),
        (3, "bad_request"),
        (3, "bad_request"),
        (None, "bad_request"),
    ]


class StreamingStub(StubModel):
    """A stub with streams of its own, which records the background work it's asked to do."""

    def __init__(self) -> None:
        super().__init__(FakeClock())
        self.appended: dict[int, int] = {}
        self.background_calls = threading.Event()

    def open_stream(self, request: TranscribeRequest) -> None:
        self.appended[request.id] = 0

    def append_stream(self, stream_id: int, samples: array) -> None:
        self.appended[stream_id] += len(samples)

    def close_stream(self, stream_id: int) -> Transcription:
        transcription = self.transcribe(transcribe_request(stream_id), array("f", [0.0] * self.appended.pop(stream_id)))
        transcription.chunks, transcription.chunks_reused = 2, 1
        return transcription

    def discard_stream(self, stream_id: int) -> None:
        del self.appended[stream_id]

    def background(self) -> bool:
        self.background_calls.set()
        return False


def transcribe_request(request_id: int) -> TranscribeRequest:
    return parse_transcribe(transcribe(request_id))


def test_a_model_with_streams_gets_the_audio_and_works_between_requests():
    model = StreamingStub()
    read_fd, write_fd = os.pipe()
    out = io.BytesIO()
    engine = threading.Thread(target=Engine(model, out, clock=model.clock).serve, args=(os.fdopen(read_fd, "rb"),))
    engine.start()
    with os.fdopen(write_fd, "wb") as inp:
        write_frame(inp, LOAD)
        write_frame(inp, open_stream(7))
        write_frame(inp, stream("append", 7), AUDIO)
        assert model.background_calls.wait(timeout=5), "no background work while the stream was open"
        write_frame(inp, stream("append", 7), AUDIO)
        write_frame(inp, stream("close", 7))
    engine.join(timeout=5)
    out.seek(0)
    replies = []
    while (frame := read_frame(out, 1 << 20)) is not None:
        replies.append(parse_header(frame.header))
    assert replies[-1]["id"] == 7
    assert (replies[-1]["timings"]["chunks"], replies[-1]["timings"]["chunks_reused"]) == (2, 1)
    assert model.transcribed == [3200]


def test_the_engine_exits_cleanly_at_shutdown_while_its_input_stays_open():
    """The server keeps the engine's stdin open after `shutdown`, waiting for it to exit."""
    engine_dir = Path(__file__).resolve().parents[1]
    proc = subprocess.Popen(
        [sys.executable, "-m", "psst_engine_faster_whisper"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env={**os.environ, "PSST_ENGINE_PROTOCOL": "1", "PYTHONPATH": str(engine_dir)},
    )
    try:
        assert parse_header(read_frame(proc.stdout, 1 << 20).header)["type"] == "hello"
        write_frame(proc.stdin, {"type": "shutdown"})
        assert proc.wait(timeout=10) == 0, proc.stderr.read().decode()
    finally:
        proc.kill()
        proc.stdin.close()
