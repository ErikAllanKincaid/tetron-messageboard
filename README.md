# tetron-messageboard

A mesh-hosted message board for one [tetron](https://github.com/ErikAllanKincaid/tetron)
network. One member runs it; everyone else on that tetron network can read
and post messages and images from their browser. Common-channel
communication scoped to the mesh, with no accounts and no login.

It is a genuinely separate, opt-in addon in the same family as
[tetron-relay](https://github.com/ErikAllanKincaid/tetron-relay),
[tetron-sync-receiver](https://github.com/ErikAllanKincaid/tetron-sync-receiver)
and [tetron-webui](https://github.com/ErikAllanKincaid/tetron-webui). It
makes **zero changes to tetron core** and talks to the local daemon only
over the existing IPC socket (via `tetron-proto`).

## Trust model: the mesh is the access control

tetron's trust boundary is admission: once a device is on the mesh, it is
trusted. tetron-messageboard leans on exactly that. The HTTP server binds **only to
the host's tetron mesh IP**, never `0.0.0.0`, so anyone who can reach it is
already an admitted peer. There is no separate auth layer.

- **Identity is not self-reported.** Each post is labelled by resolving the
  source mesh IP of the connection against the live roster
  (`tetron status`), server-side. The mesh IP is authenticated by tetron
  itself; a client cannot type in someone else's name.
- **Anyone can delete any post.** Deletion needs no ownership check, the same
  flat trust the rest of tetron already grants (any coordinator can `kick` or
  `nuke`). Deletes are soft: the post becomes a `[deleted by <host>]` ghost so
  conversational context survives.
- **No edit feature.** Delete and repost is sufficient.

## Install

Grab the release binary for your platform from the
[Releases](https://github.com/ErikAllanKincaid/tetron-messageboard/releases) page,
put it on `PATH` (e.g. `/usr/local/bin/tetron-messageboard`), then:

```bash
tetron-messageboard install                 # sole-network hosts: nothing else needed
tetron-messageboard install --network home  # pin a network when this node has several
tetron-messageboard install --port 28088    # override the default port
```

`install` registers a per-user service (systemd `--user` on Linux, a launchd
LaunchAgent on macOS), binds it to the chosen network's mesh IP, and waits for
it to come up. `tetron-messageboard uninstall` removes it.

Placing the binary in root-owned `/usr/local/bin` needs `sudo`; running the
board does not. The
[tetron-webui](https://github.com/ErikAllanKincaid/tetron-webui) Add-ons panel
can install and manage it for you.

## Using it

Open `http://<the-host's-mesh-ip>:28088/` from any device on the same tetron
network. The board is a single chat-style page: post text or an image
(attach with the paperclip, drag-and-drop an image anywhere on the page, or
paste one from the clipboard), click the three-dot menu on a post to delete
it. It polls every 10 seconds. It
reuses tetron-webui's design tokens and dark/light theming, so it looks like
another panel of the same app.

## Configuration

All knobs are environment variables (set them in the service unit, or export
before a manual `tetron-messageboard run`):

| Variable | Default | Meaning |
| --- | --- | --- |
| `TETRON_MESSAGEBOARD_PORT` | `28088` | Port to bind on the mesh interface |
| `TETRON_MESSAGEBOARD_NETWORK` | *(auto)* | Which network to serve (required only when the node has several) |
| `TETRON_MESSAGEBOARD_MAX_STORAGE_MB` | `1024` | Total attachment bytes before eviction starts |
| `TETRON_MESSAGEBOARD_MAX_ATTACHMENT_MB` | `25` | Largest single upload accepted |
| `TETRON_MESSAGEBOARD_EVICTION_GRACE_SECS` | `3600` | How long a new attachment is exempt from eviction |

Text messages are unbounded (plain text is tiny even at heavy use). Only
attachments are quota'd: once total attachment bytes exceed the storage cap,
the **largest** files older than the grace period are evicted first until back
under. An evicted attachment's post is kept, with an
`[attachment removed -- storage limit]` placeholder in place of the image.

## Storage

Everything lives under the per-user data dir (`~/.local/share/tetron-messageboard` on
Linux, `~/Library/Application Support/tetron-messageboard` on macOS):

- `messages.ndjson` -- the message log, one JSON object per line. Not sqlite:
  a single long-running service is the only writer, so an in-process lock is
  all the concurrency control needed, matching the zero-embedded-DB posture of
  every other tetron component. Deletes rewrite the file atomically (temp file
  + rename), the same technique tetron core's `InviteStore` uses.
- `attachments/<hash>` -- each attachment stored content-addressed by its
  blake3 hash, so the same image posted twice costs one file on disk.
- `board.json` -- which network this board serves and whether it was installed
  by a coordinator (decided once at install time).

## Security notes

- **No disguised uploads.** Uploads are validated by magic bytes (PNG, JPEG,
  WebP, GIF), not by filename or client-supplied type, so an SVG or HTML file
  renamed `.png` is rejected. Stored images are served with
  `X-Content-Type-Options: nosniff` and a `default-src 'none'; sandbox` CSP, so
  even a file that slipped past validation could never execute as script.
- **EXIF/GPS.** The board does **not** strip EXIF metadata; the UI warns that
  photos can carry location data and that everything posted is visible to the
  whole mesh. Stripping is deliberately not done: it is location-privacy, not
  anonymization (posters are already identified by hostname), so the warning is
  sufficient and it avoids an image-processing dependency.
- **Rate limit.** A light per-source-IP token bucket guards against a buggy
  client stuck in a retry loop. It is an accident guard, not an adversarial
  defense (the trust-network model already assumes admitted peers are not
  attackers).

## Build (development)

Release binaries are the primary install path. To build from source:

```bash
cargo build            # debug
cargo test             # unit tests
cargo build --release  # target/release/tetron-messageboard
```

The frontend (`static/`) is embedded into the binary at compile time; there is
no separate build step.

## License

MPL-2.0. Authors: Dario, ErikAllanKincaid.
