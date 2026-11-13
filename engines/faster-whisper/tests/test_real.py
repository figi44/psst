"""The real engine on the CPU with faster-whisper's ``tiny``, offline.

Skipped unless faster-whisper is installed and ``tiny`` is in the local Hugging Face cache
(``HF_HOME``); loading it once on the GPU, or with this engine, puts it there.
"""

from __future__ import annotations

import os

import pytest

from real_engine import KEY_WORDS, EngineProcess, assert_covers_long_speech, long_speech, payload, speech, words

pytest.importorskip("faster_whisper")

from psst_engine_faster_whisper import bias, hub  # noqa: E402

TINY = "Systran/faster-whisper-tiny"
STYLE = "I'm updating the NixOS config."
TERMS = ["Hyprland", "psst", "t3code"]


@pytest.fixture(scope="module")
def tokenizer():
    try:
        snapshot = hub.snapshot(TINY, None, local_files_only=True)
    except Exception as err:
        pytest.skip(f"{TINY} is not in the local Hugging Face cache: {err}")
    from tokenizers import Tokenizer

    whisper = Tokenizer.from_file(os.path.join(snapshot, "tokenizer.json"))
    return lambda text: whisper.encode(text, add_special_tokens=False).ids


@pytest.fixture(scope="module")
def engine(tokenizer):
    with EngineProcess(env={"HF_HUB_OFFLINE": "1"}) as engine:
        hello = engine.recv()
        assert hello["capabilities"] == {"bias": True, "max_audio_s": 900}
        loaded, progress = engine.load("tiny", "cpu", "int8")
        assert progress == [{"type": "progress", "phase": "load"}]
        assert len(loaded["revision"]) == 40
        yield engine


@pytest.mark.parametrize("terms_as", ["prompt", "hotwords"])
@pytest.mark.parametrize("path", ["sequential", "batched"])
def test_transcribes_speech_with_the_bias_counted_as_faster_whisper_encodes_it(engine, tokenizer, path, terms_as):
    result = engine.transcribe(1, payload(speech()), style=STYLE, terms=TERMS, path=path, terms_as=terms_as)
    assert result["type"] == "result", result
    assert KEY_WORDS <= set(words(result))
    trace, timings = result["trace"], result["timings"]
    assert (trace["path"], trace["options"]["terms_as"]) == (path, terms_as)
    assert trace["prompt"] == "I'm updating the NixOS config. Hyprland, psst, t3code."
    expected = bias.render(STYLE, TERMS, terms_as=terms_as, path=path, encode=tokenizer)
    assert (timings["prompt_tokens"], timings["bias_terms_used"]) == (expected.tokens, 3)
    segment = trace["result"]["segments"][0]
    assert [w["text"] for w in segment["words"]][:2] == [" Please", " push"]
    assert 0 <= segment["no_speech_prob"] <= 1
    assert 3000 <= trace["result"]["speech_ms"] <= 3600


def test_a_long_recording_takes_the_batched_path_on_auto(engine):
    samples = long_speech()
    result = engine.transcribe(2, payload(samples), path="auto", word_timestamps=False)
    assert_covers_long_speech(result)
    assert "words" not in result["trace"]["result"]["segments"][0]
    # VAD kept the speech, with its padding, and dropped most of the silence.
    assert 35_000 <= result["trace"]["result"]["speech_ms"] < len(samples) / 16


@pytest.mark.parametrize("path", ["sequential", "batched"])
def test_silence_has_no_segments(engine, path):
    result = engine.transcribe(3, payload([0.0] * 32_000), path=path)
    assert result["type"] == "result", result
    assert result["trace"]["result"]["segments"] == []
    assert result["trace"]["result"]["speech_ms"] == 0
