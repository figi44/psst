from __future__ import annotations

import os
import sys

from .protocol import PROTOCOL_ENV, PROTOCOL_VERSION, CorruptStream, claim_stdout


def main() -> int:
    if os.environ.get(PROTOCOL_ENV) != str(PROTOCOL_VERSION):
        print(f"psst-engine-faster-whisper: {PROTOCOL_ENV}={PROTOCOL_VERSION} is not set", file=sys.stderr)
        return 2
    # Before importing anything that might print.
    out = claim_stdout()

    from .engine import Engine, Fatal
    from .model import FasterWhisperModel

    try:
        Engine(FasterWhisperModel(), out).serve(sys.stdin.buffer)
    except CorruptStream as err:
        print(f"psst-engine-faster-whisper: corrupt input: {err}", file=sys.stderr)
        return 3
    except Fatal as err:
        print(f"psst-engine-faster-whisper: exiting: {err}", file=sys.stderr, flush=True)
        # Skip interpreter shutdown: destructors may hang on a broken CUDA context.
        os._exit(4)
    return 0


if __name__ == "__main__":
    sys.exit(main())
