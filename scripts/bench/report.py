from __future__ import annotations

import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import benchlib as bl  # noqa: E402

BIAS_NAMES = {None: "none", "prompt": "prompt", "hotwords": "hotwords"}


def load(path: Path) -> list[dict]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def ms(x: float) -> str:
    return f"{x * 1000:.0f}"


def pct(num: float, den: float) -> str:
    return f"{100 * num / den:.1f}" if den else "–"


def quantile(xs: list[float], q: float) -> float:
    xs = sorted(xs)
    if len(xs) == 1:
        return xs[0]
    return statistics.quantiles(xs, n=100, method="inclusive")[round(q * 100) - 1]


def table(header: list[str], rows: list[list]) -> str:
    lines = ["| " + " | ".join(header) + " |", "|" + "---|" * len(header)]
    lines += ["| " + " | ".join(str(c) for c in row) + " |" for row in rows]
    return "\n".join(lines)


def bias_terms_by(records) -> dict:
    """The full prompt's terms, for the leak checks of every bias setting."""
    return {(r["lang"], r["path"]): r["terms"] for r in records
            if r["kind"] == "bias" and r["terms_as"] == "prompt"}


def cases_by_id() -> dict:
    return {c["id"]: c for c in map(json.loads, (bl.EVALS / "cases.jsonl").read_text(encoding="utf-8").splitlines())}


def report_quality(records, cases) -> str:
    runs = [r for r in records if r["kind"] == "quality"]
    if not runs:
        return ""
    terms_by = bias_terms_by(records)
    groups = defaultdict(list)
    for r in runs:
        groups[(r["path"], r["terms_as"])].append(r)
    rows = []
    for (path, terms_as), rs in groups.items():
        cells = [path, BIAS_NAMES[terms_as]]
        for lang in ("en",):
            errs = words = 0
            for r in rs:
                c = cases[r["id"]]
                if c["reference"] and (lang is None or c["lang"] == lang):
                    e, n = bl.wer_counts(c["reference"], r["text"], c["lang"])
                    errs, words = errs + e, words + n
            cells.append(pct(errs, words))
        for lang in ("en",):
            found = total = 0
            for r in rs:
                c = cases[r["id"]]
                if c["lang"] == lang and c["terms"]:
                    f, t = bl.term_hits(c["reference"], r["text"], c["terms"])
                    found, total = found + f, total + t
            cells.append(pct(found, total))
        noise = [r for r in rs if not cases[r["id"]]["reference"]]
        cells.append(f"{sum(bool(r['text']) for r in noise)}/{len(noise)}")
        short = [r for r in rs if "-short-" in r["id"]]
        cells.append(f"{sum(bl.normalize(r['text'], r['lang']) == bl.normalize(cases[r['id']]['reference'], r['lang']) for r in short)}/{len(short)}")
        leaky = [r for r in rs if cases[r["id"]]["reference"]
                 and bl.leaks(cases[r["id"]]["reference"], r["text"], r["lang"], terms_by[(r["lang"], r["path"])])]
        cells.append(len(leaky))
        cells.append(ms(statistics.median(r["seconds"] for r in rs if 2 <= r["seconds_audio"] <= 5)))
        rows.append(cells)
    out = [
        "## Quality on all synthetic clips (one run each, word timestamps on)",
        "",
        table(["path", "bias", "WER %", "term recall %",
               "noise with text", "short must-pass exact", "clips with leaks", "median ms, 2–5 s"], rows),
        "",
    ]
    details = []
    for (path, terms_as), rs in groups.items():
        for r in rs:
            c = cases[r["id"]]
            if not c["reference"] and r["text"]:
                details.append(f"- {path}/{BIAS_NAMES[terms_as]} `{r['id']}` (noise): {r['text']!r}")
            elif c["reference"]:
                found = bl.leaks(c["reference"], r["text"], r["lang"], terms_by[(r["lang"], r["path"])])
                if found:
                    details.append(f"- {path}/{BIAS_NAMES[terms_as]} `{r['id']}` leaks {found}: {r['text']!r}")
            if "-short-" in r["id"] and bl.normalize(r["text"], r["lang"]) != bl.normalize(c["reference"], r["lang"]):
                details.append(f"- {path}/{BIAS_NAMES[terms_as]} `{r['id']}` (short) → {r['text']!r}")
    if details:
        out += ["Outputs with text on noise, leaks, or a short clip not matching:", ""] + details + [""]
    return "\n".join(out)


def report_prompt(records, cases) -> str:
    runs = [r for r in records if r["kind"] == "prompt"]
    if not runs:
        return ""
    terms_by = bias_terms_by(records)
    per = defaultdict(lambda: defaultdict(list))
    first = {}
    for r in runs:
        per[(r["path"], r["terms_as"])][r["id"]].append(r["seconds"])
        if r["run"] == 0:
            first[(r["path"], r["terms_as"], r["id"])] = r
    rows = []
    base = {}
    for (path, terms_as), by_clip in sorted(per.items(), key=lambda kv: (kv[0][0], str(kv[0][1]))):
        medians = [statistics.median(v) for v in by_clip.values()]
        m = statistics.median(medians)
        if terms_as is None:
            base[path] = m
        firsts = [first[(path, terms_as, cid)] for cid in by_clip]
        leaky = sum(bool(bl.leaks(cases[r["id"]]["reference"], r["text"], r["lang"], terms_by[(r["lang"], path)]))
                    for r in firsts)
        errs = sum(bl.wer_counts(cases[r["id"]]["reference"], r["text"], r["lang"])[0] for r in firsts)
        words = sum(bl.wer_counts(cases[r["id"]]["reference"], r["text"], r["lang"])[1] for r in firsts)
        rows.append([path, "full 223-token prompt" if terms_as else "none", len(by_clip), ms(m),
                     ms(quantile(medians, 0.95)), "" if terms_as is None else f"+{ms(m - base.get(path, m))}",
                     leaky, pct(errs, words)])
    return "\n".join([
        "## No prompt vs the full bias prompt on 2–5 s clips (median of 5 runs per clip)", "",
        table(["path", "prompt", "clips", "median ms", "p95 ms (over clips)", "added", "clips with leaks", "WER %"],
              rows), ""])


def report_long_terms(records) -> str:
    runs = [r for r in records if r["kind"] == "long-terms"]
    if not runs:
        return ""
    comps = {r["name"]: r for r in records if r["kind"] == "composite" and r["name"] in {x["id"] for x in runs}}
    groups = defaultdict(list)
    for r in runs:
        groups[(r["id"], r["path"], r["terms_as"])].append(r)
    rows = []
    for (cid, path, terms_as), rs in groups.items():
        comp = comps[cid]
        text = next(r["text"] for r in rs if r["run"] == 0)
        starts = defaultdict(list)
        for term, start in comp["occurrences"]:
            starts[term].append(start)
        cells = [cid, path, BIAS_NAMES[terms_as]]
        for select in (lambda s: True, lambda s: all(x < 30 for x in s), lambda s: all(x >= 30 for x in s)):
            terms = [t for t, s in starts.items() if select(s)]
            f, t = bl.term_hits(comp["reference"], text, terms)
            cells.append(f"{pct(f, t)} ({f}/{t})")
        e, n = bl.wer_counts(comp["reference"], text, comp["lang"])
        cells += [pct(e, n), ms(statistics.median(r["seconds"] for r in rs))]
        rows.append(cells)
    return "\n".join([
        "## Terms as prompt vs hotwords on 60 and 180 s (3 runs; recall from the first)", "",
        "Early terms occur only in sentences starting before 30 s, late terms only in sentences starting after it.", "",
        table(["audio", "path", "bias", "term recall %", "early terms %", "late terms %", "WER %", "median ms"], rows),
        ""])


def report_latency(records) -> str:
    runs = [r for r in records if r["kind"] == "latency"]
    if not runs:
        return ""
    comps = {r["name"]: r for r in records if r["kind"] == "composite"}
    cell = defaultdict(list)
    text = {}
    for r in runs:
        cell[(r["target_s"], r["path"], r["word_timestamps"])].append(r["seconds"])
        if r["run"] == 0:
            text[(r["target_s"], r["path"], r["word_timestamps"])] = (r["id"], r["text"])
    rows = []
    for target in sorted({k[0] for k in cell}):
        row = [f"{target} s"]
        for path in ("sequential", "batched"):
            for wt in (False, True):
                xs = cell[(target, path, wt)]
                row.append(f"{ms(statistics.median(xs))} ({ms(min(xs))}–{ms(max(xs))})")
        for path in ("sequential", "batched"):
            cid, out = text[(target, path, True)]
            e, n = bl.wer_counts(comps[cid]["reference"], out, "en")
            row.append(pct(e, n))
        rows.append(row)
    return "\n".join([
        "## Latency by length, path and word timestamps (ms, median and range of 5–7 runs; full bias prompt)", "",
        table(["audio", "sequential", "sequential + words", "batched (8)", "batched (8) + words", "WER seq %",
               "WER batched %"], rows), ""])


def report_batch(records) -> str:
    runs = [r for r in records if r["kind"] == "batch"]
    if not runs:
        return ""
    loaded = {r["config"]: r for r in records if r["kind"] == "batch-loaded"}
    peaks = {(r["config"], r["target_s"]): r for r in records if r["kind"] == "batch-peak"}
    rows = []
    for config in loaded:
        row = [config, loaded[config]["process_mib"]]
        for target in (30, 60, 180, 600):
            ok = [r["seconds"] for r in runs if r["config"] == config and r["target_s"] == target
                  and "error" not in r and r["run"] > 0]
            failed = any("error" in r for r in runs if r["config"] == config and r["target_s"] == target)
            p = peaks.get((config, target), {})
            if failed:
                row.append("infeasible (out of memory)")
            elif ok:
                row.append(f"{ms(statistics.median(ok))} ms, {p.get('process_mib', '?')} MiB")
            else:
                row.append("–")
        row.append(max((p["gpu_mib"] for (c, _), p in peaks.items() if c == config), default="?"))
        rows.append(row)
    return "\n".join([
        "## Batch size and VRAM (median of 3 runs after a warm-up; peak memory of the engine process)", "",
        table(["config", "after load MiB", "30 s", "60 s", "180 s", "600 s", "whole GPU peak MiB"], rows), ""])


def report_targets(records) -> str:
    runs = [r for r in records if r["kind"] == "targets"]
    if not runs:
        return ""
    config = runs[0]
    rows = []
    for group in ("<=15s", "60s"):
        for lang in ("en",):
            xs = [r["seconds"] for r in runs if r["group"] == group and (lang is None or r["lang"] == lang)]
            if xs:
                rows.append([group, lang or "all", len(xs), ms(quantile(xs, 0.5)), ms(quantile(xs, 0.95)),
                             ms(max(xs))])
    return "\n".join([
        f"## Engine time for the chosen configuration ({config['path']}, batch {config['batch_size']}, "
        f"word timestamps {'on' if config['word_timestamps'] else 'off'}, terms as {config['terms_as']})", "",
        table(["dictation", "lang", "runs", "p50 ms", "p95 ms", "max ms"], rows), ""])


def main():
    out = Path(sys.argv[1])
    records = {name: load(out / f"{name}.jsonl")
               for name in ("quality", "prompt", "long-terms", "latency", "batch", "targets")}
    cases = cases_by_id()
    loads = [r for rs in records.values() for r in rs if r["kind"] == "load"]
    parts = [f"# Engine benchmark results ({out.name})", ""]
    if loads:
        parts += [f"Model load: median {ms(statistics.median(r['seconds'] for r in loads))} ms over {len(loads)} "
                  f"loads; GPU memory in use after loading: {loads[0]['gpu']['used_mib']} of "
                  f"{loads[0]['gpu']['total_mib']} MiB.", ""]
    parts += [report_quality(records["quality"], cases), report_prompt(records["prompt"], cases),
              report_long_terms(records["long-terms"]), report_latency(records["latency"]),
              report_batch(records["batch"]), report_targets(records["targets"])]
    print("\n".join(p for p in parts if p))


if __name__ == "__main__":
    main()
