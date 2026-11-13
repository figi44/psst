"""Smoke test on the GPU: the real engine, started as the server starts it, on CUDA.

Marked ``gpu``, so plain ``pytest`` (and CI) leaves it out. Run it in ``nix develop .#gpu``, with
the GPU to itself (inside ``scripts/gpu-session.sh``, or under its lock):

    flock ~/.cache/psst/gpu.lock env HF_HOME=~/.cache/psst/hf PSST_ENGINE_COMMAND=psst-engine-dev pytest -m gpu -s

``PSST_ENGINE_COMMAND`` is any command that runs this checkout's engine with faster-whisper on
CUDA; ``psst-engine-dev`` does.

Models missing from ``HF_HOME`` are downloaded. ``PSST_GPU_MODELS`` picks the models (default
``tiny,large-v3-turbo``). The measured load and transcribe times are printed, and appended as
JSON lines to ``PSST_GPU_REPORT`` when it's set.
"""

from __future__ import annotations

import json
import os
import time

import pytest

from real_engine import KEY_WORDS, EngineProcess, assert_covers_long_speech, long_speech, payload, speech, words

pytestmark = pytest.mark.gpu

MODELS = os.environ.get("PSST_GPU_MODELS", "tiny,large-v3-turbo").split(",")
STYLE = "I'm dictating notes about the NixOS config and the psst engine."
TERMS = ["Hyprland", "psst", "t3code", "NixOS", "nixpkgs", "Wayland", "niri", "PipeWire", "CTranslate2", "Dvořák"]


@pytest.mark.parametrize("model", MODELS)
def test_loads_and_transcribes_on_cuda(model):
    report: dict = {"model": model}
    short, long = payload(speech()), payload(long_speech())
    with EngineProcess() as engine:
        assert engine.recv(timeout=30)["type"] == "hello"

        started = time.monotonic()
        loaded, progress = engine.load(model, "cuda", "int8_float16", timeout=600)
        report["load_wall_ms"] = round((time.monotonic() - started) * 1000)
        report["load_ms"] = loaded["load_ms"]
        report["downloaded"] = any(p["phase"] == "download" for p in progress)
        if report["downloaded"]:
            assert any("bytes" in p for p in progress), "a download reports its bytes"
        assert len(loaded["revision"]) == 40
        report["revision"] = loaded["revision"]

        def run(name: str, request_id: int, samples: bytes, **kwargs) -> dict:
            started = time.monotonic()
            result = engine.transcribe(request_id, samples, timeout=120, **kwargs)
            assert result["type"] == "result", result
            report[name] = {
                "wall_ms": round((time.monotonic() - started) * 1000),
                "decode_ms": result["timings"]["decode_ms"],
                "path": result["trace"]["path"],
                "prompt_tokens": result["timings"]["prompt_tokens"],
            }
            return result

        # The first request also loads the VAD model and warms CUDA up.
        first = run("first_sequential", 1, short, style=STYLE, terms=TERMS)
        assert KEY_WORDS <= set(words(first))
        for request_id in (2, 3):
            assert KEY_WORDS <= set(words(run(f"sequential_{request_id}", request_id, short, style=STYLE, terms=TERMS)))
        hotwords = run("hotwords", 4, short, style=STYLE, terms=TERMS, terms_as="hotwords")
        assert hotwords["trace"]["options"]["condition_on_previous_text"] is False
        assert KEY_WORDS <= set(words(run("batched", 5, short, style=STYLE, terms=TERMS, path="batched")))
        no_timestamps = run("no_word_timestamps", 6, short, word_timestamps=False)
        assert "words" not in no_timestamps["trace"]["result"]["segments"][0]

        full = run("full_budget", 7, short, style=STYLE, terms=[f"{t}{i}" for i in range(40) for t in TERMS])
        assert 200 <= full["timings"]["prompt_tokens"] <= 223

        assert_covers_long_speech(run("long_auto", 8, long, style=STYLE, terms=TERMS, path="auto"))

    print(f"\ngpu smoke {model}: {json.dumps(report)}")
    if path := os.environ.get("PSST_GPU_REPORT"):
        with open(path, "a") as out:
            out.write(json.dumps(report) + "\n")
