"""The request loop, driven through its input and output streams with a stub model."""

from __future__ import annotations

import io
from typing import Any

import pytest

from psst_engine_faster_whisper.engine import Engine, Fatal
from psst_engine_faster_whisper.protocol import parse_header, payload_from_samples, read_frame, write_frame
from stub_model import FakeClock, StubModel

LOAD = {"type": "load", "model": "large-v3-turbo", "device": "cpu", "compute_type": "int8"}


def transcribe(request_id: int) -> dict[str, Any]:
    return {
        "type": "transcribe",
        "id": request_id,
        "language": "en",
        "bias": {"style": "", "terms": []},
        "options": {"path": "sequential"},
    }


AUDIO = payload_from_samples([0.0] * 1600)


def serve(
    *requests: tuple[dict[str, Any] | bytes, bytes],
    oom_ids: frozenset[int] = frozenset(),
    errors: dict[int, Exception] | None = None,
    replies: list[dict] | None = None,
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
    try:
        Engine(StubModel(clock, oom_ids, errors), out, clock=clock).serve(inp)
    finally:
        out.seek(0)
        while (frame := read_frame(out, 1 << 20)) is not None:
            replies.append(parse_header(frame.header))
    return replies


def test_says_hello_first_then_loads_and_transcribes():
    replies = serve((LOAD, b""), (transcribe(1), AUDIO))
    assert [r["type"] for r in replies] == ["hello", "progress", "progress", "loaded", "result"]
    assert replies[0]["capabilities"] == {"bias": True, "max_audio_s": 900}
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
