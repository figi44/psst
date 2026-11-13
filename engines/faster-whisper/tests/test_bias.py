from __future__ import annotations

import os
from types import SimpleNamespace

import pytest

from fake_tokenizer import fake_encode
from psst_engine_faster_whisper import bias

STYLE = "I'm updating the NixOS config."
TERMS = ["Hyprland", "psst", "t3code"]


def render(style=STYLE, terms=TERMS, *, terms_as="prompt", budget=bias.BUDGET, encode=fake_encode):
    return bias.render(style, terms, terms_as=terms_as, encode=encode, budget=budget)


def prompt_cost(text: str, encode=fake_encode) -> int:
    return len(encode(text)) if text else 0


def hotwords_cost(text: str, encode=fake_encode) -> int:
    return len(encode(" " + text.strip())) if text else 0


@pytest.mark.parametrize(
    ("style", "terms", "text"),
    [
        (STYLE, TERMS, "I'm updating the NixOS config. Hyprland, psst, t3code."),
        (STYLE, [], "I'm updating the NixOS config."),
        ("", TERMS, "Hyprland, psst, t3code."),
        ("  " + STYLE + " ", [" psst ", "", "  "], "I'm updating the NixOS config. psst."),
    ],
)
def test_style_then_terms_form_the_prompt(style, terms, text):
    rendered = render(style, terms)
    assert (rendered.initial_prompt, rendered.hotwords, rendered.text) == (text, None, text)
    assert rendered.tokens == prompt_cost(text)
    assert rendered.terms_used == len([t for t in terms if t.strip()])
    assert not rendered.truncated


@pytest.mark.parametrize("terms_as", ["prompt", "hotwords"])
def test_no_bias_means_no_prompt(terms_as):
    rendered = render("", [], terms_as=terms_as)
    assert (rendered.initial_prompt, rendered.hotwords, rendered.text, rendered.tokens) == (None, None, "", 0)


def test_hotwords_take_the_terms_and_share_the_budget():
    rendered = render(terms_as="hotwords")
    assert rendered.initial_prompt == STYLE
    assert rendered.hotwords == "Hyprland, psst, t3code."
    assert rendered.text == "I'm updating the NixOS config. Hyprland, psst, t3code."
    assert rendered.tokens == prompt_cost(STYLE) + hotwords_cost("Hyprland, psst, t3code.")

    budget = prompt_cost(STYLE) + hotwords_cost("Hyprland, psst.")
    tight = render(terms_as="hotwords", budget=budget)
    assert (tight.hotwords, tight.terms_used, tight.tokens) == ("Hyprland, psst.", 2, budget)


@pytest.mark.parametrize("terms_as", ["prompt", "hotwords"])
def test_terms_are_dropped_from_the_end_until_it_fits(terms_as):
    terms = [f"term{i}" for i in range(300)]
    rendered = render(terms=terms, terms_as=terms_as)
    assert 0 < rendered.terms_used < len(terms)
    assert rendered.text == bias.join(STYLE, terms[: rendered.terms_used])
    assert rendered.tokens <= bias.BUDGET
    one_more = bias.join(STYLE, terms[: rendered.terms_used + 1])
    if terms_as == "prompt":
        assert prompt_cost(one_more) > bias.BUDGET
    else:
        assert prompt_cost(STYLE) + hotwords_cost(bias.join("", terms[: rendered.terms_used + 1])) > bias.BUDGET
    assert not rendered.truncated


def test_the_prompt_counts_without_a_leading_space_and_hotwords_with_one():
    # "abcd" is one token and " abcd" two; "." is one.
    as_prompt = render("", ["abcd"], budget=2)
    assert (as_prompt.initial_prompt, as_prompt.tokens) == ("abcd.", 2)
    as_hotwords = render("", ["abcd"], terms_as="hotwords", budget=2)
    assert (as_hotwords.hotwords, as_hotwords.terms_used, as_hotwords.tokens) == (None, 0, 0)


@pytest.mark.parametrize("terms_as", ["prompt", "hotwords"])
def test_a_style_over_the_budget_is_cut_from_its_start(terms_as):
    words = [f"word{i}" for i in range(200)]
    style = " ".join(words) + "."
    rendered = render(style, TERMS, terms_as=terms_as)
    assert rendered.truncated
    assert (rendered.terms_used, rendered.hotwords) == (0, None)
    assert style.endswith(rendered.text) and rendered.text.startswith("word")
    assert rendered.initial_prompt == rendered.text
    assert rendered.tokens == prompt_cost(rendered.text) <= bias.BUDGET
    kept = len(rendered.text.split())
    assert prompt_cost(" ".join(words[-kept - 1 :]) + ".") > bias.BUDGET


def test_a_style_of_one_long_word_is_cut_inside_it():
    rendered = render("x" * 2000, [], budget=10)
    assert rendered.truncated
    assert rendered.tokens <= 10
    assert rendered.text == "x" * len(rendered.text) and len(rendered.text) > 30


def whisper_tokenizer():
    pytest.importorskip("faster_whisper")
    from huggingface_hub import try_to_load_from_cache
    from tokenizers import Tokenizer

    from faster_whisper.tokenizer import Tokenizer as WhisperTokenizer

    for repo in (
        "mobiuslabsgmbh/faster-whisper-large-v3-turbo",
        "Systran/faster-whisper-tiny",
        "Systran/faster-whisper-base",
        "Systran/faster-whisper-large-v3",
    ):
        path = try_to_load_from_cache(repo, "tokenizer.json")
        if isinstance(path, str) and os.path.isfile(path):
            return WhisperTokenizer(Tokenizer.from_file(path), multilingual=True, task="transcribe", language="en")
    pytest.skip("no Whisper tokenizer in the local Hugging Face cache")


def chunk_bias(tokenizer, rendered: bias.RenderedBias) -> list[int]:
    from faster_whisper import WhisperModel

    # As faster-whisper 1.2.1's BatchedInferencePipeline.generate_segment_batched does it.
    previous = [] if rendered.initial_prompt is None else tokenizer.encode(rendered.initial_prompt)
    prompt = WhisperModel.get_prompt(
        SimpleNamespace(max_length=448), tokenizer, previous, without_timestamps=True, hotwords=rendered.hotwords
    )
    start = prompt.index(tokenizer.sot)
    assert prompt[start:] == [*tokenizer.sot_sequence, tokenizer.no_timestamps]
    if start == 0:
        return []
    assert prompt[0] == tokenizer.sot_prev
    return prompt[1:start]


TECH_TERMS = [
    "Hyprland", "psst", "t3code", "NixOS", "nixpkgs", "Wayland", "niri", "PipeWire", "CTranslate2", "faster-whisper",
    "Tailscale", "WireGuard", "systemd", "Ghostty", "Dvořák", "Kubernetes", "PostgreSQL", "Grafana", "SQLite", "rustfmt",
]  # fmt: skip


@pytest.mark.parametrize("terms_as", ["prompt", "hotwords"])
def test_real_tokenizer_budget_matches_what_faster_whisper_feeds_the_decoder(terms_as):
    tokenizer = whisper_tokenizer()
    terms = [f"{term}{i}" for i in range(30) for term in TECH_TERMS]
    rendered = render(STYLE, terms, terms_as=terms_as, encode=tokenizer.encode)

    fed = chunk_bias(tokenizer, rendered)
    assert len(fed) == rendered.tokens <= bias.BUDGET
    assert 0 < rendered.terms_used < len(terms)
    if terms_as == "hotwords":
        assert tokenizer.decode(fed).startswith(" " + rendered.hotwords)
    else:
        assert tokenizer.decode(fed).strip() == rendered.text

    one_more = render(STYLE, terms[: rendered.terms_used + 1], terms_as=terms_as, encode=tokenizer.encode, budget=10_000)
    assert one_more.tokens > bias.BUDGET


@pytest.mark.parametrize("terms_as", ["prompt", "hotwords"])
def test_real_tokenizer_cuts_a_long_style_so_faster_whisper_keeps_all_of_it(terms_as):
    tokenizer = whisper_tokenizer()
    style = " ".join(f"Sentence number {i} talks about NixOS and Hyprland." for i in range(60))
    rendered = render(style, TERMS, terms_as=terms_as, encode=tokenizer.encode)
    assert rendered.truncated and rendered.terms_used == 0
    fed = chunk_bias(tokenizer, rendered)
    assert len(fed) == rendered.tokens <= bias.BUDGET
    assert style.endswith(rendered.text)
    assert tokenizer.decode(fed).strip() == rendered.text
