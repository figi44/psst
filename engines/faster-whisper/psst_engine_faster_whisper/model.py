"""faster-whisper behind the engine's ``Model`` interface."""

from __future__ import annotations

from array import array

from .engine import LoadFailed, LoadRequest, ProgressFn, TranscribeRequest, Transcription


class FasterWhisperModel:
    """Placeholder until the engine work package lands real inference (plan 5.3, 6.3).

    Every ``load`` fails with ``load_failed``, so a server pointed at this engine reports the
    model as failed instead of hanging.
    """

    def load(self, request: LoadRequest, progress: ProgressFn) -> str:
        raise LoadFailed("the faster-whisper model is not implemented yet")

    def transcribe(self, request: TranscribeRequest, samples: array) -> Transcription:
        raise NotImplementedError("the faster-whisper model is not implemented yet")
