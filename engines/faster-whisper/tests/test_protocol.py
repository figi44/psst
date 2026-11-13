"""The frame codec and the stdout redirect."""

from __future__ import annotations

import io
import os
import subprocess
import sys
from pathlib import Path

import pytest

from psst_engine_faster_whisper.protocol import (
    MAX_HEADER_BYTES,
    PREFIX,
    BadHeader,
    CorruptStream,
    Frame,
    max_payload_bytes,
    parse_header,
    payload_from_samples,
    read_frame,
    samples_from_payload,
    write_frame,
)

ENGINE_DIR = Path(__file__).resolve().parent.parent


def frames(data: bytes, max_payload: int = 1 << 20) -> list[Frame]:
    inp = io.BytesIO(data)
    out = []
    while (frame := read_frame(inp, max_payload)) is not None:
        out.append(frame)
    return out


def test_frames_round_trip_and_end_cleanly():
    buf = io.BytesIO()
    write_frame(buf, {"type": "shutdown"})
    write_frame(buf, {"type": "x", "text": "Dvořák"}, b"\x01\x02\x03\x04")
    data = buf.getvalue()

    assert data[:8] == PREFIX.pack(19, 0)
    assert frames(data) == [
        Frame(b'{"type":"shutdown"}', b""),
        Frame('{"type":"x","text":"Dvořák"}'.encode(), b"\x01\x02\x03\x04"),
    ]


def test_a_stream_ending_inside_a_frame_is_corrupt():
    buf = io.BytesIO()
    write_frame(buf, {"type": "x"}, b"\x00" * 8)
    data = buf.getvalue()
    for cut in range(1, len(data)):
        with pytest.raises(CorruptStream):
            frames(data[:cut])


def test_frames_over_the_limits_are_corrupt():
    with pytest.raises(CorruptStream):
        frames(PREFIX.pack(MAX_HEADER_BYTES + 1, 0))
    limit = max_payload_bytes(10)
    assert limit == 10 * 64_000 + (1 << 20)
    assert frames(PREFIX.pack(0, limit) + b"\x00" * limit, limit)[0].payload == b"\x00" * limit
    with pytest.raises(CorruptStream):
        frames(PREFIX.pack(0, limit + 1), limit)


@pytest.mark.parametrize("header", [b"not json", b"\xff", b"[1]", b'{"no":"type"}', b'{"type":3}'])
def test_headers_must_be_objects_with_a_string_type(header):
    with pytest.raises(BadHeader):
        parse_header(header)


def test_audio_payloads_are_f32_little_endian():
    payload = payload_from_samples([0.0, 0.5, -1.0])
    assert payload == b"\x00\x00\x00\x00\x00\x00\x00?\x00\x00\x80\xbf"
    assert list(samples_from_payload(payload)) == [0.0, 0.5, -1.0]
    with pytest.raises(ValueError):
        samples_from_payload(payload[:-1])


def run_python(code: str, **kwargs) -> subprocess.CompletedProcess:
    env = {**os.environ, "PYTHONPATH": str(ENGINE_DIR)}
    env.update(kwargs.pop("env", {}))
    return subprocess.run([sys.executable, "-c", code], capture_output=True, env=env, timeout=30, **kwargs)


def test_claim_stdout_keeps_prints_off_the_protocol_stream():
    proc = run_python(
        "from psst_engine_faster_whisper.protocol import claim_stdout, write_frame\n"
        "out = claim_stdout()\n"
        "print('library noise')\n"
        "import os; os.write(1, b'raw noise\\n')\n"
        "write_frame(out, {'type': 'hello'})\n"
    )
    assert proc.returncode == 0, proc.stderr
    assert frames(proc.stdout) == [Frame(b'{"type":"hello"}', b"")]
    assert b"library noise" in proc.stderr
    assert b"raw noise" in proc.stderr


def test_entry_point_needs_the_protocol_variable():
    proc = run_python("from psst_engine_faster_whisper.__main__ import main; raise SystemExit(main())")
    assert proc.returncode == 2
    assert proc.stdout == b""


def test_entry_point_says_hello_and_stops_on_shutdown():
    request = io.BytesIO()
    write_frame(request, {"type": "shutdown"})
    proc = run_python(
        "from psst_engine_faster_whisper.__main__ import main; raise SystemExit(main())",
        input=request.getvalue(),
        env={"PSST_ENGINE_PROTOCOL": "1"},
    )
    assert proc.returncode == 0, proc.stderr
    [hello] = frames(proc.stdout)
    assert parse_header(hello.header)["type"] == "hello"
