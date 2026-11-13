"""Engine benchmarks for faster-whisper. Run through scripts/bench/run.sh."""

from __future__ import annotations

import argparse
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import benchlib as bl  # noqa: E402

PATHS = ("sequential", "batched")
BIASES = (None, "prompt", "hotwords")


class Runner:
    def __init__(self, out, word_timestamps=True, batch_size=8):
        self.model, self.batched, load_s = bl.load_model()
        self.out = out
        self.word_timestamps = word_timestamps
        self.batch_size = batch_size
        self._bias = {}
        bl.write(out, {"kind": "load", "seconds": load_s, "gpu": bl.gpu_memory()})

    def bias(self, lang, path, terms_as):
        key = (lang, path, terms_as)
        if key not in self._bias:
            b = bl.build_bias(bl.tokenizer_for(self.model, lang), lang, path, terms_as)
            self._bias[key] = b
            bl.write(self.out, {"kind": "bias", "lang": lang, "path": path, "terms_as": terms_as,
                                "initial_prompt": b.initial_prompt, "hotwords": b.hotwords,
                                "terms_used": b.terms_used, "tokens": b.tokens,
                                "terms": bl.bias_terms(lang)[:b.terms_used]})
        return self._bias[key]

    def run(self, exp, audio, lang, path, terms_as, record, word_timestamps=None, batch_size=None):
        wt = self.word_timestamps if word_timestamps is None else word_timestamps
        bs = batch_size or self.batch_size
        r = bl.transcribe(self.model, self.batched, audio, lang=lang, path=path,
                          bias=self.bias(lang, path, terms_as), word_timestamps=wt, batch_size=bs)
        rec = {"kind": exp, "lang": lang, "path": path, "terms_as": terms_as, "word_timestamps": wt,
               "batch_size": bs if path == "batched" else None, **record, **r}
        bl.write(self.out, rec)
        return rec

    def warm_up(self, cases):
        for c in cases[:3]:
            for path in PATHS:
                self.run("warmup", c.audio, c.lang, path, "prompt", {"id": c.id})


def exp_quality(args, out):
    cases = bl.load_cases()
    r = Runner(out)
    r.warm_up(cases)
    for path in PATHS:
        for terms_as in BIASES:
            for c in cases:
                r.run("quality", c.audio, c.lang, path, terms_as, {"id": c.id, "seconds_audio": c.seconds})


def exp_prompt(args, out):
    cases = [c for c in bl.load_cases() if not c.is_noise and 2 <= c.seconds <= 5]
    r = Runner(out)
    r.warm_up(cases)
    for c in cases:
        for run in range(5):
            for path in PATHS:
                for terms_as in (None, "prompt"):
                    r.run("prompt", c.audio, c.lang, path, terms_as,
                          {"id": c.id, "run": run, "seconds_audio": c.seconds})


# Do terms fade after the first 30 s window?
def exp_long_terms(args, out):
    cases = bl.load_cases()
    r = Runner(out)
    r.warm_up(cases)
    for comp in bl.term_composites(cases):
        bl.write(out, {"kind": "composite", "name": comp.name, "lang": comp.lang, "reference": comp.reference,
                       "terms": comp.terms, "parts": comp.parts, "occurrences": comp.occurrences})
        for run in range(3):
            for path in PATHS:
                for terms_as in BIASES:
                    r.run("long-terms", comp.audio, comp.lang, path, terms_as, {"id": comp.name, "run": run})


def exp_latency(args, out):
    cases = bl.load_cases()
    r = Runner(out)
    r.warm_up(cases)
    for target, comp in bl.duration_composites(cases).items():
        bl.write(out, {"kind": "composite", "name": comp.name, "lang": comp.lang, "reference": comp.reference,
                       "terms": comp.terms, "parts": comp.parts})
        for run in range(7 if target <= 30 else 5):
            for path in PATHS:
                for wt in (False, True):
                    r.run("latency", comp.audio, "en", path, "prompt",
                          {"id": comp.name, "target_s": target, "run": run}, word_timestamps=wt)


def exp_batch(args, out):
    """One configuration per process, so the CUDA caching allocator's high-water mark is this configuration's."""
    path = "sequential" if args.config == "sequential" else "batched"
    bs = None if path == "sequential" else int(args.config.removeprefix("batched-"))
    cases = bl.load_cases()
    # 600 s is the default max_recording_s: about 24 VAD chunks, so batch size 16 really runs 16 at once.
    comps = bl.duration_composites(cases, targets=(30, 60, 180, 600))
    sampler = bl.VramSampler()
    try:
        r = Runner(out, batch_size=bs or 8)
        time.sleep(0.5)
        loaded = sampler.peak()
        bl.write(out, {"kind": "batch-loaded", "config": args.config, **loaded})
        sampler.reset()
        for target in (30, 60, 180, 600):
            comp = comps[target]
            for run in range(4):  # run 0 is a warm-up
                try:
                    rec = r.run("batch", comp.audio, "en", path, "prompt",
                                {"id": comp.name, "target_s": target, "run": run, "config": args.config})
                except RuntimeError as err:  # CUDA out of memory
                    bl.write(out, {"kind": "batch", "config": args.config, "target_s": target, "run": run,
                                   "error": str(err)[:300]})
                    continue
            bl.write(out, {"kind": "batch-peak", "config": args.config, "target_s": target, **sampler.peak()})
    finally:
        sampler.close()


def exp_targets(args, out):
    cases = [c for c in bl.load_cases() if not c.is_noise]
    r = Runner(out, word_timestamps=args.word_timestamps == "on", batch_size=args.batch_size)
    r.warm_up(cases)
    short = [c for c in cases if c.seconds <= 15]
    for run in range(3):
        for c in short:
            r.run("targets", c.audio, c.lang, args.path, args.terms_as,
                  {"id": c.id, "run": run, "seconds_audio": c.seconds, "group": "<=15s"})
    longs = [c for c in cases if "-long-" in c.id]
    fillers = [c for c in cases if "-long-" not in c.id]
    for lc in longs:
        pool = [lc] + [f for f in fillers if f.lang == lc.lang]
        if lc.seconds > 59.8:
            continue
        comp = bl.composite(f"{lc.id}-60s", pool, 60)
        bl.write(out, {"kind": "composite", "name": comp.name, "lang": comp.lang, "reference": comp.reference,
                       "terms": comp.terms, "parts": comp.parts})
        for run in range(5):
            r.run("targets", comp.audio, comp.lang, args.path, args.terms_as,
                  {"id": comp.name, "run": run, "seconds_audio": 60.0, "group": "60s"})


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("experiment", choices=["quality", "prompt", "long-terms", "latency", "batch", "targets"])
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--config", help="batch: sequential or batched-<size>")
    ap.add_argument("--path", choices=PATHS, default="batched")
    ap.add_argument("--batch-size", type=int, default=8)
    ap.add_argument("--word-timestamps", choices=["on", "off"], default="on")
    ap.add_argument("--terms-as", choices=["prompt", "hotwords"], default="prompt")
    args = ap.parse_args()
    fn = {"quality": exp_quality, "prompt": exp_prompt, "long-terms": exp_long_terms, "latency": exp_latency,
          "batch": exp_batch, "targets": exp_targets}[args.experiment]
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with open(args.out, "a", encoding="utf-8") as out:
        bl.write(out, {"kind": "start", "experiment": args.experiment, "argv": sys.argv[1:],
                       "time": time.strftime("%Y-%m-%dT%H:%M:%S%z")})
        fn(args, out)


if __name__ == "__main__":
    main()
