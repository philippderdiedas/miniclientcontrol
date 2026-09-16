# Mini Client Control

Mini Client Control is a Rust-based local signage/playback controller.

It provides:
- a web UI for uploading assets and managing a playlist,
- a SQLite-backed scheduler (order, enable/disable, optional date window),
- browser automation via Chrome DevTools Protocol (CDP),
- optional scroll behavior for long pages and PDFs,
- an override mode to immediately play a specific asset or URL,
- screen casting: anyone on the LAN can share their screen or camera to the
  display over WebRTC, and the playlist resumes automatically afterwards.

## Documentation

Longer-form documentation lives in [`docs/`](docs/): what it does, how it is put
together, the cast subsystem in detail, deployment, Raspberry Pi quirks and
troubleshooting.

## Tech Stack

- Rust + Tokio
- Axum (HTTP server + API)
- SQLx + SQLite
- Chromiumoxide (CDP browser control)
- Embedded static UI from `/web`

## Requirements

- Rust toolchain (stable)
- Chrome or Chromium installed

The controller starts the browser itself, in kiosk mode, with the flags it needs.
It is found automatically (`google-chrome-stable`, `chromium`,
`chromium-browser`, …); pin it with `--chromium /path/to/binary` if that guesses
wrong.

If something is already listening on the debugging port, the controller connects
to that instead of starting its own — so an existing setup that launches Chromium
from a session file keeps working. `--no-launch-browser` disables starting one
entirely.

## Quick Start

1. Build and run:

```bash
cargo run --release
```

2. Open the pages (accept the certificate warning once):

- `https://<device>/` — the **cast page** for guests
- `https://<device>/admin.html` — the **operator UI**

  On a private network `<device>` is a name derived from the LAN address, such as
  `192-168-178-15.clientctrl.cc`, served with a certificate browsers already
  trust. Otherwise it is `<device-ip>:3443` with a self-signed one.

Port 3000 is plain HTTP and binds to loopback only. It exists for the display
browser on this device, not for people; `--http-listen 0.0.0.0` opens it up if
something really needs the unencrypted API.

3. In the UI:

- Upload files in **Asset Management**
- Add assets/URLs in **Playlist Management**
- Set order, duration, schedule window, and scroll mode

### Protecting the operator UI

Credentials can be set in the admin UI under **Zugang zur Verwaltung**; they are
stored as a PBKDF2 hash, not in plaintext. They can also be given on the command
line, which takes precedence and is the way back in if the password set in the UI
is ever forgotten:

```bash
cargo run --release -- \
  --basic-auth-user admin \
  --basic-auth-password 'change-me'
```

Or with environment variables:

```bash
export BASIC_AUTH_USER=admin
export BASIC_AUTH_PASSWORD='change-me'
cargo run --release
```

If credentials are enabled, the control UI and the API require authentication.

The operator UI (`/admin.html`, `/playlist.html`, `/assets.html`,
`/displays.html`, `/webhooks.html`, `/api/*`) requires the credentials. The pages
the *display* browser renders are exempt, but **only when requested from
loopback**: `/uploads/*`, `/pdf_viewer.html`, `/pdf.min.js`,
`/pdf.worker.min.js`, `/autoscroll.js`, `/no_content.svg`,
`/empty_playlist.html`, `/logo.svg` and `/api/cast/state`.
Chromium is driven over CDP and cannot present credentials, so without this
exemption enabling Basic Auth leaves the screen showing 401 errors. Anything
reaching those paths from another host still has to authenticate.

The cast pages are a separate list and are exempt **from any address**, because
the sender is a guest's laptop rather than the operator: `/`, `/index.html`,
`/cast.html`, `/cast.js`, `/audio.js`, `/cast_display.html` and
`/api/cast/{ws,claim,pair,info,qr.svg,audio}`. `/cast_display.html` and
`/cast.js` are on *this* list and not the loopback one — the display browser
fetches them too, but so does every guest.
Control who may cast in the admin UI, or remove the feature entirely with
`--disable-cast`.

## Command Line Options

```text
--port <u16>                 (default: 3000, plain HTTP)
--http-listen <ip>           (default: 127.0.0.1 — loopback only)
--assets-dir <path>          (default: ./assets)
--database-path <path>       (default: miniclient.db)
--cdp-url <url>              (default: http://127.0.0.1:9222)
--display <name[:port]>      (repeatable; one per screen this deployment drives)
--cast-display <name>        (which declared screen a cast pins; default: the first)
--basic-auth-user <string>   (optional, must be set with password)
--basic-auth-password <string> (optional, must be set with user)
--disable-cast               (default: false)
--cast-tls-port <u16>        (unset: 443, else 3443 or next free; set: fatal if taken)
--cast-cert-path <path>      (default: cast-cert.pem)
--cast-cert-san <name,...>   (extra hostnames/IPs for the certificate)
--cast-auth <none|code|pairing>  (default: none)
--cast-code <string>         (required when --cast-auth=code)
--cast-max-edge <px>         (cap the longest frame edge a cast may send)
--cast-stun-url <url>        (optional, only if LAN ICE fails)
--no-launch-browser          (connect to an existing browser only)
--chromium <path>            (autodetected when unset)
--chromium-user-data-dir <p> (default: /tmp/miniclientcontrol-chromium-<cdp-port>)
--chromium-class <name>      (default: miniclientcontrol-<cdp-port>; the WM class/app_id)
--no-kiosk                   (windowed browser, useful when testing)
--chromium-arg <flag>        (extra browser flags, repeatable)
--browser-language <list>    (default: de,de-DE,en-US,en)
--locale <tag>               (dates and times; default: from LC_ALL/LC_TIME/LANG)
--managed-cert <auto|off>    (real cert for a <lan-ip>.clientctrl.cc name; default: auto)
--guest-pages <on|off>       (may guests put a web page on the display; default: off)
--public-url <none|mdns|X>   (how guests reach this device; default: none;
                              a custom .local name is published via avahi)
```

These options also support environment variables through `clap` `env` support.
`--display` is the one whose name does not follow: it reads **`DISPLAYS`**, and
takes a comma-separated list there or in a single flag value. The plural matters —
the X11 `DISPLAY` every kiosk session exports is not read as a screen
declaration.

## Runtime Behavior

- Creates the asset directory if missing.
- Creates/migrates SQLite tables (`assets`, `playlists`, `playlist_items`,
  `displays`, `settings`, `webhooks`).
- Starts an HTTP server on `0.0.0.0:<port>`.
- Serves uploaded files from `/uploads/...`.
- Serves embedded UI files with fallback to `index.html`.
- Runs one background browser loop **per declared display**, each of which:
  - reads the active entries of the playlist its display is assigned,
  - loads content in controlled browser tabs,
  - applies scroll mode,
  - reacts to skip/playlist/override signals for that display.

## API Overview

### Assets

- `GET /api/assets` — list assets
- `POST /api/assets` — upload one or more files (`multipart/form-data`)
- `PUT /api/assets/{id}` — update asset metadata (currently duration)
- `DELETE /api/assets/{id}` — delete file + DB row

### Playlists

- `GET /api/playlists` — list playlists, each with the number of items it holds
- `POST /api/playlists` — create one from `{ "name": "…" }`
- `PUT /api/playlists/{id}` — rename
- `DELETE /api/playlists/{id}` — refused with `409` while it still holds items,
  and the message says how many. Deleting an empty one that a display is playing
  leaves the display's row alone and simply unassigns it

### Playlist items

- `GET /api/playlist` — list items with joined asset info; `?playlist_id=<id>`
  narrows it to one playlist, and leaving it off still returns everything
- `POST /api/playlist` — add item (asset or URL). `playlist_id` is **required**:
  an item in no playlist is one no screen would ever play, and nothing would say
  so
- `PUT /api/playlist/{id}` — update order/duration/enabled/schedule/scroll config,
  or move the item to another playlist with `playlist_id`. A move is sent on its
  own — combined with any other field it is a `400` — and lands the item at the
  end of the target playlist, renumbering both playlists `1..n`. An unknown
  `playlist_id` is refused, and so is `null`: an item in no playlist is the state
  this field exists to repair
- `POST /api/playlist/{id}/move` — renumber within the item's own playlist
- `DELETE /api/playlist/{id}` — remove playlist item

### Settings

- `GET /api/settings` — current settings plus which ones the command line pinned
- `PUT /api/settings` — `{ cast_enabled?, guest_pages_enabled?, cast_auth?,
  cast_code?, auth_enabled?, auth_user?, auth_password?, overlay?, locale? }`;
  a pinned setting answers `409`
- `GET /api/overlay` — the layers the display runtime wants: the global overlay
  plus the one belonging to the item on screen, with asset ids already resolved

### Room audio

- `GET /api/audio` — operator: output devices and playing streams
- `POST /api/audio` — operator: set a volume, mute, or the output device
- `GET`/`POST /api/cast/audio` — the same, for the guest who is currently casting;
  guarded by the connected sender's address instead of by credentials

### Displays

- `GET /api/displays` — the declared screens, plus any row for a screen this
  deployment no longer declares (`declared: false`), so its playlist can still be
  reassigned
- `PUT /api/displays/{name}` — set `label` and/or `playlist_id`; `null` clears
  either one, and a `playlist_id` naming a playlist that does not exist is
  refused with `400` rather than reported as saved

### Playback Control

- `GET /api/control/current` — get current item id
- `POST /api/control/current` — jump to item id (`{ "item_id": <id|null> }`)
- `GET`/`POST /api/displays/{name}/control/current` — the same, for one named
  display

### Override

- `POST /api/override` — activate override playback
  - body supports either `asset_id` or `url`
  - optional `scroll_config`
- `GET /api/override` — the override currently up, if any
- `DELETE /api/override` — clear override and return to playlist loop
- `GET`/`POST`/`DELETE /api/displays/{name}/override` — the same, for one named
  display

The two unscoped paths above address the display when exactly one is declared.
With several they answer `409` and name them, because picking one would be a
coin flip a script cannot see.

### Casting

- `GET /api/cast/ws?role=sender|display[&code=]` — WebSocket signaling relay
- `GET /api/cast/info` — public: `{ enabled, page_enabled, auth, busy, sender_url }`
- `GET /api/cast/qr.svg` — public: QR code for the guest URL
- `GET /api/cast/state` — operator: who is casting and since when, plus
  `showing`: `null`, `"cast"`, or `{ "page": "<url, credentials stripped>" }`
- `DELETE /api/cast/session` — operator: end the current cast
- `POST /api/cast/pair` — request a pairing code (pairing mode only); the code is
  shown on the display and never returned in the response
- `POST /api/cast/claim` — guest: check the code and reserve the session before
  sharing; returns a ticket the WebSocket needs. Takes
  `{ mode: "cast" | "page" }`, defaulting to `cast` — the mode is settled here
  because the display is pinned as soon as the socket connects
- `DELETE /api/cast/claim` — give the reservation back

`GET /api/cast/state` also carries a live pairing code and its remaining seconds
while one exists, so the admin page can show what the display is showing.
`/api/cast/info` never does.

### Webhooks

- `GET /api/webhooks` — list targets, each with the result of its last delivery
- `POST /api/webhooks` — create a target
- `PUT /api/webhooks/{id}` — update a target
- `DELETE /api/webhooks/{id}` — remove a target
- `POST /api/webhooks/{id}/test` — render an enabled target against a sample
  event and deliver it for real, answering `{ ok, outcome }` with what came back
- `GET /api/webhooks/events` — the event catalogue: name, description and the
  fields each one carries, plus the rule for composing a placeholder. The admin
  page builds its checkboxes and its chips from this and never from a copy of
  its own

Create and update reject what could not work: an unknown event name, a URL whose
scheme is not `http`/`https`, a method other than `POST`/`PUT`/`PATCH`, a header
name hyper would refuse, and a body or header template that does not compile.
Each answers `400` with `{ "error": "…" }`.

## Screen Casting

A guest opens `https://<device>/`, enters the code if one is required,
and picks **Bildschirm teilen** or **Kamera teilen**. The code is checked the
moment it is typed and the session is reserved right then — before the browser's
screen picker opens, so nobody chooses a window only to be told the code was
wrong, and two guests cannot both get that far. Stopping the share —
or closing the laptop — hands the screen back to the playlist automatically.

The cast page is the site root on purpose, so the address a guest has to type is
as short as possible. The operator UI sits at `/admin.html`, linked from a
discreet button at the bottom of the cast page. Opening the cast page over plain
HTTP from the LAN redirects to the HTTPS address automatically.

The stream itself is peer-to-peer WebRTC; the controller only relays the
handshake and decides when the display switches over. Internally a cast is just
an override, so it interrupts the current playlist item and resumes exactly where
it left off.

### The address guests are given

By default that is the device's LAN address. `--public-url` changes it:

```bash
--public-url none                        # https://<lan-ip>-dashed.clientctrl.cc/  (default)
--public-url mdns                        # https://<hostname>.local/
--public-url signage.example.com         # https://signage.example.com/
--public-url https://signage.example.com # taken as-is, for a reverse proxy
```

`mdns` uses this machine's hostname, which Avahi already announces. Any *other*
`.local` name is announced by the controller itself (`avahi-publish`), which is
how one machine driving two screens becomes `kiosk2-links.local` and
`kiosk2-rechts.local` rather than one ambiguous name. It needs `avahi-daemon` and
`avahi-utils`; without them the controller says so and falls back to the address.

Whichever name is chosen is added to the certificate automatically.

A machine that publishes with Avahi but resolves with systemd-resolved may not be
able to look up its own published names. Guests on the LAN still can — that is the
case that matters.

The address, a QR code for it and — in code mode — the code are shown on the
display whenever the playlist is empty, and on the cast standby screen.

### Why HTTPS

`getDisplayMedia` only works in a secure context. `http://<lan-ip>:3000` is not
one, so the sender page gets its own HTTPS listener with a self-signed
certificate that is generated on first start. Browsers warn about it once;
accepting the warning is enough. The display browser is unaffected — it reaches
the controller over loopback, which counts as secure either way.

The certificate is regenerated automatically if the device's addresses change,
and covers `localhost`, `127.0.0.1`, the primary LAN address, the hostname and
`<hostname>.local`. Add more with `--cast-cert-san`.

### Access control

Set in the admin UI under **Übertragung**, and persisted in the database:

- **Offen** — anyone on the LAN can cast (default)
- **Fester Code** — a 4-character PIN you hand out or put on a label
- **Code auf dem Display** — a fresh code appears on screen for 30 seconds and
  can only be used once

**Command-line flags always win.** A setting passed as a flag is pinned: the
admin UI shows it as locked and refuses to change it. Anything not passed falls
back to the stored value and stays editable. Besides respecting whoever wrote the
service file, this is the recovery path — a forgotten UI password is always
fixable by passing `--basic-auth-user`/`--basic-auth-password`.

`--disable-cast` additionally stops the HTTPS listener from binding at all, so it
can only be changed by restarting.

Basic Auth is deliberately *not* used for casting: it would mean handing the
operator password to every guest. Wrong codes are rate-limited per address.

For unattended audio, start the display Chromium with
`--autoplay-policy=no-user-gesture-required` — otherwise the receiver falls back
to muted playback, since nobody is there to click.

Notes and limitations:

- Only one sender at a time; a second one is told the display is busy.
- **One cast session for the whole controller, on one screen.** With several
  displays declared, `--cast-display <name>` picks which one a cast pins; without
  it, the first declared. Per-display casting is not built yet, so the QR code and
  the invitation on the idle screen are still global — a second panel standing
  idle advertises a cast that will appear on the cast display instead. See
  [docs/casting.md](docs/casting.md#which-screen-a-cast-lands-on).
- Screen sharing needs a desktop browser. Mobile browsers have no
  `getDisplayMedia`, though camera sharing works.
- Screen *audio* is only shared reliably by Chrome, and only when the user ticks
  the audio box in the picker. The page says so when no audio track arrives.

## Invalid TLS certificates

Playlist URLs are loaded even when their certificate does not validate, so
internal dashboards on self-signed certificates work without ceremony. Whoever
adds a URL is trusted to know what they are pointing the display at.

This is a property of the browser connection and cannot be set per playlist item.

## Two displays on one machine

One controller drives as many screens as it is told to. Declare them:

```bash
miniclientcontrol --display foyer --display werkstatt
```

That is the whole configuration. One HTTP port, one database, one asset library
and one admin page; each screen gets its own Chromium, its own control loop and
its own assigned playlist. Assign the playlists on `/displays.html`.

Each declared display derives what a separate controller used to be given by
hand:

| | derived from | `--display werkstatt` (second) |
|---|---|---|
| CDP port | `9222 + declaration index`, or the `name:port` form | `9223` |
| WM class / Wayland `app_id` | `miniclientcontrol-<name>` | `miniclientcontrol-werkstatt` |
| browser profile | `/tmp/miniclientcontrol-chromium-<name>` | `…-chromium-werkstatt` |

`--display werkstatt:9300` pins one display's port without shifting any other's:
the implicit ports still count from 9222 by declaration index. A name reaches a
window-manager config and a filesystem path, so it is restricted to letters,
digits, `-` and `_`.

Because the declared branch derives all three, `--chromium-class`,
`--chromium-user-data-dir` and a non-default `--cdp-url` are **refused** at
startup alongside any `--display`, rather than being accepted and ignored. So is
a `--class` smuggled in through `--chromium-arg`: Chromium takes the last
`--class` on the command line, which would collapse every display onto one
`app_id`. With no `--display` at all, none of that changes — one implicit display
named `default` on `--cdp-url`, with exactly the previous defaults.

Window placement is still the window manager's job. Under sway the class is the
`app_id`:

```
assign [app_id="miniclientcontrol-foyer"]     output HDMI-A-1
assign [app_id="miniclientcontrol-werkstatt"] output DP-1
```

Under i3 it is `WM_CLASS`, and `assign` only chooses a workspace — pin the
workspaces to outputs as well, or i3 decides which screen a workspace lands on
and not reliably the same way after a restart:

```
assign [class="miniclientcontrol-foyer"] 1
assign [class="miniclientcontrol-werkstatt"] 2
workspace 1 output HDMI-1
workspace 2 output HDMI-3
```

A systemd unit that lets the controller start the browsers needs the display in
its environment:

```ini
[Service]
Environment=DISPLAY=:0
```

One process for every screen is a deliberate trade — see
[docs/deployment.md](docs/deployment.md#declaring-the-screens-a-deployment-drives).

## The "translate this page?" bubble

Chromium offers to translate any page whose language is not among the profile's
accepted languages, and on Linux this cannot be turned off with a flag. Because
the controller writes the browser profile before every launch, it sets the
accepted languages to `--browser-language` (default `de,de-DE,en-US,en`) and
disables translation there. Set it to whatever your signage actually shows.

If the browser is started outside the controller, use the managed policy instead:
`/etc/chromium/policies/managed/no-translate.json` containing
`{ "TranslateEnabled": false }`.

## Scroll Configuration

`scroll_config` is serialized as tagged JSON:

- `{"type":"None"}`
- `{"type":"Step","options":{"step_time":500,"step_px":null,"step_delay":2000}}`
- `{"type":"Continuous","options":{"speed":1.0,"top_delay":2000,"return_delay":2000}}`

PDFs are rendered through the internal viewer (`/pdf_viewer.html`) and support both step and continuous scrolling.

## Project Layout

- `src/main.rs` — app bootstrap, router setup
- `src/handlers.rs` — REST API handlers
- `src/browser.rs` — browser/session/playback loop
- `src/db.rs` — schema init + lightweight migrations
- `src/display.rs` — the declared screens, their derivation, and the display API
- `src/playlists.rs` — playlists as objects (`/api/playlists`)
- `src/models.rs` — CLI args, DTOs, app state
- `src/web.rs` — embedded static file serving
- `src/cast.rs` — cast signaling relay and session lifecycle
- `src/tls.rs` — self-signed certificate + HTTPS listener for the sender page
- `web/` — frontend pages and JS helpers
- `assets/` — uploaded files (runtime)

## Notes

- The server currently uses permissive CORS (`CorsLayer::permissive()`).
- Max upload body size is configured to 500 MB.
- This project is designed for trusted local/network environments unless hardened further.
