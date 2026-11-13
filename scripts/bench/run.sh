#!/usr/bin/env bash
# Run the Phase 0 engine benchmarks (plan 13) inside scripts/gpu-session.sh.
#
#   scripts/bench/run.sh all                       quality, prompt, long-terms, latency and batch
#   scripts/bench/run.sh <experiment> [options]    one experiment; options go to engine.py
#   scripts/bench/run.sh report                    tables from the results so far
#
# Results go to $PSST_BENCH_OUT (default: ~/.cache/psst/bench/<date>), one <experiment>.jsonl each, and
# report.md. The model is cached in ~/.cache/psst/hf.
#
# Phase 0 has no engine package yet, so this uses localwhisper's Python environment (faster-whisper 1.2.1,
# ctranslate2 4.8.4 with CUDA; plan 2) and the CUDA library path from its wrapper (plan 11, "Fallback"). Override
# with PSST_BENCH_PYTHON and PSST_BENCH_CUDA_LIBS.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
cache=${PSST_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/psst}
python=${PSST_BENCH_PYTHON:-/nix/store/ph6w4fnslmcbzn78808ykdyy84m2h08y-python3-3.14.8-env/bin/python}
cuda_libs=${PSST_BENCH_CUDA_LIBS:-/run/opengl-driver/lib:/nix/store/lc13g3zl4s8a6kwrsnhwrrcl8nh0lhgr-cuda13.4-libcublas-13.8.2.12-lib/lib:/nix/store/r349f0nfkzbq089wswnv23yhvakp415v-cuda13.4-cudnn-9.27.0.42-lib/lib}
export PSST_BENCH_OUT=${PSST_BENCH_OUT:-$cache/bench/$(date +%Y%m%d)}
# Always the psst cache, even if HF_HOME or HF_HUB_CACHE is set in the environment.
export HF_HOME=$cache/hf HF_HUB_CACHE=$cache/hf/hub HF_HUB_DISABLE_TELEMETRY=1
export LD_LIBRARY_PATH=$cuda_libs${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}

if (($# == 0)); then
    sed -n '4,6s/^# *//p' "$0" >&2
    exit 64
fi
mkdir -p "$PSST_BENCH_OUT"

if [[ $1 == report ]]; then
    exec "$python" -I "$here/report.py" "$PSST_BENCH_OUT"
fi

if [[ -z ${PSST_GPU_SESSION:-} ]]; then
    exec "$repo/scripts/gpu-session.sh" "$0" "$@"
fi

engine() { "$python" -I "$here/engine.py" "$@"; }

run_one() {
    local exp=$1
    shift
    echo "== $exp $* -> $PSST_BENCH_OUT/$exp.jsonl" >&2
    if [[ $exp == batch ]]; then
        # One process per configuration, so each peak is that configuration's own.
        for config in sequential batched-1 batched-4 batched-8 batched-16; do
            engine batch --config "$config" --out "$PSST_BENCH_OUT/batch.jsonl" "$@"
        done
    else
        engine "$exp" --out "$PSST_BENCH_OUT/$exp.jsonl" "$@"
    fi
}

if [[ $1 == all ]]; then
    for exp in quality prompt long-terms latency batch; do run_one "$exp"; done
else
    run_one "$@"
fi
"$python" -I "$here/report.py" "$PSST_BENCH_OUT" >"$PSST_BENCH_OUT/report.md"
echo "report: $PSST_BENCH_OUT/report.md" >&2
