# mpvremote

Web remote control for mpv. One Rust binary serves a mobile-friendly web UI
and talks to mpv over its JSON IPC socket. Open `http://<pc-ip>:8000` on your
phone.

## Run

```sh
cargo build --release
./target/release/mpvremote
```

Attaches to an existing mpv IPC socket if present, otherwise launches
`mpv --idle` itself and relaunches it if it dies.

## CLI

```
mpvremote                 run the server (default)
mpvremote status | qr     address of the running daemon + a QR code to scan
mpvremote --help
```

`status` reads the port from a small state file the daemon writes at
`$XDG_RUNTIME_DIR/mpvremote.state` (falling back to `/tmp`), checks the pid is
alive, and prints every LAN address plus a scannable QR of the first one. It
reports failure if the daemon is not running or the state file is stale.

Only one daemon is tracked — the most recently started instance owns the state
file, so `status` reports that one.

## Install

### Arch (PKGBUILD)

```sh
cd packaging && makepkg -si    # builds from git HEAD, runs the test suite
systemctl --user enable --now mpvremote
mpvremote status               # scan the QR with your phone
```

The PKGBUILD lives in `packaging/` on purpose: `makepkg` uses `src/` and
`pkg/` as scratch directories and `makepkg -C` deletes them, which would wipe
this project's own `src/` if it ran from the repo root.

Installs the binary to `/usr/bin/mpvremote` and a **user** systemd unit to
`/usr/lib/systemd/user/mpvremote.service`. It is a user unit, not a system one,
because it needs the desktop session to reach mpv.

### From source

```sh
cargo build --release
sudo install -Dm755 target/release/mpvremote /usr/bin/mpvremote
install -Dm644 packaging/mpvremote.service ~/.config/systemd/user/mpvremote.service
systemctl --user enable --now mpvremote
```

The startup QR is only printed when stderr is a terminal, so it does not clutter
the journal under systemd.

## Config (env vars)

| Var | Default |
|---|---|
| `MPVREMOTE_PORT` | `8000` (or `--port N`, which wins) |
| `MPVREMOTE_SOCKET` | `/tmp/mpvremote.sock` |
| `MPVREMOTE_ROOT` | `$HOME` (file browser is confined to this) |

To attach to your own mpv instance, start it with
`mpv --input-ipc-server=/tmp/mpvremote.sock`.

## API

- `POST /api/command` — `{"command": [...]}`, raw mpv command passthrough
- `POST /api/open` — `{"target": "<path-or-url>"}` (YouTube etc. via yt-dlp)
- `GET /api/browse?path=...` — directory listing under `MPVREMOTE_ROOT`
- `GET /api/info` — port + browse root
- `GET /ws` — WebSocket, pushes mpv property changes

## Icons

[Reicon](https://reicon.dev) (MIT), inlined into `src/index.html` as an SVG
`<symbol>` sprite rather than loaded from their CDN — the page has to work on a
LAN with no internet. To swap an icon, pick a name from https://reicon.dev/icons
and replace the matching `<symbol>` body from the `@iconify-json/reicon` package.

## Layout

Playback controls are always on screen. Files, URL and Theme live behind tabs
(vanilla port of [interior.dev/docs/tabs](https://www.interior.dev/docs/tabs)):
one shared indicator, roving tabindex, arrow/Home/End keys, and only the
selected panel mounted. The selected tab is remembered in `localStorage`.

The footer shows the address this device reached the server on, plus a
connection dot. On startup the server also prints every LAN address it is
reachable at.

## Themes

Four palettes — **Cyberpunk** (default), Synthwave, Terminal, Slate — picked
from the card at the bottom of the page and remembered in `localStorage`. Each
is just a block of CSS custom properties at the top of `src/index.html`; an
inline `<head>` script restores the saved choice before first paint so there is
no flash. All four meet WCAG AA on body text, muted text, and button labels.

## File browser

The browsed directory is remembered in `localStorage` and restored on reload;
if that folder has since been deleted or moved outside the root, it falls back
to the root instead of showing an empty list.

Next / previous buttons step through the media files in the **playing file's**
own folder — derived from mpv's `path` on demand, not stored client-side, so
they keep working after a page reload, while you browse a different folder, and
when playback was started from mpv itself. They disable at the ends of the
folder and while idle or playing a URL.


The currently playing file is highlighted in the list: accent colour, a left
accent bar, its icon swapped to a soundwave, and `aria-current="true"`. mpv
reports `path` as the exact string the row sent, so it is a plain equality
match — URLs simply match nothing. The highlight follows playback started from
anywhere (including mpv itself) and clears on stop.

## Title marquee

The now-playing title is a vanilla port of
[interior.dev/docs/logo-marquee](https://www.interior.dev/docs/logo-marquee)
(`marquee()` in `src/index.html`): one rAF writing a single transform, the loop
unsubscribed while off-screen, hover/focus ramping it to a stop and resuming
from the pixel it stopped on, repeat count measured with a ResizeObserver, and
only one copy readable by a screen reader.

Two deliberate departures from the original: it only becomes a transport when
the text actually overflows (a short filename stays static, with zero animation
frames), and the focus-reveal nudge is not ported because nothing inside is
focusable.

## Detent sliders

Volume and speed use a vanilla port of
[interior.dev's slider-detents](https://www.interior.dev/docs/slider-detents)
(`detentSlider()` in `src/index.html`) — the original is React + `motion`, so
the spring is a ~20-line rAF integrator instead. Drag within the pull radius of
a detent and the value snaps there; arrow keys step past detents so in-between
values stay reachable, Shift+arrow and PageUp/PageDown jump detent to detent.

## Security

No authentication — this is a LAN tool. Do **not** expose the port to the
internet. Anyone who can reach it can control playback and list media file
names under `MPVREMOTE_ROOT`.

Two hardening measures limit the blast radius of that open access:

- **Command allowlist.** `/api/command` only accepts the exact verbs the UI
  needs (`cycle pause|mute|fullscreen`, `set_property volume|speed|aid|sid|
  pause|…`, `seek`, `stop`). mpv's IPC includes `run` and `load-script`, which
  execute arbitrary programs — a raw passthrough would be remote code
  execution for anyone on the LAN. Everything outside the allowlist is
  rejected. See `command_allowed()` in `src/main.rs`.
- **DNS-rebinding guard.** Requests whose `Host` header is a DNS name (rather
  than an IP literal or `localhost`) are refused, so a malicious website cannot
  rebind its hostname to your LAN IP and drive the server from your browser.
  See `guard_host()`.

Still open by design: the WebSocket at `/ws` does not check `Origin`, so a site
that already knows your exact LAN IP could read now-playing status (title,
path). Read-only and low-impact; not fixed to avoid breaking legitimate access
patterns. The file browser is still confined to `MPVREMOTE_ROOT` by
`check_path()` (canonicalize + prefix check, symlink-safe).
