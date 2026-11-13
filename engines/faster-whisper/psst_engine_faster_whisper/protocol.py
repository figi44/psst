from __future__ import annotations

import json
import os
import struct
import sys
from array import array
from dataclasses import dataclass
from typing import Any, BinaryIO

PROTOCOL_VERSION = 1
PROTOCOL_ENV = "PSST_ENGINE_PROTOCOL"

PREFIX = struct.Struct("<II")
MAX_HEADER_BYTES = 1 << 20
PAYLOAD_SLACK_BYTES = 1 << 20
AUDIO_BYTES_PER_SECOND = 64_000


class CorruptStream(Exception):
    pass


class BadHeader(Exception):
    pass


@dataclass(frozen=True)
class Frame:
    header: bytes
    payload: bytes


def max_payload_bytes(max_audio_s: int) -> int:
    return max_audio_s * AUDIO_BYTES_PER_SECOND + PAYLOAD_SLACK_BYTES


def claim_stdout() -> BinaryIO:
    """Points fd 1 at stderr, so a library that prints can't corrupt the protocol stream."""
    sys.stdout.flush()
    protocol_fd = os.dup(1)
    os.dup2(2, 1)
    return os.fdopen(protocol_fd, "wb")


def _read_exact(inp: BinaryIO, n: int) -> bytes:
    chunks = []
    remaining = n
    while remaining:
        chunk = inp.read(remaining)
        if not chunk:
            break
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def read_frame(inp: BinaryIO, max_payload: int) -> Frame | None:
    prefix = _read_exact(inp, PREFIX.size)
    if not prefix:
        return None
    if len(prefix) < PREFIX.size:
        raise CorruptStream("the stream ended inside a frame")
    header_len, payload_len = PREFIX.unpack(prefix)
    if header_len > MAX_HEADER_BYTES:
        raise CorruptStream(f"frame header of {header_len} bytes is over the {MAX_HEADER_BYTES}-byte limit")
    if payload_len > max_payload:
        raise CorruptStream(f"frame payload of {payload_len} bytes is over the {max_payload}-byte limit")
    header = _read_exact(inp, header_len)
    payload = _read_exact(inp, payload_len)
    if len(header) < header_len or len(payload) < payload_len:
        raise CorruptStream("the stream ended inside a frame")
    return Frame(header, payload)


def parse_header(header: bytes) -> dict[str, Any]:
    try:
        message = json.loads(header.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as err:
        raise BadHeader(f"header is not UTF-8 JSON: {err}") from err
    if not isinstance(message, dict) or not isinstance(message.get("type"), str):
        raise BadHeader("header must be a JSON object with a string `type`")
    return message


def encode_header(message: dict[str, Any]) -> bytes:
    return json.dumps(message, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def write_frame(out: BinaryIO, message: dict[str, Any], payload: bytes = b"") -> None:
    header = encode_header(message)
    if len(header) > MAX_HEADER_BYTES:
        raise ValueError(f"frame header of {len(header)} bytes is over the {MAX_HEADER_BYTES}-byte limit")
    out.write(PREFIX.pack(len(header), len(payload)) + header)
    if payload:
        out.write(payload)
    out.flush()


def samples_from_payload(payload: bytes) -> array:
    if len(payload) % 4:
        raise ValueError("audio payload length is not a multiple of 4")
    samples = array("f")
    samples.frombytes(payload)
    if sys.byteorder == "big":
        samples.byteswap()
    return samples


def payload_from_samples(samples: list[float] | array) -> bytes:
    return struct.pack(f"<{len(samples)}f", *samples)
