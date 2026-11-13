# Working on psst

psst is a dictation system: a Rust server that runs the speech model on a GPU machine, an engine
process per model, and a thin Linux desktop client. `docs/protocol.md` and `docs/config.md` are
the contracts everything else is built against.

## Hard rules

- **Never touch localwhisper or `/etc/nixos`.** Never stop, uninstall, reload or reconfigure
  localwhisper, and never edit anything under the host config. The only exception is in
  `scripts/gpu-session.sh`, which may unload localwhisper's model and never reloads it.
- **All GPU work runs inside `scripts/gpu-session.sh`.** Nothing else uses the GPU.
- **Never apply the `baseline-change` label.** Only a maintainer applies it.
- **Tests never touch the live Wayland session.** No `wl-copy`, `wtype`, clipboard or input tools
  against it. Compositor tests run in `psst_testkit::nested_niri()`, which refuses to run if any
  client variable points at the live session.
- **Cargo never compiles CUDA or C++.** Engines come prebuilt from Nix.

## Building and testing

Work inside the dev shell: `nix develop` (Rust from the pinned nixpkgs, cargo-nextest, sccache,
mold, the system libraries, and Python with pytest for the engine).

Build and test the whole workspace before every push:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cargo nextest run --workspace
(cd engines/faster-whisper && pytest)
git diff --exit-code -- engines/faster-whisper/tests/golden   # golden frames unchanged
```

- **Tests run without a GPU**, through `psst-server __fake-engine` (`psst_engine_proto::fake`).
  GPU tests are marked as such and run locally inside `scripts/gpu-session.sh`.
- **Wayland tests run by default.** Each starts its own offscreen nested niri
  (`psst_testkit::nested_niri()`); sway, niri and mesa come from `PATH`, or from the flake's nixpkgs
  on the first run. In-process clients connect to `Sandbox::wayland_socket()` by path, never through
  this process's `WAYLAND_DISPLAY`.
- **Overlay snapshots:** after an intended change to the overlay's look, regenerate them with
  `PSST_UPDATE_SNAPSHOTS=1 cargo nextest run -p psst-overlay` and review the PNGs.
- **End-to-end tests live only in `psst-e2e`.** They start
  `env!("CARGO_BIN_EXE_psst-server-e2e")` and `env!("CARGO_BIN_EXE_psst-e2e-daemon")` through
  `psst-testkit`, so they always run freshly built binaries. Evals run the server in-process with
  `psst_server::serve_in_process`.
- **Regenerate `evals/baseline.json` after a rebase:** `cargo run -p psst-eval -- replay --write`
  (`docs/evals.md`).
- Add dependencies with `cargo add`, not by editing manifests.

## Contracts

Plan section 5 is implemented by:

| Contract | Code | Doc |
|---|---|---|
| Client ↔ server messages, HTTP bodies, shared types | `psst-proto` | `docs/protocol.md` |
| Server ↔ engine messages and framing | `psst-engine-proto`, `engines/faster-whisper/psst_engine_faster_whisper/protocol.py` | `docs/protocol.md` |
| Config files and the resolved `Profile` | `psst-config` | `docs/config.md` |
| Daemon ↔ Wayland thread | `psst-wayland` (`OverlayState` in `psst-overlay`) | the crate docs |
| Library seams | `psst-dict`, `psst-pipeline`, `psst-client`, `psst-server`, `psst-testkit` | the crate docs |

- **A change to a contract updates its doc in the same pull request.** Tests parse every `psst:`
  example in both docs into the real types, and compare the defaults in `docs/config.md` with the
  code.
- **Golden frames** in `engines/faster-whisper/tests/golden/` check the engine contract in both
  directions: pytest writes `from-engine/`, a Rust test in `psst-engine-proto` writes
  `to-engine/`, and each side decodes the other's. Running the tests regenerates them; commit the
  result. CI fails if they differ from what's committed.

## Crates and dependency direction

```
psst-proto ← psst-engine-proto, psst-config
psst-config ← psst-dict ← psst-pipeline ← psst-server ← psst-eval
psst-proto ← psst-client ← psst, psst-eval
psst-config ← psst
psst-overlay ← psst-wayland ← psst
psst-server, psst, psst-client, psst-testkit ← psst-e2e
```

Keep to these directions; in particular `psst-client` never depends on `psst-config`, and
`psst-overlay` and `psst-wayland` know nothing about the network.

## Git

Atomic commits with short imperative messages (`Add session registry`), no conventional-commit
prefixes.
