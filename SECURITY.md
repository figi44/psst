# Security

Report a vulnerability privately, under this repository's **Security** tab ("Report a
vulnerability"), rather than in a public issue.

## What psst trusts

- **The network.** psst has no TLS of its own. The server takes connections only from its own
  machine and its tailnet, where Tailscale encrypts them, unless `allow_from` in `server.toml` lets
  in another network. Anyone on such a network can read its tokens and dictations.
- **Every paired device.** A paired device, and any browser it logs in to the web UI, can do what
  the server's own users can: change the dictionary, profiles and settings, read every device's
  history, and load or unload the model. Pair only devices you control, and revoke a lost one with
  `psst-server devices revoke`.
- **The server's machine.** Pairing codes come only from `psst-server pair`, run there, and last
  five minutes.
