"""Render the synthetic eval clips with Piper and write evals/cases.jsonl (plan 7.3).

Run through scripts/synth-evals.sh, which provides Piper, Python and flac from the pinned nixpkgs.

What it does:
- Reads evals/sentences/{en,cs}.txt and checks them against evals/dictionary.toml.
- Downloads the Piper voices (pinned commit, checked by sha256) into the psst cache.
- Renders every entry with one voice, plus a few silence and noise cases, as 16 kHz mono FLAC in evals/clips/.
- Rewrites the synthetic lines of evals/cases.jsonl, and keeps every other line (the author's recordings).

Determinism: Piper adds random noise inside the ONNX model. Each segment seeds onnxruntime from the clip id before
its session is created, so the same inputs give byte-identical audio. Each clip carries a PSST_SYNTH tag (a hash of
everything that went into it); a clip whose tag matches is left alone, and its cases.jsonl line (with any engine
outputs added later) is kept as it is.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
import tempfile
import tomllib
import urllib.request
import wave
import zlib
from dataclasses import dataclass, field
from pathlib import Path

import numpy as np

SR = 16000
# Bump to re-render every clip when the rendering itself changes.
RENDER_VERSION = 1

PIPER_VOICES_REPO = "rhasspy/piper-voices"
PIPER_VOICES_REV = "e7229f7d33a10e145e109b886d661984201ac3c4"

# model stem -> (path in the voices repo, sha256 of .onnx, sha256 of .onnx.json). Licences are in evals/SOURCES.md.
MODELS = {
    "en_US-ljspeech-high": (
        "en/en_US/ljspeech/high",
        "5d4f08ba6a2a48c44592eed3ce56bf85e9de3dd4e20df90541ae68a8310c029a",
        "7e1f4634af596d83cca997fb7a931ba80b70f8a316a2655ee69c55365e0ace14",
    ),
    "en_US-norman-medium": (
        "en/en_US/norman/medium",
        "b9739443232a80a59c7d18810dd856899bf16a7964725f5ab81ea49b1351cb71",
        "6c2db7f558a4a8deb9fe822583c1c5105f6c4e834dd0f9de8ad17a888ee9fe1d",
    ),
    "en_GB-cori-high": (
        "en/en_GB/cori/high",
        "470b4dd634c98f8a4850d7626ffc3dfc90774628eeef6605a6dd8f88f30a5903",
        "9e7fb5b5671612c22f3c81cbe46c1ae87b031a4632bcb509e499dad6f1e2adec",
    ),
    "cs_CZ-jirka-medium": (
        "cs/cs_CZ/jirka/medium",
        "cbd5c900acacc8e8cbecd64347abb8de39c00a9d3104bed06fee92e4f319efc8",
        "fb38b1799b7354808227c065efa97b1ffa2b0cde59505babb56a36d35af9c637",
    ),
}

# voice tag (part of the clip id) -> (model stem, length_scale). Piper has one Czech voice, used at two speeds.
VOICES = {
    "lj": ("en_US-ljspeech-high", 1.0),
    "norman": ("en_US-norman-medium", 1.0),
    "cori": ("en_GB-cori-high", 1.0),
    "jirka-fast": ("cs_CZ-jirka-medium", 0.9),
    "jirka-slow": ("cs_CZ-jirka-medium", 1.15),
}
EN_VOICES = ["lj", "norman", "cori"]

LEAD_IN_S = 0.3  # silence before speech, as if the recording started a moment before speaking
TAIL_S = 0.5  # silence after speech
LONG_TAIL_S = 4.0  # the silent tail of long must-pass clips (plan 7.3)
SENTENCE_GAP_S = 0.2  # between sentences inside one segment
PEAK = 10 ** (-1 / 20)  # normalise speech to -1 dBFS peak

ENTRY_ID = re.compile(r"^(en|cs)-(?:(long|short)-)?(\d+)$")
MARK = re.compile(r"\[\[([^\[\]|{}]+?)(?:\|([^\[\]|{}]+?))?\]\]")
PAUSE = re.compile(r"\{pause (\d+(?:\.\d+)?)\}")


class InputError(Exception):
    pass


@dataclass
class Entry:
    id: str
    lang: str
    kind: str  # "regular", "long" or "short"
    number: int
    text: str  # with markup
    source_line: str  # file:line, for errors

    @property
    def reference(self) -> str:
        return " ".join(PAUSE.sub(" ", MARK.sub(lambda m: m.group(1), self.text)).split())

    @property
    def terms(self) -> list[str]:
        return list(dict.fromkeys(m.group(1) for m in MARK.finditer(self.text)))

    def spoken_segments(self) -> list[tuple[str, float]]:
        """(text for Piper, pause after it in seconds) per segment."""
        spoken = MARK.sub(lambda m: m.group(2) or m.group(1), self.text)
        parts = PAUSE.split(spoken)
        texts, pauses = parts[0::2], [float(p) for p in parts[1::2]] + [0.0]
        return [(" ".join(t.split()), p) for t, p in zip(texts, pauses, strict=True)]


@dataclass
class Clip:
    id: str
    lang: str
    split: str
    reference: str
    terms: list[str]
    must_pass: bool
    voice: str | None = None  # None for noise cases
    segments: list[tuple[str, float]] = field(default_factory=list)
    noise: str | None = None
    noise_seconds: float = 0.0

    @property
    def seed(self) -> int:
        return zlib.crc32(self.id.encode()) & 0x7FFFFFFF

    def case(self) -> dict:
        return {
            "id": self.id,
            "lang": self.lang,
            "source": "synthetic",
            "split": self.split,
            "reference": self.reference,
            "terms": self.terms,
            "must_pass": self.must_pass,
        }

    def render_key(self) -> str:
        if self.voice:
            model, length_scale = VOICES[self.voice]
            what = {"model": MODELS[model][1], "length_scale": length_scale, "segments": self.segments,
                    "tail": LONG_TAIL_S if self.id.split("-")[2] == "long" else TAIL_S}
        else:
            what = {"noise": self.noise, "seconds": self.noise_seconds}
        what |= {"version": RENDER_VERSION, "seed": self.seed}
        return hashlib.sha256(json.dumps(what, sort_keys=True).encode()).hexdigest()[:16]


def parse_sentences(path: Path) -> list[Entry]:
    entries: list[Entry] = []
    for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        where = f"{path}:{lineno}"
        if not line.strip() or line.startswith("#"):
            continue
        if line[0].isspace():
            if not entries:
                raise InputError(f"{where}: continuation line before the first entry")
            entries[-1].text += " " + line.strip()
            continue
        entry_id, _, text = line.partition(" ")
        m = ENTRY_ID.match(entry_id)
        if not m or not text.strip():
            raise InputError(f"{where}: expected '<id> <text>' with an id like en-001, en-long-01 or cs-short-01")
        entries.append(Entry(entry_id, m.group(1), m.group(2) or "regular", int(m.group(3)), text.strip(), where))
    return entries


def check_entries(entries: list[Entry], dictionary: dict[str, str]) -> None:
    seen: set[str] = set()
    for e in entries:
        if e.id in seen:
            raise InputError(f"{e.source_line}: duplicate id {e.id}")
        seen.add(e.id)
        leftover = PAUSE.sub("", MARK.sub("", e.text))
        if any(c in leftover for c in "[]{}|"):
            raise InputError(f"{e.source_line}: {e.id}: unbalanced or unknown markup in {e.text!r}")
        pauses = PAUSE.findall(e.text)
        if e.kind == "long" and not pauses:
            raise InputError(f"{e.source_line}: {e.id}: long entries need pauses")
        if e.kind != "long" and pauses:
            raise InputError(f"{e.source_line}: {e.id}: only long entries have pauses")
        for term in e.terms:
            langs = dictionary.get(term)
            if langs is None:
                raise InputError(f"{e.source_line}: {e.id}: [[{term}]] isn't in evals/dictionary.toml")
            if langs not in ("any", e.lang):
                raise InputError(f"{e.source_line}: {e.id}: [[{term}]] isn't valid for {e.lang}")


def load_dictionary(path: Path) -> dict[str, str]:
    """Term text -> its `languages`: "any" or one language code (docs/config.md)."""
    terms = tomllib.loads(path.read_text(encoding="utf-8"))["terms"]
    return {t["text"]: t.get("languages", "any") for t in terms}


def voice_for(e: Entry) -> str:
    if e.lang == "en":
        return EN_VOICES[(e.number - 1) % len(EN_VOICES)]
    # Czech: blocks of four ordinary sentences (both splits, with and without terms) alternate between the speeds.
    fast = ((e.number - 1) // 4) % 2 == 0 if e.kind == "regular" else e.number % 2 == 1
    return "jirka-fast" if fast else "jirka-slow"


# Silence and noise cases: the guard must leave them empty (plan 6.4). (id suffix, kind, seconds)
NOISE_CASES = {
    "en": [("silence", "silence", 5.0), ("roomtone", "roomtone", 8.0), ("fan", "fan", 6.0),
           ("white", "white", 4.0), ("hum", "hum", 6.0), ("typing", "typing", 6.0)],
    "cs": [("silence", "silence", 5.0), ("roomtone", "roomtone", 8.0), ("typing", "typing", 6.0)],
}


def plan_clips(entries: list[Entry]) -> list[Clip]:
    clips = []
    for e in entries:
        voice = voice_for(e)
        clips.append(Clip(
            id=f"syn-{e.id}-{voice}", lang=e.lang, split="tune" if e.number % 2 else "holdout",
            reference=e.reference, terms=e.terms, must_pass=e.kind != "regular", voice=voice,
            segments=e.spoken_segments(),
        ))
    for lang, cases in NOISE_CASES.items():
        for i, (name, kind, seconds) in enumerate(cases):
            clips.append(Clip(
                id=f"syn-{lang}-{name}", lang=lang, split="tune" if i % 2 == 0 else "holdout", reference="",
                terms=[], must_pass=True, noise=kind, noise_seconds=seconds,
            ))
    return clips


# --- audio ----------------------------------------------------------------------------------------------------------


def fetch_voice(cache: Path, model: str) -> Path:
    path_in_repo, onnx_sha, json_sha = MODELS[model]
    out = cache / "piper" / PIPER_VOICES_REV
    out.mkdir(parents=True, exist_ok=True)
    for suffix, sha in ((".onnx", onnx_sha), (".onnx.json", json_sha)):
        dest = out / f"{model}{suffix}"
        if not dest.exists() or sha256_file(dest) != sha:
            url = f"https://huggingface.co/{PIPER_VOICES_REPO}/resolve/{PIPER_VOICES_REV}/{path_in_repo}/{model}{suffix}"
            print(f"downloading {url}", file=sys.stderr)
            tmp = dest.with_suffix(dest.suffix + ".part")
            with urllib.request.urlopen(url) as r, open(tmp, "wb") as f:
                while chunk := r.read(1 << 20):
                    f.write(chunk)
            if sha256_file(tmp) != sha:
                tmp.unlink()
                raise SystemExit(f"error: {url} doesn't match its pinned sha256")
            tmp.rename(dest)
    return out / f"{model}.onnx"


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(1 << 20):
            h.update(chunk)
    return h.hexdigest()


def silence(seconds: float) -> np.ndarray:
    return np.zeros(int(round(seconds * SR)), dtype=np.float32)


def render_speech(clip: Clip, model_path: Path) -> np.ndarray:
    import onnxruntime
    import soxr
    from piper import PiperVoice, SynthesisConfig

    _, length_scale = VOICES[clip.voice]
    pieces = [silence(LEAD_IN_S)]
    for i, (text, pause) in enumerate(clip.segments):
        # The seed is read when the session's random kernels are created, so each segment gets a fresh session:
        # the audio then doesn't depend on what was rendered before it.
        onnxruntime.set_seed(clip.seed + i)
        voice = PiperVoice.load(model_path)
        chunks = []
        for chunk in voice.synthesize(text, syn_config=SynthesisConfig(length_scale=length_scale)):
            if chunks:
                chunks.append(np.zeros(int(SENTENCE_GAP_S * chunk.sample_rate), dtype=np.float32))
            chunks.append(chunk.audio_float_array.astype(np.float32))
            rate = chunk.sample_rate
        pieces.append(soxr.resample(np.concatenate(chunks), rate, SR, quality="VHQ").astype(np.float32))
        if pause:
            pieces.append(silence(pause))
    pieces.append(silence(LONG_TAIL_S if clip.id.split("-")[2] == "long" else TAIL_S))
    audio = np.concatenate(pieces)
    return audio * (PEAK / max(float(np.abs(audio).max()), 1e-9))


def dbfs(x: np.ndarray, level_db: float) -> np.ndarray:
    rms = float(np.sqrt(np.mean(x.astype(np.float64) ** 2))) or 1.0
    return (x * (10 ** (level_db / 20) / rms)).astype(np.float32)


def pink(rng: np.random.Generator, n: int) -> np.ndarray:
    spectrum = np.fft.rfft(rng.standard_normal(n))
    freqs = np.fft.rfftfreq(n, 1 / SR)
    spectrum[1:] /= np.sqrt(freqs[1:])
    spectrum[0] = 0
    return np.fft.irfft(spectrum, n).astype(np.float32)


def render_noise(clip: Clip) -> np.ndarray:
    rng = np.random.default_rng(clip.seed)
    n = int(clip.noise_seconds * SR)
    t = np.arange(n) / SR
    match clip.noise:
        case "silence":  # digital silence
            return np.zeros(n, dtype=np.float32)
        case "roomtone":  # a quiet room through a laptop mic
            return dbfs(pink(rng, n), -58)
        case "fan":  # a desk fan close by
            return dbfs(pink(rng, n), -32)
        case "white":  # broadband hiss, loud
            return dbfs(rng.standard_normal(n).astype(np.float32), -30)
        case "hum":  # 50 Hz mains hum with harmonics, over room tone
            hum = sum(np.sin(2 * np.pi * 50 * k * t) / k for k in (1, 2, 3, 5)).astype(np.float32)
            return dbfs(hum, -36) + dbfs(pink(rng, n), -58)
        case "typing":  # keyboard clicks, about six per second, over room tone
            out = dbfs(pink(rng, n), -58)
            click_len = int(0.012 * SR)
            envelope = np.exp(-np.arange(click_len) / (0.002 * SR)).astype(np.float32)
            for start in np.sort(rng.integers(0, n - click_len, int(clip.noise_seconds * 6))):
                click = rng.standard_normal(click_len).astype(np.float32) * envelope
                out[start:start + click_len] += click * 10 ** (-12 / 20) * float(rng.uniform(0.4, 1.0))
            return out
    raise ValueError(clip.noise)


def write_flac(audio: np.ndarray, dest: Path, tag: str) -> None:
    pcm = np.clip(np.round(audio * 32767), -32768, 32767).astype("<i2")
    with tempfile.TemporaryDirectory() as tmp:
        wav_path = Path(tmp) / "clip.wav"
        with wave.open(str(wav_path), "wb") as w:
            w.setnchannels(1)
            w.setsampwidth(2)
            w.setframerate(SR)
            w.writeframes(pcm.tobytes())
        subprocess.run(["flac", "--silent", "--force", "--best", "--no-padding", f"--tag=PSST_SYNTH={tag}",
                        "-o", str(dest), str(wav_path)], check=True)


def clip_tag(path: Path) -> str | None:
    if not path.exists():
        return None
    out = subprocess.run(["metaflac", "--show-tag=PSST_SYNTH", str(path)], capture_output=True, text=True)
    return out.stdout.strip().partition("=")[2] or None


def flac_seconds(path: Path) -> float:
    out = subprocess.run(["metaflac", "--show-total-samples", "--show-sample-rate", str(path)],
                         capture_output=True, text=True, check=True).stdout.split()
    return int(out[0]) / int(out[1])


# --- main -----------------------------------------------------------------------------------------------------------


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--repo", type=Path, required=True)
    ap.add_argument("--cache", type=Path, required=True, help="the psst cache directory (voices go under piper/)")
    ap.add_argument("--piper-site", required=True, help="site-packages of the piper-tts package")
    ap.add_argument("--force", action="store_true", help="re-render every clip, even if its tag matches")
    ap.add_argument("--check", action="store_true", help="only check the sentences and list the plan")
    args = ap.parse_args()
    sys.path.insert(0, args.piper_site)

    evals = args.repo / "evals"
    try:
        dictionary = load_dictionary(evals / "dictionary.toml")
        entries = [e for lang in ("en", "cs") for e in parse_sentences(evals / "sentences" / f"{lang}.txt")]
        check_entries(entries, dictionary)
    except InputError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1
    clips = plan_clips(entries)

    if args.check:
        for c in clips:
            print(f"{c.id}\t{c.split}\t{'must_pass' if c.must_pass else '-'}\t{','.join(c.terms)}")
        return 0

    clip_dir = evals / "clips"
    clip_dir.mkdir(parents=True, exist_ok=True)
    rendered: set[str] = set()
    for i, clip in enumerate(clips, 1):
        dest = clip_dir / f"{clip.id}.flac"
        tag = clip.render_key()
        if not args.force and clip_tag(dest) == tag:
            continue
        print(f"[{i}/{len(clips)}] {clip.id}", file=sys.stderr)
        if clip.voice:
            audio = render_speech(clip, fetch_voice(args.cache, VOICES[clip.voice][0]))
        else:
            audio = render_noise(clip)
        write_flac(audio, dest, tag)
        rendered.add(clip.id)

    planned = {c.id for c in clips}
    for stale in sorted(clip_dir.glob("syn-*.flac")):
        if stale.stem not in planned:
            print(f"removing {stale}", file=sys.stderr)
            stale.unlink()

    # cases.jsonl: keep the author's lines, and a synthetic line as it is (engine outputs included) if its clip and fields
    # are unchanged; everything else is rewritten from the plan.
    cases_path = evals / "cases.jsonl"
    old = {}
    if cases_path.exists():
        for line in cases_path.read_text(encoding="utf-8").splitlines():
            if line.strip():
                case = json.loads(line)
                old[case["id"]] = case
    cases = {cid: c for cid, c in old.items() if c.get("source") != "synthetic"}
    for clip in clips:
        base = clip.case()
        prev = old.get(clip.id)
        keep = prev and clip.id not in rendered and all(prev.get(k) == v for k, v in base.items())
        cases[clip.id] = prev if keep else base
    with open(cases_path, "w", encoding="utf-8") as f:
        for cid in sorted(cases):
            f.write(json.dumps(cases[cid], ensure_ascii=False, separators=(",", ":")) + "\n")

    summarize(clips, clip_dir)
    return 0


def summarize(clips: list[Clip], clip_dir: Path) -> None:
    secs = {c.id: flac_seconds(clip_dir / f"{c.id}.flac") for c in clips}
    size = sum((clip_dir / f"{c.id}.flac").stat().st_size for c in clips)
    print(f"{len(clips)} synthetic clips, {sum(secs.values()) / 60:.1f} min, {size / 1e6:.1f} MB", file=sys.stderr)
    for lang in ("en", "cs"):
        mine = [c for c in clips if c.lang == lang]
        for split in ("tune", "holdout"):
            s = [c for c in mine if c.split == split]
            print(f"  {lang} {split:7}: {len(s):3} clips, {sum(bool(c.terms) for c in s):2} with terms, "
                  f"{sum(c.must_pass for c in s):2} must-pass", file=sys.stderr)
    longs = [secs[c.id] for c in clips if "-long-" in c.id]
    print(f"  long clips: {len(longs)}, {min(longs):.0f}–{max(longs):.0f} s", file=sys.stderr)


if __name__ == "__main__":
    sys.exit(main())
