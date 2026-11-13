"""How ``transcribe.bias`` becomes faster-whisper's ``initial_prompt`` and ``hotwords``.

The bias text is ``style + " " + ", ".join(terms) + "."``, just ``style`` without terms, or empty.
With ``terms_as = "prompt"`` all of it is the initial prompt. With ``terms_as = "hotwords"`` it's
split: ``style`` is the initial prompt and ``", ".join(terms) + "."`` is ``hotwords``.

Style and terms share one budget of 223 tokens, counted with the model's tokenizer exactly as
faster-whisper 1.2.1's batched path encodes them:

- the initial prompt as it is, without a leading space: ``encode(prompt)``
  (``BatchedInferencePipeline.generate_segment_batched``);
- hotwords with one: ``encode(" " + hotwords.strip())`` (``WhisperModel.get_prompt``).

``get_prompt`` keeps only the last 223 tokens of the initial prompt and the first 223 of hotwords,
so within the budget nothing is cut. The batched path sends the same prompt with every chunk. Terms
are dropped from the end until the whole fits. A style that alone is over the budget is cut from
its start, as faster-whisper itself would cut it, but at a word boundary.
"""

from __future__ import annotations

from collections.abc import Callable, Sequence
from dataclasses import dataclass

BUDGET = 223
"""Tokens for style and terms together: faster-whisper's ``max_length // 2 - 1``."""

PROMPT = "prompt"
HOTWORDS = "hotwords"

Encode = Callable[[str], Sequence[int]]
"""The model's tokenizer: text to token ids, without special tokens."""


@dataclass(frozen=True)
class RenderedBias:
    initial_prompt: str | None
    """Passed as ``initial_prompt``; ``None`` when empty."""
    hotwords: str | None
    """Passed as ``hotwords``; ``None`` when empty."""
    text: str
    """The bias text actually used (style, then terms), for ``EngineTrace.prompt``."""
    tokens: int
    """Tokens of the initial prompt and hotwords, counted as the batched path encodes them."""
    terms_used: int
    truncated: bool
    """Whether ``style`` alone was over the budget and was cut from its start."""


def render(
    style: str,
    terms: Sequence[str],
    *,
    terms_as: str,
    encode: Encode,
    budget: int = BUDGET,
) -> RenderedBias:
    """Fits ``style`` and as many leading ``terms`` as possible into ``budget`` tokens.

    ``terms_as`` is ``prompt`` or ``hotwords``. Blank terms are skipped, and style and terms are
    stripped.
    """
    if terms_as not in (PROMPT, HOTWORDS):
        raise ValueError(f"unknown terms_as: {terms_as}")
    style = style.strip()
    terms = [term.strip() for term in terms if term.strip()]

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
    """``style + " " + ", ".join(terms) + "."``, just ``style`` without terms, or empty."""
    listed = ", ".join(terms) + "." if terms else ""
    return " ".join(part for part in (style, listed) if part)


def cut_start(text: str, fits: Callable[[str], bool]) -> str:
    """The longest suffix of ``text`` that ``fits``, starting at a word if one does.

    Assumes a shorter suffix never fits worse. Falls back to cutting inside the last word.
    """
    word_starts = [i for i, ch in enumerate(text) if not ch.isspace() and (i == 0 or text[i - 1].isspace())]
    found = _first_fitting([text[i:] for i in word_starts], fits)
    if found is not None:
        return found
    last = word_starts[-1] if word_starts else 0
    found = _first_fitting([text[i:] for i in range(last + 1, len(text))], fits)
    return found or ""


def _first_fitting(candidates: Sequence[str], fits: Callable[[str], bool]) -> str | None:
    """The first candidate that fits, by binary search over candidates ordered from worst fit."""
    lo, hi = 0, len(candidates)
    while lo < hi:
        mid = (lo + hi) // 2
        if fits(candidates[mid]):
            hi = mid
        else:
            lo = mid + 1
    return candidates[lo] if lo < len(candidates) else None
