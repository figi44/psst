"""A deterministic model and clock for driving the engine without faster-whisper or a GPU."""

from __future__ import annotations

from array import array

from psst_engine_faster_whisper.engine import (
    LoadRequest,
    OutOfMemory,
    ProgressFn,
    TranscribeRequest,
    Transcription,
)

REVISION = "0123456789abcdef0123456789abcdef01234567"


class FakeClock:
    """A monotonic clock that only moves when told to."""

    def __init__(self) -> None:
        self.now = 100.0

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


class StubModel:
    """Loads in 2.4 s with download progress, transcribes in 180 ms, runs out of GPU memory for the
    request ids in ``oom_ids``, and raises ``errors[id]`` for the ids in ``errors``."""

    def __init__(
        self, clock: FakeClock, oom_ids: frozenset[int] = frozenset(), errors: dict[int, Exception] | None = None
    ) -> None:
        self.clock = clock
        self.oom_ids = oom_ids
        self.errors = errors or {}

    def load(self, request: LoadRequest, progress: ProgressFn) -> str:
        progress("download", 524_288, 1_048_576)
        progress("load", None, None)
        self.clock.advance(2.4)
        return request.revision or REVISION

    def transcribe(self, request: TranscribeRequest, samples: array) -> Transcription:
        if request.id in self.oom_ids:
            raise OutOfMemory("CUDA out of memory (stub)")
        if request.id in self.errors:
            raise self.errors[request.id]
        self.clock.advance(0.18)
        terms = ", ".join(request.bias.terms)
        prompt = " ".join(part for part in (request.bias.style, terms and terms + ".") if part)
        return Transcription(
            result={
                "language": request.language,
                "speech_ms": 5200,
                "segments": [
                    {
                        "text": " Push the fix to psst.",
                        "start_ms": 0,
                        "end_ms": 2100,
                        "no_speech_prob": 0.01,
                        "words": [
                            {"text": " Push", "start_ms": 0, "end_ms": 300, "prob": 0.98},
                            {"text": " the", "start_ms": 300, "end_ms": 450, "prob": 0.99},
                            {"text": " fix", "start_ms": 450, "end_ms": 800, "prob": 0.97},
                            {"text": " to", "start_ms": 800, "end_ms": 950, "prob": 0.99},
                            {"text": " psst.", "start_ms": 950, "end_ms": 2100, "prob": 0.61},
                        ],
                    }
                ],
            },
            path="sequential",
            options=request.options,
            prompt=prompt,
            prompt_tokens=19,
            bias_terms_used=len(request.bias.terms),
        )
