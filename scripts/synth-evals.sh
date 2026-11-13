#!/usr/bin/env bash
# Render the synthetic eval clips with Piper text-to-speech.
#
#   scripts/synth-evals.sh [--force] [--check]
#
# Reads evals/sentences/{en,cs}.txt and evals/dictionary.toml, writes evals/clips/syn-*.flac (16 kHz mono) and the
# synthetic lines of evals/cases.jsonl. Clips whose inputs haven't changed are left alone, so a re-run is cheap and,
# because rendering is seeded, a --force re-run produces the same bytes. --check only validates the sentences and
# prints the plan. Voices and licences: evals/SOURCES.md. The logic is in scripts/lib/synth_evals.py.
#
# Tools come from the flake's pinned nixpkgs: piper-tts 1.8.0, flac and Python with onnxruntime.
# Voices are downloaded once (checked by sha256) into ${PSST_CACHE_DIR:-~/.cache/psst}/piper.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
nixpkgs=github:NixOS/nixpkgs/1d060086d60cb76a40cf37109f979d88a5bd6396

if [[ -z ${PSST_SYNTH_SHELL:-} ]]; then
    # shellcheck disable=SC2016 # ${builtins.currentSystem} is Nix, not shell
    exec nix shell --impure --expr '
      let pkgs = (builtins.getFlake "'"$nixpkgs"'").legacyPackages.${builtins.currentSystem};
      in {
        inherit (pkgs) flac piper-tts;
        python = pkgs.python3.withPackages (ps: [ ps.numpy ps.onnxruntime ps.soxr ]);
      }' flac piper-tts python --command env PSST_SYNTH_SHELL=1 "$0" "$@"
fi

piper_root=$(dirname "$(dirname "$(readlink -f "$(command -v piper)")")")
piper_site=$(echo "$piper_root"/lib/python3*/site-packages)
exec python -I "$repo/scripts/lib/synth_evals.py" --repo "$repo" \
    --cache "${PSST_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/psst}" --piper-site "$piper_site" "$@"
