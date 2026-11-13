"""Shared helpers for the Phase 0 engine benchmarks: eval cases, test audio, bias, metrics, VRAM sampling."""

from __future__ import annotations

import json
import os
import re
import subprocess
import threading
import time
import tomllib
import unicodedata
from dataclasses import dataclass, field
from pathlib import Path

import numpy as np

SR = 16000
REPO = Path(__file__).resolve().parents[2]
EVALS = REPO / "evals"
MAX_BIAS_TOKENS = 223  # plan 5.3: style and terms together, prompt or hotwords
MODEL = "large-v3-turbo"


# --- eval cases and audio -------------------------------------------------------------------------------------------


@dataclass
class Case:
    id: str
    lang: str
    split: str
    reference: str
    terms: list[str]
    must_pass: bool
    audio: np.ndarray = field(repr=False)

    @property
    def seconds(self) -> float:
        return len(self.audio) / SR

    @property
    def is_noise(self) -> bool:
        return self.reference == ""


def load_cases(source: str = "synthetic") -> list[Case]:
    from faster_whisper import decode_audio

    cases = []
    for line in (EVALS / "cases.jsonl").read_text(encoding="utf-8").splitlines():
        c = json.loads(line)
        if c["source"] != source:
            continue
        audio = decode_audio(str(EVALS / "clips" / f"{c['id']}.flac"), sampling_rate=SR)
        cases.append(Case(c["id"], c["lang"], c["split"], c["reference"], c["terms"], c["must_pass"], audio))
    return cases


@dataclass
class Composite:
    """Test audio built by concatenating eval clips, padded with silence to an exact length."""

    name: str
    lang: str
    audio: np.ndarray = field(repr=False)
    reference: str
    terms: list[str]
    parts: list[tuple[str, float]]  # (case id, start in seconds)
    # term occurrences per part start, so recall can be split at the first 30 s window
    occurrences: list[tuple[str, float]] = field(default_factory=list)


GAP_S = 0.6


def composite(name: str, pool: list[Case], target_s: float) -> Composite:
    """Greedy fill: take clips from `pool` in order while they fit, then pad to exactly `target_s`."""
    pieces, parts, refs, occ = [], [], [], []
    t = 0.0
    for c in pool:
        need = c.seconds + (GAP_S if pieces else 0.0)
        if t + need > target_s - 0.2:
            continue
        if pieces:
            pieces.append(np.zeros(int(GAP_S * SR), dtype=np.float32))
            t += GAP_S
        parts.append((c.id, t))
        refs.append(c.reference)
        occ += [(term, t) for term in c.terms for _ in range(count_term(c.reference, term))]
        pieces.append(c.audio)
        t += c.seconds
    audio = np.concatenate(pieces)
    audio = np.concatenate([audio, np.zeros(int(round(target_s * SR)) - len(audio), dtype=np.float32)])
    terms = list(dict.fromkeys(term for term, _ in occ))
    return Composite(name, pool[0].lang, audio, " ".join(refs), terms, parts, occ)


def duration_composites(cases: list[Case], targets=(2, 5, 15, 30, 60, 180)) -> dict[int, Composite]:
    """English test audio of the given lengths: long dictations first, then ordinary sentences."""
    en = [c for c in cases if c.lang == "en" and not c.is_noise]
    longs = [c for c in en if "-long-" in c.id]
    regular = [c for c in en if "-long-" not in c.id]
    out = {}
    for target in targets:
        pool = (longs + regular) if target >= 60 else sorted(regular, key=lambda c: -c.seconds)
        out[target] = composite(f"en-{target}s", pool, target)
    return out


def term_composites(cases: list[Case]) -> list[Composite]:
    """60 and 180 s of English sentences with terms, and 60 s of Czech: terms after the first 30 s window."""
    out = []
    for lang, target in (("en", 60), ("en", 180), ("cs", 60)):
        pool = [c for c in cases if c.lang == lang and c.terms and "-long-" not in c.id and "-short-" not in c.id]
        out.append(composite(f"{lang}-terms-{target}s", pool, target))
    return out


# --- bias (plan 5.3 and 7.2) ----------------------------------------------------------------------------------------


def load_profiles() -> dict:
    return tomllib.loads((EVALS / "profiles.toml").read_text(encoding="utf-8"))


def style_for(lang: str) -> str:
    profiles = load_profiles()["profiles"]
    return next(p["prompt_style"] for p in profiles.values() if p["language"] == lang)


def fillers_for(lang: str) -> list[str]:
    return load_profiles()["fillers"].get(lang, [])


def bias_terms(lang: str) -> list[str]:
    """Dictionary::bias_terms: valid for the language, pinned first, then the rest, each alphabetical."""
    terms = tomllib.loads((EVALS / "dictionary.toml").read_text(encoding="utf-8"))["terms"]
    valid = [t for t in terms if t.get("languages", "any") in ("any", lang)]
    key = lambda t: (t["text"].casefold(), t["text"])  # noqa: E731
    return [t["text"] for t in sorted((t for t in valid if t.get("pin")), key=key)] + [
        t["text"] for t in sorted((t for t in valid if not t.get("pin")), key=key)
    ]


@dataclass
class Bias:
    initial_prompt: str | None
    hotwords: str | None
    terms_used: int
    tokens: int


def build_bias(tokenizer, lang: str, path: str, terms_as: str | None) -> Bias:
    """terms_as None: no bias at all. Otherwise style plus as many terms as fit in 223 tokens (plan 5.3).

    Tokens are counted as each path encodes them: the sequential path adds a leading space to the prompt, the batched
    path doesn't, and both add one to hotwords.
    """
    if terms_as is None:
        return Bias(None, None, 0, 0)
    style = style_for(lang)
    terms = bias_terms(lang)

    def prompt_tokens(prompt: str) -> int:
        return len(tokenizer.encode(" " + prompt.strip() if path == "sequential" else prompt))

    for n in range(len(terms), -1, -1):
        listed = ", ".join(terms[:n])
        if terms_as == "prompt":
            prompt = f"{style} {listed}." if n else style
            tokens, hotwords = prompt_tokens(prompt), None
        else:
            prompt, hotwords = style, (listed or None)
            tokens = prompt_tokens(style) + (len(tokenizer.encode(" " + listed)) if n else 0)
        if tokens <= MAX_BIAS_TOKENS:
            return Bias(prompt, hotwords, n, tokens)
    raise ValueError("style alone is over the budget")


# --- metrics (plan 7.3) ---------------------------------------------------------------------------------------------

_BOUNDARY = r"\s.,;:!?()\[\]{}\"'"


def term_pattern(term: str, flags=0) -> re.Pattern:
    return re.compile(rf"(?<![^{_BOUNDARY}]){re.escape(term)}(?![^{_BOUNDARY}])", flags)


def count_term(text: str, term: str, ignore_case: bool = False) -> int:
    return len(term_pattern(term, re.IGNORECASE if ignore_case else 0).findall(text))


def normalize(text: str, lang: str) -> list[str]:
    """NFC, lowercase, punctuation stripped except apostrophes and hyphens inside words, fillers removed."""
    text = unicodedata.normalize("NFC", text).lower().replace("’", "'")
    text = "".join(ch if ch.isalnum() or ch in "'-" or ch.isspace() else " " for ch in text)
    text = re.sub(r"(?<!\w)['-]+|['-]+(?!\w)", " ", text)
    fillers = set(fillers_for(lang))
    return [w for w in text.split() if w not in fillers]


def edit_distance(ref: list[str], hyp: list[str]) -> int:
    prev = list(range(len(hyp) + 1))
    for i, r in enumerate(ref, 1):
        cur = [i] + [0] * len(hyp)
        for j, h in enumerate(hyp, 1):
            cur[j] = min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (r != h))
        prev = cur
    return prev[-1]


def wer_counts(reference: str, output: str, lang: str) -> tuple[int, int]:
    ref, hyp = normalize(reference, lang), normalize(output, lang)
    return edit_distance(ref, hyp), len(ref)


def term_hits(reference: str, output: str, terms: list[str]) -> tuple[int, int]:
    """(occurrences found with the exact spelling, occurrences in the reference)."""
    found = total = 0
    for term in terms:
        n = count_term(reference, term)
        total += n
        found += min(n, count_term(output, term))
    return found, total


def leaks(reference: str, output: str, lang: str, bias_terms_used: list[str]) -> list[str]:
    """Bias terms in the output that the reference doesn't have, and echoes of the style sentence (4+ words)."""
    found = [t for t in bias_terms_used
             if count_term(output, t, ignore_case=True) > count_term(reference, t, ignore_case=True)]
    style, out, ref = (" ".join(normalize(x, lang)) for x in (style_for(lang), output, reference))
    words = style.split()
    for i in range(len(words) - 3):
        gram = " ".join(words[i:i + 4])
        if gram in out and gram not in ref:
            found.append(f"style: {gram}")
            break
    return found


# --- running ---------------------------------------------------------------------------------------------------------


def load_model():
    from faster_whisper import BatchedInferencePipeline, WhisperModel

    t0 = time.perf_counter()
    model = WhisperModel(MODEL, device="cuda", compute_type="int8_float16")
    load_s = time.perf_counter() - t0
    return model, BatchedInferencePipeline(model=model), load_s


def tokenizer_for(model, lang: str):
    from faster_whisper.tokenizer import Tokenizer

    return Tokenizer(model.hf_tokenizer, True, task="transcribe", language=lang)


def transcribe(model, batched, audio: np.ndarray, *, lang: str, path: str, bias: Bias, word_timestamps: bool,
               batch_size: int = 8) -> dict:
    opts = dict(language=lang, beam_size=5, vad_filter=True, word_timestamps=word_timestamps,
                initial_prompt=bias.initial_prompt, hotwords=bias.hotwords)
    t0 = time.perf_counter()
    if path == "sequential":
        # With hotwords, the previous text and the hotwords would share the decoder's positions (plan 5.3).
        segments, info = model.transcribe(audio, condition_on_previous_text=bias.hotwords is None, **opts)
    else:
        segments, info = batched.transcribe(audio, batch_size=batch_size, **opts)
    segments = list(segments)
    seconds = time.perf_counter() - t0
    return {
        "seconds": seconds,
        "text": " ".join(s.text.strip() for s in segments if s.text.strip()),
        "speech_s": info.duration_after_vad,
        "segments": len(segments),
        "no_speech_max": max((s.no_speech_prob for s in segments), default=None),
    }


class VramSampler:
    """Peak GPU memory of this process (and of the whole GPU), sampled from nvidia-smi every 20 ms."""

    def __init__(self):
        self.pid = os.getpid()
        self.process_peak = 0
        self.gpu_peak = 0
        self._procs = [
            subprocess.Popen(["nvidia-smi", f"--query-{q}", "--format=csv,noheader,nounits", "-lms", "20"],
                             stdout=subprocess.PIPE, text=True)
            for q in ("compute-apps=pid,used_memory", "gpu=memory.used")
        ]
        self._threads = [threading.Thread(target=self._read, args=(p, i), daemon=True)
                         for i, p in enumerate(self._procs)]
        for t in self._threads:
            t.start()

    def _read(self, proc, kind):
        for line in proc.stdout:
            fields = [f.strip() for f in line.split(",")]
            try:
                if kind == 0 and int(fields[0]) == self.pid:
                    self.process_peak = max(self.process_peak, int(fields[1]))
                elif kind == 1:
                    self.gpu_peak = max(self.gpu_peak, int(fields[0]))
            except (ValueError, IndexError):
                pass

    def reset(self):
        time.sleep(0.1)
        self.process_peak = self.gpu_peak = 0

    def peak(self) -> dict:
        time.sleep(0.2)  # let the last samples arrive
        return {"process_mib": self.process_peak, "gpu_mib": self.gpu_peak}

    def close(self):
        for p in self._procs:
            p.terminate()


def gpu_memory() -> dict:
    out = subprocess.run(["nvidia-smi", "--query-gpu=memory.used,memory.total", "--format=csv,noheader,nounits"],
                         capture_output=True, text=True, check=True).stdout.split(",")
    return {"used_mib": int(out[0]), "total_mib": int(out[1])}


def write(out, record: dict) -> None:
    out.write(json.dumps(record, ensure_ascii=False) + "\n")
    out.flush()
