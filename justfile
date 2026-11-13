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

# The debug APK and Android lint, in `nix develop .#android`. Arguments go to Gradle: `just android -Ppsst.abis=arm64-v8a`.
[positional-arguments]
android *args:
    gradle -p android :app:assembleDebug :app:lintDebug "$@"

# The release APK, signed when PSST_KEYSTORE and PSST_KEYSTORE_PASSWORD are set, in `nix develop .#android`.
[positional-arguments]
android-release *args:
    gradle -p android :app:assembleRelease "$@"

# Install the debug APK on the connected device or emulator, in `nix develop .#android`.
android-install:
    gradle -p android :app:installDebug
