# Gesso

Make images, video and music with your own Google Cloud and AWS accounts, with Jev choosing the
model, and hand the results to the Craft apps. No subscription, no credits.

The app is in [`gesso/`](gesso/): setup, local mode, the tools and the hand-offs are described in
[`gesso/README.md`](gesso/README.md).

```bash
cargo run -p gesso --release
```

## Based on ArtCraft

Gesso is a modified version of [ArtCraft](https://github.com/storytold/artcraft) by the ArtCraft
Team, under ArtCraft's MIT OR Apache-2.0 licence. It keeps ArtCraft's pure-Rust desktop app and
its client crates, and adds local mode, cost metering, routing, the studio tools and the Craft app
hand-offs. ArtCraft's Tauri desktop app and web frontend are not part of Gesso.

Gesso is not made, endorsed or supported by the ArtCraft Team. "ArtCraft" and its logos are the
ArtCraft Team's trademarks; Gesso uses the name only to say what it is based on. See
[`NOTICE`](NOTICE).

## Licence

MIT OR Apache-2.0, at your option ([`LICENSE-MIT`](LICENSE-MIT), [`LICENSE-APACHE`](LICENSE-APACHE)),
as for ArtCraft.
