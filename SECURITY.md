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

## Verifying the Android app

Every APK is signed by one key, whose certificate's SHA-256 is
`E6:E4:2B:0C:40:34:1B:7D:A5:98:13:D8:33:B9:9A:5D:F4:DD:9D:C7:5A:E7:6F:27:95:4C:72:AB:71:91:DF:0E`
(`apksigner verify --print-certs` shows it). GitHub attests that this repository's workflow built
and signed it: `gh attestation verify psst-*.apk --repo figi44/psst`.
