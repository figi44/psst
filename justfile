_default:
    @just --list

# Everything CI checks, in CI's order.
check: fmt-check clippy test pytest golden evals flake

# Formatting (`just fix` applies it).
fmt-check:
    cargo fmt --check

# Lints on every target, warnings as errors.
clippy:
    cargo clippy --workspace --all-targets --locked -- -D warnings

# The whole suite. Arguments go to nextest: `just test -E 'package(psst-dict)'`.
[positional-arguments]
test *args:
    cargo nextest run --workspace --locked "$@"

# The engine's tests, which also write the Python half of the golden frames.
pytest:
    cd engines/faster-whisper && pytest

# The golden frames `test` and `pytest` wrote are the committed ones.
golden:
    git diff --exit-code -- engines/faster-whisper/tests/golden

# Replay and stress against the committed evals/baseline.json; CI also gates against main's.
evals:
    cargo run -q -p psst-eval --locked -- replay --check
    cargo run -q -p psst-eval --locked -- stress --check

# The flake evaluates, as CI's Nix job checks it.
flake:
    nix flake check --no-build

# Apply rustfmt and clippy's suggestions.
fix:
    cargo fmt
    cargo clippy --workspace --all-targets --locked --fix --allow-dirty --allow-staged -- -D warnings
