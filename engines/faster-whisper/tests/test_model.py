"""The faster-whisper engine, driven through the protocol, with faster-whisper and the Hugging Face
cache faked where the engine calls them."""

from __future__ import annotations

import io
from typing import Any

import pytest

from fake_tokenizer import fake_encode
from fake_whisper import INVALID_DEVICE, OUT_OF_MEMORY, REMOTE, FakeWhisper, default_segment, snapshot_dir
from psst_engine_faster_whisper.engine import Engine, Fatal
from psst_engine_faster_whisper.model import FasterWhisperModel
from psst_engine_faster_whisper.protocol import parse_header, read_frame, write_frame

TURBO = "mobiuslabsgmbh/faster-whisper-large-v3-turbo"
TURBO_DIR = snapshot_dir(TURBO, REMOTE[TURBO])
LOAD = {"type": "load", "model": "large-v3-turbo", "device": "cuda", "compute_type": "int8_float16"}
STYLE = "I'm updating the NixOS config."
TERMS = ["Hyprland", "psst", "t3code"]
PROMPT = "I'm updating the NixOS config. Hyprland, psst, t3code."
OPTIONS = {"path": "batched", "beam_size": 5, "word_timestamps": True, "vad_filter": True, "terms_as": "prompt", "batch_size": 8}
"""The defaults in ``psst-config``, as every trace records them."""


def transcribe(request_id: int = 1, *, seconds: float = 3.0, style=STYLE, terms=TERMS, language="en", **options):
    header = {
        "type": "transcribe",
        "id": request_id,
        "language": language,
        "bias": {"style": style, "terms": list(terms)},
        "options": options,
    }
    return header, bytes(4 * round(seconds * 16_000))


def run(fake: FakeWhisper, *requests: dict | tuple[dict, bytes]) -> tuple[list[dict[str, Any]], Fatal | None]:
    """Sends the requests to an engine over faster-whisper; returns its replies, and how it ended."""
    inp = io.BytesIO()
    for request in requests:
        header, payload = request if isinstance(request, tuple) else (request, b"")
        write_frame(inp, header, payload)
    inp.seek(0)
    out = io.BytesIO()
    fatal = None
    try:
        Engine(FasterWhisperModel(fake.backend()), out).serve(inp)
    except Fatal as err:
        fatal = err
    out.seek(0)
    replies = []
    while (frame := read_frame(out, 1 << 20)) is not None:
        replies.append(parse_header(frame.header))
    return replies, fatal


def serve(fake: FakeWhisper, *requests: dict | tuple[dict, bytes]) -> list[dict[str, Any]]:
    """The engine's replies after hello; it must not end on a fatal error."""
    replies, fatal = run(fake, *requests)
    assert fatal is None
    assert replies[0]["type"] == "hello"
    return replies[1:]


def loaded(fake: FakeWhisper, *requests) -> list[dict[str, Any]]:
    """Replies to ``requests`` after a successful load of a cached turbo."""
    fake.cached.add(TURBO)
    replies = serve(fake, LOAD, *requests)
    assert replies[1]["type"] == "loaded"
    return replies[2:]


# Loading.


def test_a_cached_model_loads_without_downloading():
    fake = FakeWhisper(cached={TURBO})
    assert serve(fake, LOAD) == [
        {"type": "progress", "phase": "load"},
        {"type": "loaded", "model": "large-v3-turbo", "revision": REMOTE[TURBO], "load_ms": pytest.approx(0, abs=1000)},
    ]
    assert fake.downloads == []
    assert fake.models == [{"path": TURBO_DIR, "device": "cuda", "compute_type": "int8_float16"}]
    assert len(fake.warmed_up) == 1


def test_running_out_of_gpu_memory_while_warming_up_fails_the_load():
    fake = FakeWhisper(cached={TURBO}, warm_up_error=RuntimeError("CUDA failed with error out of memory"))
    error = serve(fake, LOAD, transcribe())
    assert [(r["type"], r.get("code")) for r in error] == [
        ("progress", None), ("error", "load_failed"), ("error", "not_loaded")
    ]  # fmt: skip
    assert "out of memory" in error[1]["message"]
    assert fake.downloads == []


def test_a_missing_model_is_downloaded_reporting_the_transferred_bytes():
    fake = FakeWhisper()
    replies = serve(fake, LOAD)
    assert replies[0] == {"type": "progress", "phase": "download"}
    transfer = [(r["bytes"], r["total"]) for r in replies if r["type"] == "progress" and "bytes" in r]
    assert transfer[0] == (400, 1000)
    assert set(transfer) <= {(400, 1000), (1000, 1000)}  # never the reconstruction bar's 300 or 700
    assert replies[-2:] == [
        {"type": "progress", "phase": "load"},
        {"type": "loaded", "model": "large-v3-turbo", "revision": REMOTE[TURBO], "load_ms": pytest.approx(0, abs=1000)},
    ]
    assert fake.downloads == [{"repo": TURBO, "revision": None, "force_download": False}]


def test_a_pinned_revision_is_loaded_and_reported():
    fake = FakeWhisper(cached={TURBO})
    pinned = "fedcba9876543210fedcba9876543210fedcba98"
    replies = serve(fake, {**LOAD, "revision": pinned})
    assert replies[-1]["revision"] == pinned
    assert fake.models[0]["path"] == snapshot_dir(TURBO, pinned)


def test_a_cached_model_that_fails_to_load_is_downloaded_again():
    fake = FakeWhisper(cached={TURBO}, broken={TURBO_DIR: RuntimeError("Variable 'decoder/embeddings/weight' has an invalid payload size")})
    replies = serve(fake, LOAD)
    assert [r.get("phase", r["type"]) for r in replies if "bytes" not in r] == ["load", "download", "load", "loaded"]
    assert fake.downloads == [{"repo": TURBO, "revision": None, "force_download": True}]
    assert len(fake.models) == 2


@pytest.mark.parametrize(
    "error",
    [
        ValueError("Requested int8_float16 compute type, but the target device or backend do not support efficient int8_float16 computation."),
        RuntimeError("CUDA failed with error out of memory"),
    ],
)
def test_a_bad_device_or_no_gpu_memory_fails_the_load_without_downloading(error):
    fake = FakeWhisper(cached={TURBO}, broken={TURBO_DIR: error})
    replies = serve(fake, LOAD, transcribe())
    assert replies[-2]["type"] == "error" and replies[-2]["code"] == "load_failed"
    assert str(error) in replies[-2]["message"]
    assert replies[-1]["code"] == "not_loaded"
    assert fake.downloads == []


CUDA_ERROR = RuntimeError("CUDA failed with error an illegal memory access was encountered")


@pytest.mark.parametrize(
    "fake",
    [
        FakeWhisper(cached={TURBO}, broken={TURBO_DIR: CUDA_ERROR}),
        FakeWhisper(cached={TURBO}, warm_up_error=CUDA_ERROR),
        FakeWhisper(warm_up_error=CUDA_ERROR),
    ],
    ids=["loading-cached", "warming-up-cached", "warming-up-downloaded"],
)
def test_other_cuda_errors_while_loading_end_the_engine_without_a_reply(fake):
    replies, fatal = run(fake, LOAD, transcribe())
    assert "illegal memory access" in str(fatal)
    assert replies[-1] == {"type": "progress", "phase": "load"}
    assert all(r["type"] != "error" for r in replies)
    assert all(d["force_download"] is False for d in fake.downloads)


def test_a_failed_download_fails_the_load():
    fake = FakeWhisper(download_error=OSError("connection reset by peer"))
    error = serve(fake, LOAD)[-1]
    assert (error["type"], error["code"]) == ("error", "load_failed")
    assert "connection reset by peer" in error["message"]


def test_an_unknown_model_fails_the_load():
    error = serve(FakeWhisper(), {**LOAD, "model": "huge-v9"})[-1]
    assert (error["code"], error["retryable"]) == ("load_failed", False)
    assert "huge-v9" in error["message"] and "large-v3-turbo" in error["message"]


# Transcribing.


def test_transcribes_on_the_batched_path_with_the_bias_as_the_initial_prompt():
    fake = FakeWhisper()
    [result] = loaded(fake, transcribe(7, path="batched", beam_size=5, word_timestamps=True, vad_filter=True, batch_size=8))
    assert result["id"] == 7
    trace = result["trace"]
    assert (trace["engine"], trace["model"], trace["revision"], trace["path"]) == (
        "faster-whisper", "large-v3-turbo", REMOTE[TURBO], "batched"
    )  # fmt: skip
    assert trace["options"] == OPTIONS
    assert trace["prompt"] == PROMPT
    assert trace["result"] == {
        "language": "en",
        "speech_ms": 2500,
        "segments": [
            {
                "text": " Push the fix to psst.",
                "start_ms": 0,
                "end_ms": 2100,
                "no_speech_prob": 0.0123,
                "words": [
                    {"text": " Push", "start_ms": 0, "end_ms": 300, "prob": 0.98},
                    {"text": " the", "start_ms": 300, "end_ms": 450, "prob": 0.99},
                    {"text": " fix", "start_ms": 450, "end_ms": 800, "prob": 0.97},
                    {"text": " to", "start_ms": 800, "end_ms": 950, "prob": 0.99},
                    {"text": " psst.", "start_ms": 950, "end_ms": 2100, "prob": 0.6123},
                ],
            }
        ],
    }
    timings = result["timings"]
    # The batched path encodes the prompt without a leading space.
    assert timings["prompt_tokens"] == len(fake_encode(PROMPT))
    assert (timings["bias_terms_used"], timings["prompt_truncated"]) == (3, False)
    assert fake.runs == [
        (
            "batched",
            {
                "language": "en",
                "beam_size": 5,
                "word_timestamps": True,
                "vad_filter": True,
                "initial_prompt": PROMPT,
                "hotwords": None,
                "batch_size": 8,
            },
        )
    ]


def test_options_default_to_the_documented_defaults():
    fake = FakeWhisper()
    [result] = loaded(fake, transcribe())
    assert result["trace"]["options"] == OPTIONS
    assert fake.runs[0][0] == "batched"


def test_the_batch_size_is_passed_on_and_recorded():
    fake = FakeWhisper()
    [result] = loaded(fake, transcribe(batch_size=1))
    assert fake.runs[0][1]["batch_size"] == 1
    assert result["trace"]["options"]["batch_size"] == 1


def test_hotwords_carry_the_terms_under_the_same_budget():
    fake = FakeWhisper()
    [result] = loaded(fake, transcribe(terms_as="hotwords"))
    _, kwargs = fake.runs[0]
    assert (kwargs["initial_prompt"], kwargs["hotwords"]) == (STYLE, "Hyprland, psst, t3code.")
    assert result["trace"]["prompt"] == PROMPT
    assert result["trace"]["options"]["terms_as"] == "hotwords"
    assert result["timings"]["prompt_tokens"] == len(fake_encode(STYLE)) + len(fake_encode(" Hyprland, psst, t3code."))


def test_the_prompt_is_counted_without_a_leading_space():
    [result] = loaded(FakeWhisper(), transcribe(style="abcd", terms=[]))
    assert result["timings"]["prompt_tokens"] == 1  # " abcd" would be 2


def test_terms_beyond_the_budget_are_dropped_from_the_end():
    fake = FakeWhisper()
    terms = [f"term{i}" for i in range(300)]
    [result] = loaded(fake, transcribe(terms=terms))
    used = result["timings"]["bias_terms_used"]
    assert 0 < used < len(terms)
    assert result["trace"]["prompt"] == STYLE + " " + ", ".join(terms[:used]) + "."
    assert result["timings"]["prompt_tokens"] <= 223
    assert not result["timings"]["prompt_truncated"]


def test_a_style_over_the_budget_is_truncated():
    fake = FakeWhisper()
    style = " ".join(f"word{i}" for i in range(300))
    [result] = loaded(fake, transcribe(style=style))
    assert result["timings"]["prompt_truncated"] is True
    assert result["timings"]["bias_terms_used"] == 0
    assert style.endswith(result["trace"]["prompt"])
    assert fake.runs[0][1]["initial_prompt"] == result["trace"]["prompt"]


def test_no_bias_sends_no_prompt():
    fake = FakeWhisper()
    [result] = loaded(fake, transcribe(style="", terms=[]))
    assert (fake.runs[0][1]["initial_prompt"], fake.runs[0][1]["hotwords"]) == (None, None)
    assert (result["trace"]["prompt"], result["timings"]["prompt_tokens"]) == ("", 0)


def test_without_word_timestamps_segments_have_no_words():
    fake = FakeWhisper(segments=[default_segment(words=False)])
    [result] = loaded(fake, transcribe(word_timestamps=False))
    assert "words" not in result["trace"]["result"]["segments"][0]
    assert fake.runs[0][1]["word_timestamps"] is False


@pytest.mark.parametrize(
    "request_",
    [
        transcribe(vad_filter=False),
        transcribe(path="sequential"),
        transcribe(path="auto"),
        transcribe(path="greedy"),
        transcribe(beam_size=0),
        transcribe(beam_size=True),
        transcribe(batch_size=0),
        transcribe(word_timestamps="yes"),
        transcribe(terms_as="suffix"),
        transcribe(language="xx"),
    ],
)
def test_invalid_options_and_languages_are_bad_requests(request_):
    fake = FakeWhisper()
    replies = loaded(fake, request_, transcribe(2))
    assert replies[0] == {"type": "error", "id": 1, "code": "bad_request", "message": replies[0]["message"], "retryable": False}
    assert replies[1]["type"] == "result" and replies[1]["id"] == 2
    assert len(fake.runs) == 1


def test_unknown_options_are_ignored():
    fake = FakeWhisper()
    [result] = loaded(fake, transcribe(temperature=0.3))
    assert "temperature" not in result["trace"]["options"]
    assert "temperature" not in fake.runs[0][1]


def test_after_running_out_of_gpu_memory_the_engine_replies_oom_and_the_retry_works():
    # The fake fails the first call after an out-of-memory error, as CTranslate2 does.
    fake = FakeWhisper(fail_next=OUT_OF_MEMORY)
    replies = loaded(fake, transcribe(1, batch_size=8), transcribe(2, batch_size=1), transcribe(3))
    assert replies[0] == {"type": "error", "id": 1, "code": "oom", "message": str(OUT_OF_MEMORY), "retryable": True}
    assert [(r["type"], r["id"]) for r in replies[1:]] == [("result", 2), ("result", 3)]
    assert [kwargs["batch_size"] for _, kwargs in fake.runs] == [8, 1, 8]


def test_an_engine_whose_model_still_fails_after_running_out_of_gpu_memory_exits_without_replying():
    fake = FakeWhisper(cached={TURBO}, fail_next=OUT_OF_MEMORY, after_oom=[INVALID_DEVICE, INVALID_DEVICE])
    replies, fatal = run(fake, LOAD, transcribe(1), transcribe(2))
    assert "cudaErrorInvalidDevice" in str(fatal)
    assert [r["type"] for r in replies] == ["hello", "progress", "loaded"]


def test_other_cuda_errors_end_the_engine_without_a_reply():
    fake = FakeWhisper(cached={TURBO}, fail_next=RuntimeError("CUDA failed with error an illegal memory access was encountered"))
    replies, fatal = run(fake, LOAD, transcribe(1), transcribe(2))
    assert "illegal memory access" in str(fatal)
    assert [r["type"] for r in replies] == ["hello", "progress", "loaded"]


def test_other_errors_are_internal_errors_and_the_engine_keeps_serving():
    fake = FakeWhisper(fail_next=IndexError("list index out of range"))
    replies = loaded(fake, transcribe(1), transcribe(2))
    assert replies[0] == {"type": "error", "id": 1, "code": "internal", "message": "IndexError: list index out of range", "retryable": False}
    assert replies[1]["type"] == "result"
