#!/usr/bin/env python3
"""Derives the common-word lists in common-words/ (plan 7.2) and the stress-test sentences in
evals/stress/ (plan 7.3) from Common Voice's CC0 sentences.

    python3 -I derive-common-words.py <download dir>

Downloads the files listed in SOURCES at COMMIT into <download dir> (once), checks their SHA-256,
and rewrites common-words/<lang>.txt next to this script and evals/stress/<lang>.txt.
evals/SOURCES.md records the same files and commit.

Sentences are split in two halves by a hash of their text: split A feeds the lists, and split B
the stress test, so the two never share a sentence. A sentence is one line, stripped and
NFC-normalised; duplicates count once. Its split is A when the first byte of the SHA-256 of its
UTF-8 text is even, and B when it's odd.

Word forms are runs of letters, joined by inner apostrophes (`don't`), after NFC, case folding and
mapping typographic apostrophes to `'`. Digits, hyphens and other punctuation separate words. The
list holds the 5,000 most frequent forms, by count and then by code point order.

The stress sentences are split B's sentences in the order of their SHA-256 (hex), taken until the
language has STRESS_WORDS whitespace-separated words, leaving out any that starts with `#` (a
comment in evals/stress/).
"""

import hashlib
import re
import sys
import unicodedata
import urllib.request
from collections import Counter
from pathlib import Path

COMMIT = "3cf5dd0f81555dced52fa5a1b85f3956cdfe0c76"
BASE = f"https://raw.githubusercontent.com/common-voice/common-voice/{COMMIT}/server/data"
SIZE = 5000
# Words per language in evals/stress/: about 50,000 in all.
STRESS_WORDS = {"en": 30_000, "cs": 20_000}

# Every file in server/data/en at COMMIT but its LICENSE; for Czech, sentence-collector.txt and
# wiki.cs.txt only (never europarl-v7-cs.txt). Each with its SHA-256 at COMMIT.
SOURCES = {
    "en": {
        "DNGros.txt": "d21c9183327c2a21fabef0add2f366ea4e63d19e823e3728b7ea1a2465fd9673",
        "DictionaryofForeignPhrases.txt": "8655860354c84d5efac32b7b836c6d71b033591f268f17ffca2521c51da8f9a5",
        "RLarissa.txt": "b42ce85ad34dfa5e504c02b3eb703248b973e7f94223cb70b560a50e4b76aa15",
        "activate.txt": "6d438d4d32a55bd2fba3dcd83be560e2413de150d62d6ecda653c82fb7757793",
        "ajaydee.txt": "5c76e5c6baab9e7546b62f0ca2bf7987bfbad73c2abda899627fa47c35c56295",
        "bavencope.txt": "b9a6e84d4bf2d1c5069562a92368d5ad13bd3c4fb4a08b4b0ffbd4c3de0a61a7",
        "elleo.txt": "5de742690b7e389bb1b0388466ef44dcac84f5d0ccf194244309e8bff5d06fa2",
        "est31.txt": "7278f16bba6fafb036687722c80044385332a75acbf41a16c946c1a5226203b9",
        "fxvoice-cues.txt": "263a3d6c656e293297f42f0d4f3f62e6e195e90025ee0d6e524ad7ec658dd4df",
        "harvsents.txt": "6d11a74e46a93b9463c25843d5a1f33ffcdf299a721b35b01f32dd8dc3bea657",
        "james_fortune.txt": "1803a9335ab9663d8f856b869faba61584191c87fbce5825070874ebbb5ec94f",
        "jf99.txt": "0c2bcb8ad553d1f956b36a276a1f76749f4da808175fb375b8ce91af7e23933c",
        "kieran_drew.txt": "062e6a5a30ed5475aafbe7945d41e5ee88aa5b26c52f7a9130156bbebbdd1137",
        "mlennox.txt": "8e397def2948b9701994213f4583d1a8ae974b2fe17b6a5debdeb0aa31049609",
        "pro.gadget.txt": "1484fd31ae31627e774659004aed18c9e31a7098e66d8826bd1d687935028b7f",
        "reinhart1010.txt": "6a085a2f3058b178f86bf1e099e2b4364525eaeb484134dc0895aa951edae5bf",
        "sentence-collector.txt": "31ac8e200449ebd7aee4a8c6bd16d4dafd076cf11f7c3efe1035f0283712d136",
        "singleword-benchmark.txt": "6e9ed6cf31d9e1b88aeaef9714ac89d23de04fc8f02cf58d19bd021bbcaa996a",
        "thehowl.txt": "85ad5098e647641471d0fd19e28cb874847af2466ec569ebcfa46b06257b644b",
        "tlcoles.txt": "014f7795a2b8ca5ea6877c5d970de612b7426f4ad8043e44ea6d0c9923a4bcae",
        "tww.id.au.proverbs.txt": "630df2a5d0e7c971086871135d27d6e66263635af96225c1189ca00f03406239",
        "wiki.en.rerun-2021-11.txt": "16647e64ba4124ccfeb7061e88ffb53261a16a15b7c4abcd2122b168b9f29639",
        "wiki.en.txt": "438e55e8bc91dfa6cbe098a7233702c6b177c5ae07137a01d66b9c75c9789ae5",
    },
    "cs": {
        "sentence-collector.txt": "67bbf84823641f367082ab46dcafabf8e548f6fe20f50ab32dc2202c593a0b99",
        "wiki.cs.txt": "a3c1131438133a5e30c32b9f0df7e1b37503f1b96a303be48b7971ac0c733cce",
    },
}

WORD = re.compile(r"[^\W\d_]+(?:'[^\W\d_]+)*")
APOSTROPHES = str.maketrans({"’": "'", "‘": "'", "ʼ": "'"})


def fetch(lang: str, name: str, digest: str, cache: Path) -> Path:
    path = cache / lang / name
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        part = path.with_suffix(path.suffix + ".part")
        with urllib.request.urlopen(f"{BASE}/{lang}/{name}") as response, open(part, "wb") as out:
            while chunk := response.read(1 << 20):
                out.write(chunk)
        part.rename(path)
    actual = hashlib.sha256(path.read_bytes()).hexdigest()
    if actual != digest:
        sys.exit(f"{lang}/{name}: SHA-256 {actual}, expected {digest}")
    print(f"{lang}/{name}\t{actual}", file=sys.stderr)
    return path


def digest(sentence: str) -> bytes:
    return hashlib.sha256(sentence.encode()).digest()


def in_split_a(sentence: str) -> bool:
    return digest(sentence)[0] % 2 == 0


def words(sentence: str) -> list[str]:
    folded = unicodedata.normalize("NFC", sentence.casefold().translate(APOSTROPHES))
    return WORD.findall(folded)


def sentences(lang: str, cache: Path) -> set[str]:
    found: set[str] = set()
    for name, sha in SOURCES[lang].items():
        with open(fetch(lang, name, sha, cache), encoding="utf-8") as file:
            for line in file:
                sentence = unicodedata.normalize("NFC", line.strip())
                if sentence:
                    found.add(sentence)
    return found


def common_words(lang: str, all_sentences: set[str]) -> list[str]:
    counts: Counter[str] = Counter()
    split_a = 0
    for sentence in all_sentences:
        if in_split_a(sentence):
            split_a += 1
            counts.update(words(sentence))
    print(f"{lang}: {len(all_sentences)} sentences, {split_a} in split A", file=sys.stderr)
    ranked = sorted(counts.items(), key=lambda item: (-item[1], item[0]))
    return [word for word, _ in ranked[:SIZE]]


def stress_sentences(lang: str, all_sentences: set[str]) -> tuple[list[str], int]:
    split_b = sorted(
        (digest(s).hex(), s) for s in all_sentences if not in_split_a(s) and not s.startswith("#")
    )
    chosen: list[str] = []
    total = 0
    for _, sentence in split_b:
        if total >= STRESS_WORDS[lang]:
            break
        chosen.append(sentence)
        total += len(sentence.split())
    print(f"{lang}: {len(chosen)} stress sentences, {total} words", file=sys.stderr)
    return chosen, total


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    cache = Path(sys.argv[1])
    out = Path(__file__).resolve().parent / "common-words"
    out.mkdir(exist_ok=True)
    stress = Path(__file__).resolve().parents[3] / "evals" / "stress"
    stress.mkdir(exist_ok=True)
    for lang in SOURCES:
        all_sentences = sentences(lang, cache)
        ranked = common_words(lang, all_sentences)
        header = (
            f"# The {len(ranked):,} most frequent word forms in Common Voice's CC0 {lang} sentences "
            f"(split A),\n# most frequent first. Generated by ../derive-common-words.py from commit "
            f"{COMMIT};\n# see evals/SOURCES.md.\n"
        )
        (out / f"{lang}.txt").write_text(header + "\n".join(ranked) + "\n", encoding="utf-8")

        chosen, total = stress_sentences(lang, all_sentences)
        header = (
            f"# {len(chosen):,} sentences ({total:,} words) from Common Voice's CC0 {lang} sentences "
            f"(split B, never in the\n# common-word lists), for psst-eval stress. Generated by "
            f"crates/psst-dict/data/derive-common-words.py\n# from commit {COMMIT}; "
            f"see evals/SOURCES.md.\n"
        )
        (stress / f"{lang}.txt").write_text(header + "\n".join(chosen) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
