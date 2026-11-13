"""Invent the eval set's dictionary terms, probe them with Whisper, and write evals/dictionary.toml.

Every term in the eval set is invented, or for the `real` class a real place or science name, so the eval set names
no product or company. Each class is a way a term can go wrong in dictation (CLASSES). The steps:

1. `generate` writes evals/terms.toml: seeded candidates for every class, more than its quota, leaving out common
   English words (crates/psst-dict/data/common-words/en.txt). It keeps what later steps recorded for candidates it
   generates again.
2. `probe <dir>` writes a throwaway eval set to <dir>/evals: every candidate in the same plain sentence. Render it,
   record its traces, and read them back:

       scripts/synth-evals.sh --evals <dir>/evals --probe
       scripts/gpu-session.sh nix develop .#gpu --command \\
           cargo run --release -p psst-eval -- audio --evals <dir>/evals
       python3 -I scripts/lib/eval_terms.py heard <dir>

   `heard` records whether Whisper spelled each candidate exactly without a prompt, but not what it wrote instead:
   a mishearing can be an existing name. A class keeps the candidates Whisper gets wrong, so each one exercises the
   dictionary; `real` keeps the ones it gets right, which the dictionary must leave
   alone. A candidate that turns out to be an existing name, or that Whisper hears as one, goes into REVIEW_DROPS
   by hand, and `generate` leaves it out of terms.toml.
3. `dictionary` writes evals/dictionary.toml: psst and the first kept candidates of each class, up to its quota. With
   recorded traces in evals/cases.jsonl, it also mines each term's sounds_like from the tune split's traces, never the
   holdout split's, so holdout measures the dictionary on errors it wasn't tuned on. Run it again after the audio
   eval; sounds_like isn't part of the engine request, so the traces stay valid.

It needs only Python's standard library: `python3 -I scripts/lib/eval_terms.py <command>`.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import random
import re
import shutil
import sys
import tomllib
import unicodedata
from dataclasses import dataclass
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
EVALS = REPO / "evals"
COMMON_WORDS = REPO / "crates" / "psst-dict" / "data" / "common-words" / "en.txt"

# class -> (terms in the dictionary, fewest distinct terms the sentences must use, what it tests)
CLASSES: dict[str, tuple[int, int, str]] = {
    "project": (1, 1, "psst itself"),
    "coinage": (24, 12, "a made-up name that sounds like nothing else, so Whisper spells it its own way"),
    "soundalike": (9, 5, "an ordinary word respelled: Whisper writes the ordinary word, so only the prompt can help"),
    "letters": (8, 4, "a few letters, spoken one by one"),
    "alnum": (8, 4, "letters and a digit, like a tool or version name"),
    "camel": (12, 6, "two words joined by an inner capital"),
    "joined": (10, 5, "a lowercase compound or abbreviation, like a command name"),
    "hyphen": (8, 4, "two lowercase words joined by a hyphen"),
    "multiword": (7, 4, "two capitalised words that name one thing"),
    "real": (8, 4, "a real place or science name Whisper already knows: the dictionary must not break it"),
}

# Candidates generated per class: about twice the quota, and fixed, so changing a quota doesn't change the
# candidates. `real` takes every name in REAL.
CANDIDATES = {"coinage": 50, "soundalike": 22, "letters": 18, "alnum": 18, "camel": 26, "joined": 22, "hyphen": 18,
              "multiword": 18}

# Reviewed by hand, and listed by the start of the SHA-256 of their casefolded text (review_key), so the names among
# them stay out of the repository:
# - REVIEW_DROPS: candidates that turned out to be an existing name, or that Whisper hears as one. `generate` leaves
#   them out.
# - NOT_SOUNDS_LIKE: spellings Whisper wrote for a term that are ordinary words, phrases or names the common-word list
#   doesn't have. As sounds_like they would rewrite ordinary text.
REVIEW_DROPS = {
    "02c493fa64f7", "042c2aa74f18", "07e6321345cc", "0a1b411b4a64", "0a96f93b12ec", "0de55ccc9f3e",
    "11203b441725", "14dd8d20de8b", "15ac8b27ba38", "1e4e67ef798e", "21241bc1e4c4", "2a1cd59ad5ea",
    "2e4f3ca74b6e", "36bf7bd61200", "36c1f58028cc", "3856a0fd65c0", "4638f71e3100", "4ea716a6b793",
    "4eea097391fb", "51cf4a2aac52", "56e687dd1238", "57bb4f537f7d", "5e7945976ff8", "698ba01053c9",
    "73d0e61b60ea", "7492dbc6616c", "753ed79c3aff", "784d98114b03", "804f38661ef2", "809febc400c6",
    "80f7c2d63e79", "81f1b89df0a7", "8902bd709086", "8b0f802ae400", "8b71d5d587e8", "9722a138be55",
    "9c74138d6871", "9de0d0fa9c17", "a4c745facd3a", "bbc69861eabd", "bc261a2c449c", "bc3c2a9f939b",
    "d8a0c1bc63f2", "de3458e199df", "e97812e7e56b", "ecd3f6f72c50", "ee66b247b7b3", "ef21b87f17e1",
    "ef3a41c5eff2", "f2b46dec00e8", "fa9973f2ff88", "fd9538d3adc6",
}
NOT_SOUNDS_LIKE = {
    "0ab77abc5d93", "17840323ea59", "20fc93d6831d", "3169d53fe6bd", "3ce608fa5903", "4b948dc47e33",
    "4c1c69e423bc", "5adf4c73a0f6", "688cff831436", "6ec46bceb6f2", "92bc4f68b7fb", "c25668db5133",
    "ff4d55529fa5",
}

# The sentence every candidate is probed in: plain, so nothing around the term helps or hurts.
CARRIER = "Please add {} to the list."

# --- candidates -----------------------------------------------------------------------------------------------------

ONSETS = ["b", "br", "bl", "d", "dr", "f", "fl", "fr", "g", "gr", "gl", "h", "j", "k", "kr", "kl", "l", "m", "n",
          "p", "pr", "pl", "qu", "r", "s", "sk", "sl", "sn", "st", "t", "tr", "v", "w", "z"]
NUCLEI = ["a", "e", "i", "o", "u", "ai", "ee", "oo"]
CODAS = ["", "", "n", "r", "l", "x", "k", "m", "nd", "rk", "st", "sk", "th"]
ENDINGS = ["ly", "ix", "o", "a", "ex", "ium", "ora", "et", "yn", "is", "ar", "en"]
LIGHT_ONSETS = ["b", "d", "f", "g", "k", "l", "m", "n", "p", "r", "s", "t", "v", "z"]
LIGHT_CODAS = ["", "", "n", "r", "l", "x"]

# Ordinary words to respell, and respellings that keep the sound.
SOUNDALIKE_SOURCES = ["lantern", "harbor", "cobalt", "falcon", "willow", "ember", "beacon", "thistle", "quarry",
                      "ripple", "timber", "marble", "kettle", "cinder", "copper", "juniper", "quiver", "compass",
                      "pepper", "carpet", "candle", "pebble", "saddle", "cactus", "comet", "cotton", "ladder",
                      "cricket"]
RESPELLINGS = [(r"c(?=[aou])", "k"), (r"^c", "k"), (r"ck", "k"), (r"ern$", "urn"), (r"er$", "ur"), (r"or$", "ur"),
               (r"le$", "el"), (r"ow$", "o"), (r"qu", "kw"), (r"ea", "ee"), (r"et$", "ett")]

CONSONANTS = "bcdfghjklmnpqrstvwxz"
RARE = "qxzjvkw"
DIGITS = ["two", "three", "four", "five", "six", "seven", "eight", "nine"]

NATURE = ["fern", "slate", "moss", "flint", "cedar", "amber", "frost", "clover", "tide", "quartz", "birch", "heron",
          "basalt", "lichen", "otter", "sorrel"]
TOOLS = ["wire", "gate", "loom", "latch", "forge", "relay", "spool", "hatch", "dock", "vault", "beam", "grid",
         "shelf", "trellis"]
STEMS = ["spool", "tile", "ledger", "grain", "loom", "pulse", "scribe", "quill", "shard", "rivet", "kiln", "trowel"]
SUFFIXES = [("ctl", "control"), ("d", "D"), ("sync", "sync"), ("watch", "watch"), ("fmt", "format"),
            ("keep", "keep")]
ROLES = ["manager", "runner", "agent", "broker", "keeper", "watcher", "operator", "router"]
FIRST_WORDS = ["Harbor", "Ember", "Willow", "Granite", "Lantern", "Meadow", "Cedar", "Thistle", "Heron", "Basalt",
               "Juniper", "Kestrel"]
SECOND_WORDS = ["Desk", "Ledger", "Atlas", "Board", "Station", "Keeper", "Works", "Almanac", "Relay", "Compass"]

# Real names Whisper should already know: places and stars, nothing anyone sells.
REAL = ["Reykjavik", "Ljubljana", "Tbilisi", "Thessaloniki", "Kilimanjaro", "Okavango", "Svalbard", "Lofoten",
        "Cappadocia", "Aldebaran", "Ouagadougou", "Kathmandu", "Yerevan", "Antananarivo", "Nunavut",
        "Galapagos", "Arabidopsis"]


@dataclass
class Term:
    text: str
    cls: str
    spoken: str = ""  # what text-to-speech reads, when the spelling alone misleads it
    status: str = "new"  # new, kept, or "dropped: <why>"


def nfc(text: str) -> str:
    return unicodedata.normalize("NFC", text)


def common_words() -> set[str]:
    lines = COMMON_WORDS.read_text(encoding="utf-8").splitlines()
    return {line.strip().casefold() for line in lines if line.strip() and not line.startswith("#")}


def generate() -> list[Term]:
    """Every class's candidates, in a fixed order: the same on every run."""
    common = common_words()
    taken: set[str] = set()
    out = [Term("psst", "project", "p s s t")]
    taken.add("psst")

    def add(cls: str, text: str, spoken: str = "") -> bool:
        key = text.casefold()
        if key in taken or key in common:
            return False
        taken.add(key)
        out.append(Term(text, cls, spoken))
        return True

    def want(cls: str) -> int:
        return CANDIDATES[cls]

    rng = random.Random("psst-eval-terms coinage")
    n = 0
    while n < want("coinage"):
        # A full syllable, then a light one or a name-like ending, so the result is easy to say.
        word = rng.choice(ONSETS) + rng.choice(NUCLEI) + rng.choice(CODAS)
        if rng.random() < 0.6:
            word += rng.choice(ENDINGS)
        else:
            word += rng.choice(LIGHT_ONSETS) + rng.choice(NUCLEI) + rng.choice(LIGHT_CODAS)
        if not 4 <= len(word) <= 8 or re.search(r"(.)\1\1|[aeiou]{3}|[^aeiouy]{3}", word):
            continue
        n += add("coinage", word.capitalize() if rng.random() < 0.7 else word)

    rng = random.Random("psst-eval-terms soundalike")
    sources = SOUNDALIKE_SOURCES[:]
    rng.shuffle(sources)
    n = 0
    for source in sources:
        if n >= want("soundalike"):
            break
        rules = RESPELLINGS[:]
        rng.shuffle(rules)
        word = source
        for pattern, repl in rules:
            changed = re.sub(pattern, repl, word, count=1)
            if changed != word:
                word = changed
                if rng.random() < 0.5:
                    break
        if word != source:
            n += add("soundalike", word.capitalize(), source)

    rng = random.Random("psst-eval-terms letters")
    n = 0
    while n < want("letters"):
        letters = rng.sample(CONSONANTS, 3)
        if sum(c in RARE for c in letters) < 2:
            continue
        n += add("letters", "".join(letters), " ".join(c.upper() for c in letters))

    rng = random.Random("psst-eval-terms alnum")
    n = 0
    while n < want("alnum"):
        letter, digit = rng.choice(CONSONANTS), rng.randrange(2, 10)
        if rng.random() < 0.6:
            word = rng.choice(TOOLS + ["path", "link", "pack", "cast", "core", "s"])
            said = word.upper() if len(word) == 1 else word
            text, spoken = f"{letter}{digit}{word}", f"{letter.upper()} {DIGITS[digit - 2]} {said}"
        else:
            word = rng.choice(STEMS)
            text, spoken = f"{word}{digit}", f"{word} {DIGITS[digit - 2]}"
        n += add("alnum", text, spoken)

    rng = random.Random("psst-eval-terms camel")
    n = 0
    while n < want("camel"):
        a, b = rng.choice(NATURE), rng.choice(TOOLS)
        n += add("camel", a.capitalize() + b.capitalize(), f"{a} {b}")

    rng = random.Random("psst-eval-terms joined")
    n = 0
    while n < want("joined"):
        stem, (suffix, said) = rng.choice(STEMS), rng.choice(SUFFIXES)
        n += add("joined", stem + suffix, f"{stem} {said}")

    rng = random.Random("psst-eval-terms hyphen")
    n = 0
    while n < want("hyphen"):
        stem, role = rng.choice(STEMS + NATURE), rng.choice(ROLES)
        n += add("hyphen", f"{stem}-{role}", f"{stem} {role}")

    rng = random.Random("psst-eval-terms multiword")
    n = 0
    while n < want("multiword"):
        first, second = rng.choice(FIRST_WORDS), rng.choice(SECOND_WORDS)
        if first.casefold() != second.casefold():
            n += add("multiword", f"{first} {second}")

    for name in REAL:
        add("real", name)
    return [t for t in out if review_key(t.text) not in REVIEW_DROPS]


def review_key(text: str) -> str:
    return hashlib.sha256(text.casefold().encode()).hexdigest()[:12]


# --- terms.toml -----------------------------------------------------------------------------------------------------


def toml_str(value: str) -> str:
    return json.dumps(value, ensure_ascii=False)


def read_terms(path: Path = EVALS / "terms.toml") -> list[Term]:
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    return [Term(t["text"], t["class"], t.get("spoken", ""), t.get("status", "new"))
            for t in data.get("terms", [])]


def write_terms(terms: list[Term], path: Path = EVALS / "terms.toml") -> None:
    lines = [
        "# Candidate terms for the eval set, by class, and the Whisper probe's verdict on them. Written by",
        "# scripts/lib/eval_terms.py (`generate`, then `heard`); evals/dictionary.toml takes the first kept ones of",
        "# each class. Candidates that turned out to be an existing name are left out (REVIEW_DROPS).",
        "#",
        "# - class: what the term tests (CLASSES in eval_terms.py).",
        "# - spoken: what text-to-speech reads, when the spelling alone would mislead it.",
        "# - status: kept, or why it was dropped, from how Whisper wrote it without a prompt in",
        "#   \"" + CARRIER.format("…") + "\"",
    ]
    for t in terms:
        lines += ["", "[[terms]]", f"text = {toml_str(t.text)}", f"class = {toml_str(t.cls)}"]
        if t.spoken:
            lines.append(f"spoken = {toml_str(t.spoken)}")
        lines.append(f"status = {toml_str(t.status)}")
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def cmd_generate() -> int:
    path = EVALS / "terms.toml"
    old = {t.text: t for t in read_terms(path)} if path.exists() else {}
    terms = []
    for t in generate():
        prev = old.get(t.text)
        if prev and prev.cls == t.cls and prev.spoken == t.spoken:
            t.status = prev.status
        terms.append(t)
    write_terms(terms, path)
    for cls in CLASSES:
        print(f"{cls:10} {sum(t.cls == cls for t in terms):3} candidates", file=sys.stderr)
    return 0


# --- the probe ------------------------------------------------------------------------------------------------------


def write_dictionary_texts(path: Path, texts: list[str]) -> None:
    path.write_text("".join(f"[[terms]]\ntext = {toml_str(t)}\n\n" for t in texts), encoding="utf-8")


def cmd_probe(dir: Path) -> int:
    terms = read_terms()
    out = dir / "evals"
    if out.exists():
        print(f"error: {out} exists", file=sys.stderr)
        return 1
    (out / "sentences").mkdir(parents=True)
    for name in ("server.toml", "profiles.toml"):
        shutil.copy(EVALS / name, out / name)
    write_dictionary_texts(out / "dictionary.toml", [t.text for t in terms])
    lines = ["# The Whisper probe of evals/terms.toml, written by scripts/lib/eval_terms.py probe."]
    for i, t in enumerate(terms, 1):
        mark = f"[[{t.text}|{t.spoken}]]" if t.spoken else f"[[{t.text}]]"
        lines.append(f"en-{i:03d} {CARRIER.format(mark)}")
    (out / "sentences" / "en.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"wrote {len(terms)} probe sentences to {out}; next:\n"
          f"  scripts/synth-evals.sh --evals {out} --probe\n"
          f"  scripts/gpu-session.sh nix develop .#gpu --command cargo run --release -p psst-eval -- audio "
          f"--evals {out}\n"
          f"  python3 -I scripts/lib/eval_terms.py heard {dir}", file=sys.stderr)
    return 0


BOUNDARY = set(".,;:!?()[]{}\"'’‘“”„«»…—–")


def is_boundary(c: str) -> bool:
    return c.isspace() or c in BOUNDARY


def term_spans(text: str, term: str) -> list[tuple[int, int]]:
    """Where `term` occurs in `text` with its exact spelling, as psst-eval's text::count_term counts it."""
    spans: list[tuple[int, int]] = []
    start = 0
    while term and (pos := text.find(term, start)) != -1:
        end = pos + len(term)
        if (pos == 0 or is_boundary(text[pos - 1])) and (end == len(text) or is_boundary(text[end])):
            spans.append((pos, end))
            start = end
        else:
            start = pos + 1
    return spans


def trace_text(trace: dict) -> str:
    return nfc("".join(segment["text"] for segment in trace["result"]["segments"]).strip())


def read_cases(path: Path) -> list[dict]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def cmd_heard(dir: Path) -> int:
    terms = read_terms()
    cases = {c["terms"][0]: c for c in read_cases(dir / "evals" / "cases.jsonl") if c["terms"]}
    missing = 0
    for t in terms:
        case = cases.get(t.text)
        if not case or "engine_output_raw" not in case:
            missing += 1
            continue
        raw = trace_text(case["engine_output_raw"])
        right = bool(term_spans(raw, nfc(t.text)))
        if t.cls == "project":
            t.status = "kept"
        elif t.cls == "real":
            t.status = "kept" if right else "dropped: Whisper misspells it"
        else:
            t.status = "dropped: Whisper already spells it" if right else "kept"
    write_terms(terms)
    if missing:
        print(f"warning: {missing} candidates have no recorded probe", file=sys.stderr)
    for cls in CLASSES:
        kept = sum(t.cls == cls and t.status == "kept" for t in terms)
        print(f"{cls:10} {kept:3} kept of {sum(t.cls == cls for t in terms):3}, quota {CLASSES[cls][0]}",
              file=sys.stderr)
    return 0


# --- the dictionary -------------------------------------------------------------------------------------------------


def select(terms: list[Term]) -> list[Term]:
    """The first kept candidates of each class, up to its quota."""
    chosen = []
    for cls, (quota, _, _) in CLASSES.items():
        kept = [t for t in terms if t.cls == cls and t.status == "kept"][:quota]
        if len(kept) < quota:
            raise SystemExit(f"error: only {len(kept)} kept {cls} candidates for a quota of {quota}; "
                             "generate more and probe them")
        chosen += kept
    return chosen


WORD = re.compile(r"[\w]+(?:['’-][\w]+)*")


def words(text: str) -> list[tuple[str, int, int]]:
    """Normalised words with their spans, roughly as psst-eval's text::normalise splits them."""
    return [(m.group().casefold().replace("’", "'"), m.start(), m.end()) for m in WORD.finditer(text)]


def align(ref: list[str], hyp: list[str]) -> list[tuple[int | None, int | None]]:
    """Levenshtein alignment as (reference index, hypothesis index) pairs, None for a gap."""
    n, m = len(ref), len(hyp)
    cost = [[0] * (m + 1) for _ in range(n + 1)]
    for i in range(n + 1):
        cost[i][0] = i
    for j in range(m + 1):
        cost[0][j] = j
    for i in range(1, n + 1):
        for j in range(1, m + 1):
            cost[i][j] = min(cost[i - 1][j - 1] + (ref[i - 1] != hyp[j - 1]), cost[i - 1][j] + 1, cost[i][j - 1] + 1)
    pairs = []
    i, j = n, m
    while i > 0 or j > 0:
        if i > 0 and j > 0 and cost[i][j] == cost[i - 1][j - 1] + (ref[i - 1] != hyp[j - 1]):
            pairs.append((i - 1, j - 1))
            i, j = i - 1, j - 1
        elif i > 0 and cost[i][j] == cost[i - 1][j] + 1:
            pairs.append((i - 1, None))
            i -= 1
        else:
            pairs.append((None, j - 1))
            j -= 1
    return pairs[::-1]


def heard_as(reference: str, text: str, term: str) -> list[str]:
    """What `text` has where `reference` has `term`, once per occurrence that it doesn't spell exactly."""
    ref_words, hyp_words = words(reference), words(text)
    pairs = align([w for w, _, _ in ref_words], [w for w, _, _ in hyp_words])
    out = []
    for start, end in term_spans(reference, term):
        inside = {i for i, (_, s, e) in enumerate(ref_words) if s >= start and e <= end}
        positions = [k for k, (i, _) in enumerate(pairs) if i in inside]
        if not positions:
            continue
        # Words Whisper added next to the term are part of it: "G6 relay" for g6relay aligns one word, adds the other.
        first, last = positions[0], positions[-1]
        while first > 0 and pairs[first - 1][0] is None:
            first -= 1
        while last + 1 < len(pairs) and pairs[last + 1][0] is None:
            last += 1
        hyp = [j for i, j in pairs[first:last + 1] if j is not None]
        if hyp:
            out.append(text[hyp_words[hyp[0]][1]:hyp_words[hyp[-1]][2]].strip(".,;:!?\"'"))
    return out


def mine(chosen: list[Term], cases: list[dict]) -> dict[str, list[str]]:
    """sounds_like per term from the tune split's traces, leaving out spellings that would rewrite ordinary text.

    A soundalike gets none: Whisper writes the ordinary word it sounds like, which must stay as it is. A leading
    article is dropped, and a spelling made only of common words is left out: it's ordinary text.
    """
    common = common_words()
    stress_path = EVALS / "stress" / "en.txt"
    stress = stress_path.read_text(encoding="utf-8").casefold() if stress_path.exists() else ""
    texts = {t.text.casefold() for t in chosen}
    by_text = {t.text: t for t in chosen}
    mined: dict[str, list[str]] = {t.text: [] for t in chosen}
    for case in sorted(cases, key=lambda c: c["id"]):
        if case.get("split") != "tune":
            continue
        for key in ("engine_output", "engine_output_raw"):
            if key not in case:
                continue
            text = trace_text(case[key])
            for name in case["terms"]:
                term = by_text.get(name)
                if term is None or term.cls == "soundalike":
                    continue
                for spelling in heard_as(nfc(case["reference"]), text, nfc(name)):
                    spelling = re.sub(r"^(?:a|an|the)\s+(?=\S)", "", spelling, flags=re.IGNORECASE)
                    folded = spelling.casefold()
                    if " " in folded and all(word in common for word in folded.split()):
                        continue
                    if (not spelling or folded in texts or folded in common or review_key(folded) in NOT_SOUNDS_LIKE
                            or folded in {m.casefold() for m in mined[name]}
                            or re.search(r"(?<![\w'’-])" + re.escape(folded) + r"(?![\w'’-])", stress)):
                        continue
                    mined[name].append(spelling)
    return mined


def write_dictionary(chosen: list[Term], mined: dict[str, list[str]], path: Path = EVALS / "dictionary.toml") -> None:
    pinned = set()
    for cls in CLASSES:
        first = next(t for t in chosen if t.cls == cls)
        pinned.add(first.text)
    lines = [
        "# The eval set's dictionary, written by scripts/lib/eval_terms.py from evals/terms.toml: don't edit it by",
        "# hand.",
        "#",
        "# Every term is invented, or in the `real` class a real place or science name, so the eval set names no",
        "# product or company. `notes` is the term's class (CLASSES in eval_terms.py). It's large on purpose: the bias",
        "# terms (pinned first, then the rest alphabetically) overflow the 223-token budget for style plus terms, so",
        "# evals exercise dropping terms from the end. The first term of each class is pinned.",
        "#",
        "# sounds_like holds what Whisper wrote for a term in the tune split's traces, never the holdout split's, so",
        "# holdout measures the dictionary on errors it wasn't tuned on. Spellings that are common words, occur in",
        "# evals/stress/, or are the ordinary word a soundalike respells are left out: as exact spellings they would",
        "# rewrite every ordinary use of the word.",
        "#",
        "# The format is the server's dictionary.toml. Defaults: sounds_like = [], languages = \"any\", fuzzy = true,",
        "# pin = false. Changing the terms or pins means re-running the audio eval; sounds_like doesn't.",
    ]
    for t in sorted(chosen, key=lambda t: (t.text not in pinned, list(CLASSES).index(t.cls))):
        lines += ["", "[[terms]]", f"text = {toml_str(t.text)}"]
        if mined.get(t.text):
            lines.append("sounds_like = [" + ", ".join(toml_str(s) for s in mined[t.text]) + "]")
        if t.text in pinned:
            lines.append("pin = true")
        note = f"soundalike of “{t.spoken}”" if t.cls == "soundalike" else t.cls
        lines.append(f"notes = {toml_str(note)}")
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def cmd_dictionary() -> int:
    chosen = select(read_terms())
    cases = [c for c in read_cases(EVALS / "cases.jsonl") if "engine_output" in c]
    mined = mine(chosen, cases)
    write_dictionary(chosen, mined)
    print(f"wrote {len(chosen)} terms, {sum(map(len, mined.values()))} sounds_like from "
          f"{sum(c['split'] == 'tune' for c in cases)} tune cases", file=sys.stderr)
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = ap.add_subparsers(dest="command", required=True)
    sub.add_parser("generate", help="write evals/terms.toml with fresh candidates")
    probe = sub.add_parser("probe", help="write a probe eval set of every candidate to DIR/evals")
    probe.add_argument("dir", type=Path)
    heard = sub.add_parser("heard", help="record the probe's results in evals/terms.toml")
    heard.add_argument("dir", type=Path)
    sub.add_parser("dictionary", help="write evals/dictionary.toml, with sounds_like from the tune split")
    args = ap.parse_args()
    match args.command:
        case "generate":
            return cmd_generate()
        case "probe":
            return cmd_probe(args.dir)
        case "heard":
            return cmd_heard(args.dir)
        case "dictionary":
            return cmd_dictionary()
    return 2


if __name__ == "__main__":
    sys.exit(main())
