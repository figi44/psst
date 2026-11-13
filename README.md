# psst

A self-hosted dictation system for Linux. A server runs the speech model on a machine with a GPU; thin clients
(desktop now, Android later) send audio to it over Tailscale, a LAN or a local socket, and get text back with a
shared, centrally managed dictionary applied.

Status: early development.

## Documentation

- [docs/protocol.md](docs/protocol.md): the client ↔ server and server ↔ engine protocols.
- [docs/config.md](docs/config.md): every configuration key with its default.
- [docs/nix.md](docs/nix.md): the packages, the NixOS and Home Manager modules, and the dev shells.
- [AGENTS.md](AGENTS.md): how to build, test and contribute.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
