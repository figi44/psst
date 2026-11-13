"""The real engine as the server runs it: a child process speaking the protocol, and speech for it.

``data/speech.wav`` (16 kHz mono, 3.5 s) was made with
``espeak-ng -v en-us -s 150 -w raw.wav "Please push the fix to the main branch before lunch."``
and ``sox raw.wav -r 16000 -c 1 -b 16 speech.wav``.
"""

from __future__ import annotations

import os
import queue
import re
import subprocess
import sys
import threading
import wave
from array import array
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

from psst_engine_faster_whisper.protocol import parse_header, payload_from_samples, read_frame, write_frame

ENGINE_DIR = Path(__file__).resolve().parent.parent
SPEECH = Path(__file__).resolve().parent / "data" / "speech.wav"
KEY_WORDS = {"push", "fix", "main", "branch", "before"}
"""Words every model gets. Not "lunch": with a technical bias, models hear "launch"."""
SAMPLE_RATE = 16_000


def speech() -> list[float]:
    with wave.open(str(SPEECH)) as wav:
        assert (wav.getframerate(), wav.getnchannels(), wav.getsampwidth()) == (SAMPLE_RATE, 1, 2)
        pcm = array("h")
        pcm.frombytes(wav.readframes(wav.getnframes()))
    if sys.byteorder == "big":
        pcm.byteswap()
    return [sample / 32768 for sample in pcm]


def payload(samples: Sequence[float]) -> bytes:
    return payload_from_samples(list(samples))


def long_speech(repeats: int = 10, gap_s: float = 1.0) -> list[float]:
    """The speech ``repeats`` times with ``gap_s`` of silence after each: 45 s by default."""
    once = speech() + [0.0] * round(gap_s * SAMPLE_RATE)
    return once * repeats


def words(result: dict[str, Any], after_ms: int = 0) -> list[str]:
    """The words of the segments starting at ``after_ms`` or later, lowercase, without punctuation."""
    segments = result["trace"]["result"]["segments"]
    text = " ".join(segment["text"] for segment in segments if segment["start_ms"] >= after_ms)
    return re.findall(r"[a-z0-9']+", text.lower())


def assert_covers_long_speech(result: dict[str, Any]) -> None:
    """The engine decoded both 30 s chunks of ``long_speech()``, in the recording's time.

    Not a word count: on a chunk that repeats one sentence, Whisper may write it more or fewer
    times (turbo writes it once).
    """
    assert result["trace"]["path"] == "batched"
    starts = [segment["start_ms"] for segment in result["trace"]["result"]["segments"]]
    assert min(starts) < 5_000 and max(starts) > 30_000, starts
    assert {"push", "fix", "branch"} <= set(words(result, after_ms=30_000))


class EngineProcess:
    """The engine started as the server starts it, from this checkout.

    ``command`` defaults to ``PSST_ENGINE_COMMAND`` (split on spaces), then to this Python running
    ``-m psst_engine_faster_whisper``. Its stderr goes to the test's.
    """

    def __init__(self, command: Sequence[str] | None = None, env: Mapping[str, str] | None = None) -> None:
        if command is None:
            configured = os.environ.get("PSST_ENGINE_COMMAND")
            command = configured.split() if configured else [sys.executable, "-m", "psst_engine_faster_whisper"]
        python_path = os.pathsep.join(filter(None, [str(ENGINE_DIR), os.environ.get("PYTHONPATH")]))
        self.proc = subprocess.Popen(
            list(command),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            env={**os.environ, **(env or {}), "PSST_ENGINE_PROTOCOL": "1", "PYTHONPATH": python_path},
        )
        self._replies: queue.Queue[dict[str, Any] | None] = queue.Queue()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self) -> None:
        try:
            while (frame := read_frame(self.proc.stdout, 1 << 24)) is not None:
                self._replies.put(parse_header(frame.header))
        finally:
            self._replies.put(None)

    def send(self, message: dict[str, Any], data: bytes = b"") -> None:
        write_frame(self.proc.stdin, message, data)

    def recv(self, timeout: float = 60) -> dict[str, Any]:
        try:
            reply = self._replies.get(timeout=timeout)
        except queue.Empty:
            raise TimeoutError(f"no reply from the engine in {timeout} s") from None
        if reply is None:
            raise EOFError(f"the engine exited with status {self.proc.wait(10)}")
        return reply

    def load(self, model: str, device: str, compute_type: str, timeout: float = 60) -> tuple[dict, list[dict]]:
        """Loads and returns ``loaded`` and the ``progress`` before it."""
        self.send({"type": "load", "model": model, "device": device, "compute_type": compute_type})
        progress = []
        while (reply := self.recv(timeout))["type"] == "progress":
            progress.append(reply)
        assert reply["type"] == "loaded", reply
        return reply, progress

    def transcribe(
        self, request_id: int, samples: bytes, *, style: str = "", terms: Sequence[str] = (), timeout: float = 60, **options: Any
    ) -> dict[str, Any]:
        self.send(
            {
                "type": "transcribe",
                "id": request_id,
                "language": "en",
                "bias": {"style": style, "terms": list(terms)},
                "options": options,
            },
            samples,
        )
        return self.recv(timeout)

    def close(self) -> int:
        if self.proc.poll() is None:
            try:
                self.send({"type": "shutdown"})
                self.proc.stdin.close()
                return self.proc.wait(10)
            except (OSError, subprocess.TimeoutExpired):
                self.proc.kill()
        return self.proc.wait()

    def __enter__(self) -> EngineProcess:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()
