"""Golden frames, the engine contract checked in both directions without a GPU.

- ``golden/from-engine/``: written here, by the real request loop with a stub model. A Rust test in
  ``psst-engine-proto`` decodes them into its typed messages.
- ``golden/to-engine/``: written by a Rust test in ``psst-engine-proto``; decoded here.

Tests regenerate their files. CI fails if the regenerated files differ from the committed ones,
so a contract change shows up as a diff of these files in the pull request.
"""

from __future__ import annotations

import io
from pathlib import Path

from psst_engine_faster_whisper.engine import Engine, parse_load, parse_transcribe
from psst_engine_faster_whisper.protocol import (
    max_payload_bytes,
    parse_header,
    payload_from_samples,
    read_frame,
    samples_from_payload,
    write_frame,
)
from stub_model import REVISION, FakeClock, StubModel

GOLDEN = Path(__file__).resolve().parent / "golden"


def read_golden(name: str):
    with open(GOLDEN / "to-engine" / name, "rb") as f:
        frame = read_frame(f, max_payload_bytes(900))
        assert frame is not None
        assert f.read() == b"", f"{name} holds more than one frame"
    return frame


def test_decodes_server_frames():
    load = read_golden("load.bin")
    assert parse_load(parse_header(load.header)).__dict__ == {
        "model": "large-v3-turbo",
        "revision": REVISION,
        "device": "cuda",
        "compute_type": "int8_float16",
    }
    assert load.payload == b""

    transcribe = read_golden("transcribe.bin")
    request = parse_transcribe(parse_header(transcribe.header))
    assert request.id == 42
    assert request.language == "en"
    assert request.bias.style == "I'm updating the NixOS config."
    assert request.bias.terms == ("Hyprland", "psst", "t3code", "Dvořák")
    assert request.options == {
        "path": "batched",
        "beam_size": 5,
        "word_timestamps": True,
        "vad_filter": True,
        "terms_as": "prompt",
        "batch_size": 8,
    }
    samples = samples_from_payload(transcribe.payload)
    assert len(samples) == 1600
    assert list(samples[:8]) == [0.0, 0.25, -0.25, 0.5, -0.5, 1.0, -1.0, 0.125]

    shutdown = read_golden("shutdown.bin")
    assert parse_header(shutdown.header) == {"type": "shutdown"}


def golden_name(message: dict) -> str:
    detail = message.get("phase") or message.get("code")
    return f"{message['type']}-{detail}.bin".replace("_", "-") if detail else f"{message['type']}.bin"


def test_writes_engine_frames():
    requests = io.BytesIO()
    write_frame(requests, {"type": "load", "model": "large-v3-turbo", "device": "cuda", "compute_type": "int8_float16"})
    write_frame(
        requests,
        {
            "type": "transcribe",
            "id": 42,
            "language": "en",
            "bias": {"style": "I'm updating the NixOS config.", "terms": ["Hyprland", "psst", "t3code"]},
            "options": {
                "path": "batched",
                "beam_size": 5,
                "word_timestamps": True,
                "vad_filter": True,
                "terms_as": "prompt",
                "batch_size": 8,
            },
        },
        payload_from_samples([0.0] * 96_000),
    )
    write_frame(requests, {"type": "frobnicate"})
    write_frame(
        requests,
        {"type": "transcribe", "id": 43, "language": "en", "bias": {"style": "", "terms": []}, "options": {}},
        payload_from_samples([0.0] * 1600),
    )
    write_frame(requests, {"type": "shutdown"})
    requests.seek(0)

    replies = io.BytesIO()
    clock = FakeClock()
    Engine(StubModel(clock, oom_ids=frozenset({43})), replies, clock=clock).serve(requests)
    replies.seek(0)

    out_dir = GOLDEN / "from-engine"
    out_dir.mkdir(parents=True, exist_ok=True)
    for stale in out_dir.glob("*.bin"):
        stale.unlink()
    names = []
    while (frame := read_frame(replies, 1 << 20)) is not None:
        name = golden_name(parse_header(frame.header))
        assert name not in names, f"two frames would be written to {name}"
        names.append(name)
        (out_dir / name).write_bytes(frame_bytes(frame))

    assert names == [
        "hello.bin",
        "progress-download.bin",
        "progress-load.bin",
        "loaded.bin",
        "result.bin",
        "error-unsupported.bin",
        "error-oom.bin",
    ]


def frame_bytes(frame) -> bytes:
    return len(frame.header).to_bytes(4, "little") + len(frame.payload).to_bytes(4, "little") + frame.header + frame.payload
