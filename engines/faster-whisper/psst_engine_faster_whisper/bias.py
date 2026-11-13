from __future__ import annotations

from collections.abc import Callable, Sequence
from dataclasses import dataclass

BUDGET = 223
"""faster-whisper's ``max_length // 2 - 1``: ``get_prompt`` keeps the last this many tokens of the
initial prompt and the first this many of hotwords, so within it nothing is cut."""

PROMPT = "prompt"
HOTWORDS = "hotwords"

Encode = Callable[[str], Sequence[int]]


@dataclass(frozen=True)
class RenderedBias:
    initial_prompt: str | None
    hotwords: str | None
    text: str
    tokens: int
    terms_used: int
    truncated: bool


def render(
    style: str,
    terms: Sequence[str],
    *,
    terms_as: str,
    encode: Encode,
    budget: int = BUDGET,
) -> RenderedBias:
    if terms_as not in (PROMPT, HOTWORDS):
        raise ValueError(f"unknown terms_as: {terms_as}")
    style = style.strip()
    terms = [term.strip() for term in terms if term.strip()]

    # As faster-whisper 1.2.1's batched path encodes them: the initial prompt as it is
    # (``BatchedInferencePipeline.generate_segment_batched``), hotwords after a space
    # (``WhisperModel.get_prompt``).
    def prompt_tokens(text: str) -> int:
        return len(encode(text)) if text else 0

    def hotwords_tokens(text: str) -> int:
        return len(encode(" " + text)) if text else 0

    def cost(style: str, k: int) -> int:
        if terms_as == PROMPT:
            return prompt_tokens(join(style, terms[:k]))
        return prompt_tokens(style) + hotwords_tokens(join("", terms[:k]))

    truncated = False
    if cost(style, len(terms)) <= budget:
        used = len(terms)
    elif cost(style, 0) <= budget:
        # The largest count that fits, assuming more terms never take fewer tokens.
        lo, hi = 0, len(terms) - 1
        while lo < hi:
            mid = (lo + hi + 1) // 2
            if cost(style, mid) <= budget:
                lo = mid
            else:
                hi = mid - 1
        used = lo
    else:
        # From its start, as faster-whisper would cut it, but at a word boundary.
        style = cut_start(style, lambda text: prompt_tokens(text) <= budget)
        truncated = True
        used = 0

    used_terms = terms[:used]
    text = join(style, used_terms)
    if terms_as == PROMPT:
        initial_prompt, hotwords = text, ""
    else:
        initial_prompt, hotwords = style, join("", used_terms)
    return RenderedBias(
        initial_prompt=initial_prompt or None,
        hotwords=hotwords or None,
        text=text,
        tokens=cost(style, used),
        terms_used=used,
        truncated=truncated,
    )


def join(style: str, terms: Sequence[str]) -> str:
    listed = ", ".join(terms) + "." if terms else ""
    return " ".join(part for part in (style, listed) if part)


def cut_start(text: str, fits: Callable[[str], bool]) -> str:
    """Assumes a shorter suffix never fits worse."""
    word_starts = [i for i, ch in enumerate(text) if not ch.isspace() and (i == 0 or text[i - 1].isspace())]
    found = _first_fitting([text[i:] for i in word_starts], fits)
    if found is not None:
        return found
    last = word_starts[-1] if word_starts else 0
    found = _first_fitting([text[i:] for i in range(last + 1, len(text))], fits)
    return found or ""


def _first_fitting(candidates: Sequence[str], fits: Callable[[str], bool]) -> str | None:
    lo, hi = 0, len(candidates)
    while lo < hi:
        mid = (lo + hi) // 2
        if fits(candidates[mid]):
            hi = mid
        else:
            lo = mid + 1
    return candidates[lo] if lo < len(candidates) else None
