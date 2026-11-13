# psst

A self-hosted dictation system for Linux. A server runs the speech model on a machine with a GPU; thin clients
(desktop now, Android later) send audio to it over Tailscale, a LAN or a local socket, and get text back with a
shared, centrally managed dictionary applied.

Status: early development.

## Documentation

The code is the documentation: each crate's docs (`cargo doc --open`) describe its part, and
[AGENTS.md](AGENTS.md) covers building, testing and contributing.

## License

MIT ([LICENSE](LICENSE)).
