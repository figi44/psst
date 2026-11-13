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
        assert hello["capabilities"] == {"bias": True, "max_audio_s": 900, "stream": True}
        loaded, progress = engine.load("tiny", "cpu", "int8")
        assert progress == [{"type": "progress", "phase": "load"}]
        assert len(loaded["revision"]) == 40
        yield engine


@pytest.mark.parametrize("terms_as", ["prompt", "hotwords"])
def test_transcribes_speech_with_the_bias_counted_as_faster_whisper_encodes_it(engine, tokenizer, terms_as):
    result = engine.transcribe(1, payload(speech()), style=STYLE, terms=TERMS, terms_as=terms_as)
    assert result["type"] == "result", result
    assert KEY_WORDS <= set(words(result))
    trace, timings = result["trace"], result["timings"]
    assert (trace["path"], trace["options"]["terms_as"], trace["options"]["batch_size"]) == ("batched", terms_as, 8)
    assert trace["prompt"] == "I'm updating the NixOS config. Hyprland, psst, t3code."
    expected = bias.render(STYLE, TERMS, terms_as=terms_as, encode=tokenizer)
    assert (timings["prompt_tokens"], timings["bias_terms_used"]) == (expected.tokens, 3)
    segment = trace["result"]["segments"][0]
    assert [w["text"] for w in segment["words"]][:2] == [" Please", " push"]
    assert 0 <= segment["no_speech_prob"] <= 1
    assert 3000 <= trace["result"]["speech_ms"] <= 3600


def test_a_long_recording_is_decoded_in_chunks(engine):
    samples = long_speech()
    result = engine.transcribe(2, payload(samples), word_timestamps=False)
    assert_covers_long_speech(result)
    assert "words" not in result["trace"]["result"]["segments"][0]
    # VAD kept the speech, with its padding, and dropped most of the silence.
    assert 35_000 <= result["trace"]["result"]["speech_ms"] < len(samples) / 16


def test_silence_has_no_segments(engine):
    result = engine.transcribe(3, payload([0.0] * 32_000))
    assert result["type"] == "result", result
    assert result["trace"]["result"]["segments"] == []
    assert result["trace"]["result"]["speech_ms"] == 0


def without_decode_time(result: dict) -> dict:
    return {**result, "id": None, "timings": {k: v for k, v in result["timings"].items() if k not in ("decode_ms", "chunks", "chunks_reused")}}


def test_a_stream_that_ends_in_a_pause_is_answered_from_the_work_done_ahead(engine):
    """The answer is the whole recording's, and the chunk was decoded during the pause."""
    second = 16_000
    samples = speech() + [0.0] * second
    whole = engine.transcribe(4, payload(samples), style=STYLE, terms=TERMS)
    # The pause is in the audio before the wait, so the engine decodes ahead while it waits.
    streamed = engine.stream(
        5, [payload(samples[: -second // 5]), payload(samples[-second // 5 :])], wait_s=3, style=STYLE, terms=TERMS
    )
    assert streamed["type"] == "result", streamed
    assert (streamed["timings"]["chunks"], streamed["timings"]["chunks_reused"]) == (1, 1)
    assert without_decode_time(streamed) == without_decode_time(whole)


def test_a_stream_still_speaking_at_close_is_decoded_then(engine):
    samples = speech()
    whole = engine.transcribe(6, payload(samples), style=STYLE, terms=TERMS)
    streamed = engine.stream(7, [payload(samples[:16_000]), payload(samples[16_000:])], wait_s=1, style=STYLE, terms=TERMS)
    assert (streamed["timings"]["chunks"], streamed["timings"]["chunks_reused"]) == (1, 0)
    assert without_decode_time(streamed) == without_decode_time(whole)


def test_a_long_stream_reuses_the_chunks_finished_while_recording(engine):
    """Over 30 s, the chunks decoded ahead were decoded alone rather than in one batch, which may
    move word timings and probabilities slightly, as another batch size does; the text is the
    same."""
    samples = long_speech()
    whole = engine.transcribe(8, payload(samples))
    piece = 5 * 16_000
    pieces = [payload(samples[i : i + piece]) for i in range(0, len(samples), piece)]
    streamed = engine.stream(9, pieces, wait_s=1)
    assert streamed["timings"]["chunks"] == 2
    assert streamed["timings"]["chunks_reused"] >= 1
    assert_covers_long_speech(streamed)
    texts = [[(s["text"], s["start_ms"]) for s in r["trace"]["result"]["segments"]] for r in (streamed, whole)]
    assert texts[0] == texts[1]
