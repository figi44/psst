#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
# The nixpkgs the committed clips were rendered with (piper-tts 1.8.0).
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
