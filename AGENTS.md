# Working on psst

psst is a self-hosted dictation system: a Rust server runs the speech model on a GPU machine, and
thin clients send it audio over the network. The code is the documentation.

## What we care about

- **Latency.** Text should be there the moment the user stops talking. Back performance changes
  with measurements, not intuition.
- **Bad connections.** Clients sit on slow, lossy, high-latency links far from the server. Design
  for those first; never assume a LAN.
- **Thin clients.** Heavy work belongs on the server, so clients stay light and easy to port.
- **Ease of use.** It should just work: sensible defaults, no setup step the software could do
  itself, errors that say what to do next.
- **UX.** Fast, quiet and predictable.
- **Dev experience.** Fast builds, a small `target/`, fast CI. Weigh every new dependency, test
  binary and CI step against what it costs to build and run.

## Building and testing

Work in the dev shell (`nix develop`). `just` lists the recipes; run `just check`, which runs
everything CI checks, before every push.

- Tests run without a GPU, against a fake engine.
- Each crate's integration tests are one binary, `tests/it/main.rs`: add a module there, not a new
  `tests/*.rs`.
- Add dependencies with `cargo add`.
- Comment only what the code can't say: a counter-intuitive why, a workaround, an invariant that
  is easy to break. Never restate what the code does. Doc comments that tools turn into output
  (clap's `--help`, `just --list`) stay.

## Hard rules

- All GPU work runs inside `scripts/gpu-session.sh`.
- Tests never touch the live Wayland session: no clipboard or input tools against it. Compositor
  tests run in `psst_testkit::nested_niri()`.
- Cargo never compiles CUDA or C++; engines come prebuilt from Nix.
- Never apply the `baseline-change` label; only a maintainer does.

## Git

Atomic commits with short imperative messages (`Add session registry`), no conventional-commit
prefixes.
