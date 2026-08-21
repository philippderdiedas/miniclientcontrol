# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What this is

`miniclientcontrol` is a single-binary digital-signage controller that runs **on the
client/display device itself** (typically a Raspberry Pi — note the
`arm-unknown-linux-gnueabihf` target in `target/`). It does two things at once:

1. Serves a small web UI + JSON API on `--port` (default `3000`) so an operator can
   upload assets and manage a playlist.
2. Drives a locally running Chromium over the Chrome DevTools Protocol (CDP),
   navigating it through the playlist and injecting a scroll runtime.

The controller **starts Chromium itself** (`src/chromium.rs`) unless something is
already listening on the CDP port, in which case it connects to that instead. So
an existing deployment whose sway `exec` line starts Chromium keeps working
untouched, and a fresh one needs no browser configuration at all.
`--no-launch-browser` forces connect-only.

The binary is found via `chromiumoxide::detection` (`CHROME` env var, then
`google-chrome-stable`, `chromium`, `chromium-browser`, …) or pinned with
`--chromium`. The supervisor also restarts the browser if it exits, which the
connect-only arrangement could not do — the loop just sat there reconnecting.

**Two displays on one machine** run two controllers, and everything that can
collide has to differ. `--chromium-user-data-dir` and `--chromium-class` therefore
default to values derived from the CDP port, which instances already have to keep
distinct. Sharing a profile directory is the nasty one: a second Chromium started
on a profile that is already in use hands its URL to the running instance and
exits, taking its debugging port with it — so the second display silently never
appears, and nothing looks wrong except that it is not there.

`--class` sets the second field of `WM_CLASS`, which is what an i3
`assign [class="..."] <workspace>` rule matches. Measured on the two-screen kiosk:
`class=chrome-1` landed on HDMI-1 at `0,0` and `class=chrome-2` on HDMI-3 at
`1920,0`. Note that `assign` only picks the workspace — pin workspaces to outputs
with `workspace 1 output HDMI-1`, or which screen gets which is up to i3 and not
stable across restarts.

A unit that lets the controller start the browser needs `DISPLAY` (and
`XAUTHORITY`) in its environment, which a unit that only talked CDP did not.

On a **Wayland** kiosk (sway) it is `WAYLAND_DISPLAY=wayland-1` plus
`--chromium-arg=--ozone-platform=wayland` instead, and a systemd *user* service
does not inherit either from the compositor even when sway's own config is what
starts it (`exec systemctl --user start ...`). Without them Chromium launches,
finds no display server, exits before it opens the debugging port, and the
supervisor relaunches it every ten seconds forever: no `DevToolsActivePort` file,
nothing listening on the CDP port, and — because journald on the Pi image stores
nothing — no log to say so. Give the unit
`StandardOutput=append:<path>` if the journal is empty; the diagnosis is
otherwise invisible.

It deliberately does **not** kill the browser when the controller stops: a deploy
or a crash should not blank the screen, and the next start simply reattaches.

Chromium's "translate this page?" bubble is **not** suppressible by a command-line
flag, and there is no CDP command for it either.

Since the controller owns the launch, the fix is now in the profile it writes
before every start (`chromium::write_preferences`): `intl.accept_languages` is set
to `--browser-language` (default `de,de-DE,en-US,en`) and `translate.enabled` to
`false`. The language list is the part that matters — the bubble appears when the
page's language is not among the accepted ones, so matching them removes the
trigger rather than fighting the symptom. Measured: with these written,
`navigator.languages` follows, and Chromium keeps both keys when it rewrites the
file. Writing it on every launch is also what makes it survive the profile
directory being wiped on boot, which is why a profile preference used to be
useless here.

The keys are merged into any existing `Preferences` rather than replacing the
file, so window bounds and zoom levels are not thrown away.

The managed policy below still works and is the right answer when the browser is
started outside this controller. It needs root, on the device at
`/etc/chromium/policies/managed/no-translate.json`:

```json
{ "TranslateEnabled": false }
```

Verify with `chrome://policy`: the row must read `TranslateEnabled / false /
Platform / Machine / Mandatory / OK`. Three flags look like they should do this
and do not: `--disable-translate` and `--disable-infobars` are ignored outright by
current Chromium (144 on the device), and `--disable-features=Translate` *is*
applied — child processes inherit it — but does not gate the bubble. The prompt
appears because the profile's `intl.selected_languages` is `en-GB,en-US,en` while
the signage shows German pages. The policy is also the only durable fix here:
`--user-data-dir=/tmp/chromium-1` is wiped on boot, so a profile preference would
not survive.

## Build & run

```bash
cargo build            # debug
cargo build --release
cargo run --release -- --port 3000 --cdp-url http://127.0.0.1:9222
```

Casting adds an HTTPS listener (default `3443`) and a self-signed certificate:

```bash
cargo run --release
# guests:   https://<lan-ip>:3443/            (accept the cert warning once)
# operator: https://<lan-ip>:3443/admin.html
# port 3000 is loopback-only and serves the display browser
```

`--disable-cast` turns the whole thing off. When testing locally, share a single
*window* rather than the whole screen, or the display shows an infinite mirror.

Cross build for the Pi target that is already configured:

```bash
cargo build --release --target arm-unknown-linux-gnueabihf
```

The device (`pi@10.124.11.124`) reports `uname -m` = `armv7l`, so the build that
actually ships is `armv7-unknown-linux-gnueabihf` via `cross` (Docker daemon must
be running). Give each cross target its **own** `--target-dir`: host proc-macro
`.so`s land in the shared `target/release/build/`, and the per-target `cross`
images carry different glibc versions, so reusing one directory across two targets
fails with `symbol getrandom, version GLIBC_2.25 not defined`.

```bash
cross build --release --target armv7-unknown-linux-gnueabihf --target-dir target/cross-armv7
```

The TLS and QR dependencies were the risk in that build and have been verified on
a Raspberry Pi 2 (armv7l, Raspbian bookworm): `ring`, `rcgen` and `qrcode` compile
and the binary generates its certificate on the device. The result links against
nothing but glibc, libgcc and libm.

There is no linting config, and `cargo build` is the gate for the Rust side.
**Stop any locally running instance before the suite.** `test_port.py` needs the
default `3443` free to test the fallback, and `test_browser.py`/`test_overlay.py`
launch their own Chrome on `9222`/`9232`. A dev instance holding those makes both
fail in a way that looks like a code regression -- they pass again the moment it
is stopped.

`tests/cast/` holds stdlib-only Python end-to-end tests for the cast feature
(signaling, override coupling, auth modes, runtime settings, basic-auth
boundaries, and a real two-Chrome WebRTC session). They are not wired into any
CI; run them by hand after touching `cast.rs`, `tls.rs` or the route table.

### Deploying to the device

The binary is running, so copy beside it and rename over the top — an in-place
`scp` gets `ETXTBSY`. Keep the previous binary as a rollback.

```bash
scp <binary> pi@10.124.11.124:~/miniclientcontrol/miniclientcontrol.new
ssh pi@10.124.11.124 'cd ~/miniclientcontrol && cp -a miniclientcontrol miniclientcontrol.bak && mv miniclientcontrol.new miniclientcontrol'
```

**Restart with `sudo loginctl terminate-session <id>`, not by restarting
`getty@tty1`.** Sway is launched from an autologin `/bin/login -f` and lives in a
logind session scope; restarting the getty *service* leaves that scope alone. The
controller and Chromium then survive as orphans re-parented to PID 1, the old
controller keeps port 3000, and the fresh one from sway's `exec` dies on the bind
— leaving the display running the old, already-deleted binary. Terminating the
session kills the whole cgroup, orphans included, and autologin brings everything
back. Find the id with `loginctl list-sessions` (the one with a `tty1` seat).

Also beware `pkill -f` over SSH: a pattern like `miniclientcontrol/miniclientcontrol`
matches the remote shell running the command and kills it mid-script, so the rest
of the command never runs and the output is silently empty.
After changing anything under `web/`, you must **rebuild** — `web/` is compiled
into the binary via `include_dir!` (see `src/web.rs`), it is not read from disk.

## Architecture

```
src/main.rs      CLI parsing, DB pool + schema bootstrap, basic-auth middleware,
                 axum Router, spawns browser_loop
src/models.rs    Args (clap), Asset, PlaylistItemWithAsset, ScrollMode, AppState
src/db.rs        run_migrations() — idempotent CREATE TABLE + ADD COLUMN probes
src/handlers.rs  JSON/multipart API handlers
src/browser.rs   the CDP control loop (largest file; all playback logic)
src/web.rs       serves web/ embedded via include_dir
src/cast.rs      screen-cast signaling relay + session lifecycle
src/settings.rs  runtime settings (cast, overlay, operator credentials) + their API
src/chromium.rs  finds, launches and supervises the display browser
src/mdns.rs      publishes an extra `.local` name via avahi-publish
src/tls.rs       self-signed cert + HTTPS listener for the cast sender page
web/             operator UI + the pages the *display* browser renders
web/overlay.js   the overlay runtime, injected into whatever is on screen
```

### Three audiences for HTTP

This is the single most important thing to keep in mind when touching routes or
middleware. The HTTP server has **three different clients**:

- The **operator** (a human, possibly remote): `/admin.html`,
  `/assets.html`, `/playlist.html`, `/api/*`.
- The **display browser** (Chromium on loopback, sends no credentials):
  `/uploads/*`, `/pdf_viewer.html`, `/pdf.min.js`, `/pdf.worker.min.js`,
  `/autoscroll.js`, `/no_content.svg`, `/empty_playlist.html`, `/logo.svg`,
  `/cast_display.html`, `/cast.js`, `/api/cast/state`.
- The **cast sender** (a guest's laptop, *remote*, no credentials):
  `/`, `/index.html`, `/cast.js`, `/api/cast/ws`, `/api/cast/pair`,
  `/api/cast/info`.

**The site root is the guest page, not the operator page.** `/` serves the cast
sender so a guest can be handed `https://<device>:3443` and nothing longer; the
operator landing page lives at `/admin.html`. Moving a page between those two
sets means moving it between `is_cast_public_path` and plain (authenticated)
routing — get that backwards and either the signage guests are locked out or the
admin UI is wide open. `/cast.html` 301s to `/` so the older URL keeps working.

Basic auth must never be applied to the second set, or the signage goes blank.
`src/main.rs` handles this by exempting loopback peers (`ConnectInfo<SocketAddr>`)
— which is why the server is started with
`into_make_service_with_connect_info::<SocketAddr>()`. Do not drop that. The TLS
listener uses it too, and the extractor panics without it.

The third set is exempt **regardless of address** (`cast::is_cast_public_path`),
because the sender is by definition not loopback. Gating it behind basic auth
would mean giving the operator password to everyone who wants to share a screen;
`--cast-auth` is what guards those routes instead. Keep the two predicates
separate — widening `is_display_path` would expose the display-only paths to the
whole LAN.

### Control loop (`src/browser.rs`)

Nested loops:

- **outer**: connect to CDP, subscribe to `Target.attachedToTarget`, pick/clean a
  single control page, install the scroll runtime. Any `is_connection_lost` error
  breaks back out to here and reconnects.
- **inner**: if an override is set, run `run_override_loop`; otherwise fetch the
  active playlist (`is_enabled = 1` and inside the date window), reconcile
  `keep_loaded` tabs, then iterate items.
- **per item**: navigate, wait for readiness, start scrolling, then
  `tokio::select!` on the duration timer / `skip_signal` / `playlist_signal`.

### Signals

`AppState` carries three `Arc<Notify>`: `skip_signal`, `playlist_signal`,
`override_signal`.

**Always use `notify_one()`, never `notify_waiters()`.** The control loop is only
parked on these notifies for part of its cycle (navigation and readiness waiting
can take 10+ seconds). `notify_waiters()` drops a notification when no task is
currently parked, which silently loses "Play now" clicks and override changes.
`notify_one()` stores a permit, so the loop picks it up at the next await point.

### "Play now" / jumps

`POST /api/control/current` writes `AppState::pending_jump`, **not**
`current_item_id`. `current_item_id` is loop-owned (it reports what is on screen);
the loop overwrites it at the top of every item, so a request writing there would
be clobbered.

**Never consume `pending_jump` before the target has been looked up in a freshly
fetched playlist.** The loop's `playlist` snapshot is read once per inner-loop pass
and then iterated item by item, so it can be a whole item duration out of date — an
item added or re-enabled since the last fetch is simply not in it. The loop
therefore *peeks* on `skip_signal`: if the target is in the current snapshot it
jumps immediately, otherwise it breaks out, re-reads the playlist, and resolves the
jump against the fresh list before the item loop starts. Only then is it cleared.
`take()`ing it on the miss path silently dropped the click and resumed playback on
an unrelated item, which is what "Play now plays something random" was.

### Scroll runtime

`web/autoscroll.js` is both served over HTTP *and* `include_str!`-ed into the
binary (`scroll_runtime_script()` in `browser.rs`). It installs `globalThis.__as`
plus `__asApply`. It is registered via `Page.addScriptToEvaluateOnNewDocument` and
also re-evaluated after navigation, because pages with a strict CSP can block it —
`apply_scroll_settings` probes `!!globalThis.__as` and no-ops if it is missing.

PDFs are a special case: they are rendered by `web/pdf_viewer.html` (pdf.js), which
drives its own scrolling from query parameters. `browser.rs` detects this with
`is_internal_pdf_viewer_url` and skips `start_scrolling`/`stop_scrolling` for those.

### Room audio, two doors (`web/audio.js`)

The same panel serves the guest and the operator, from one implementation, over
two endpoints that differ only in who is let in:

- `/api/cast/audio` — the guest's. Guarded by `caster_only` (the connected
  sender's address, nobody else) and listed in `is_cast_public_path`, because
  somebody sharing a screen has to reach it without operator credentials.
- `/api/audio` — the operator's. Absent from that list, so basic auth decides,
  and it works whether or not anybody is casting. **Loopback is not a way around
  it**: the loopback exemption in `basic_auth_middleware` covers `is_display_path`
  only, so the admin page asks for credentials even on the device itself, exactly
  like `/api/settings`.

Both end in `cast::apply_audio`, so the guest's knobs and the operator's cannot
drift apart. The panel hides itself when the device reports no sound server
(`available: false`), and the admin card follows the panel rather than
second-guessing it — a Pi image without `pactl` simply shows no card.

Widening `caster_only` to admit the operator was the tempting shortcut and is
wrong: that route is exempt from basic auth, so "also allow anyone else" would let
any guest on the LAN turn the room up at three in the morning.

### The overlay (`web/overlay.js`)

Badges the operator can put on top of whatever is playing — text, an uploaded
image, a clock, a date, a QR code. There are **two sources and they are
additive**: the global overlay in the admin UI (`settings.overlay_config`) and the
current playlist item's own (`playlist_items.overlay_config`). The point of the
split is the standing case versus the contextual one — a clock that is always
there, plus a "Mehr Info" QR that belongs to one item.

The runtime therefore takes a *list* of layers, global first. **Layers that want
the same corner share one box**, stacked in order, and the first layer in a box
decides how it looks; layers in different corners get their own box. An item that
names no corner joins the global overlay's box, which is the arrangement nobody
has to think about. Two competing background colours in one box would read as a
bug rather than a choice, which is why the style is not per layer.

The global overlay's QR has a **source**, not just a text: `qr_source` is either
`text` (whatever was typed) or `cast`, which resolves to the guest URL when the
overlay is drawn. Resolved rather than stored because that address is not stable —
`--public-url` decides its shape, an occupied `--cast-tls-port` moves it to the
next free port, and a DHCP lease changes the LAN address. A typed copy would go
quietly wrong on a screen nobody is checking. With casting switched off the cast
source draws nothing at all: advertising a way to share a screen that refuses
every sender is worse than silence.

The admin UI offers a third choice, `keiner`, which is not a server-side value —
it just clears `qr_text` with source `text`. Naming it beats an operator guessing
that emptying a field is how you switch the code off.

An item's overlay carries content and a corner only — colours, sizes and opacity
come from the global one, so the display keeps one look across items and the
playlist card stays small enough to edit next to everything else on it.

It is injected the same way as the scroll runtime, via
`Page.addScriptToEvaluateOnNewDocument` plus a re-evaluation afterwards, and
`apply_overlay` probes `!!globalThis.__ov` and no-ops when a strict CSP kept the
injection out. There is **no CDP command that draws over a page**: an overlay is
always DOM in the target document, which is what the four defences in
`overlay.js` are about.

- **Shadow DOM plus `all: initial`.** Otherwise the target page's CSS decides how
  the notice looks, and a `div { display: none }` somewhere makes it vanish.
- **The top layer, via `popover="manual"`.** An element in fullscreen covers
  *every* z-index there is, so a video or a fullscreen dashboard would hide the
  overlay exactly when it matters. `manual` and not `auto`: an auto popover
  closes on the next Escape or outside click, and this one is not the page's to
  dismiss. The `z-index` in the host style is only the fallback for a browser
  without popover support.
- **A `MutationObserver` that re-attaches it.** SPAs replace whole subtrees and
  take the overlay with them.
- **Nothing is fetched.** Chromium's Local Network Access refuses a request to
  `127.0.0.1` from any origin that is not itself loopback, and a kiosk has nobody
  to click the permission prompt. Measured on Chrome 151 with a fresh profile,
  from a page on `https://example.com`: `fetch` fails with `TypeError` and an
  `<img>` with `EncodingError`. Since `--chromium-user-data-dir` is wiped on boot,
  a granted permission would not survive anyway.

  So the QR arrives in the payload as a **module matrix** (`qr_modules`, one
  `0`/`1` string per row, `cast::qr_matrix`) and `overlay.js` draws it as inline
  SVG, merging runs of dark modules into one rect each. That also survives an
  `img-src` CSP, which would refuse even a `data:` URI. An image is the one thing
  the overlay cannot draw itself, so it travels as a `data:` URI capped at
  `OVERLAY_IMAGE_MAX_BYTES` (512 KB) — every apply carries that string over CDP,
  and the display re-applies on every item. `check_overlay_image` refuses an
  oversized or non-image asset when it is *picked*, so the API and the payload
  builder cannot disagree.

  The test for this serves its foreign page from the machine's **LAN address**,
  not `127.0.0.1`: loopback-to-loopback is not gated, so a foreign page on
  `127.0.0.1` passes while a real display fails. That blind spot is exactly how
  the first version shipped a QR that only worked after a permission click.

Sizes are in `vmin`/`vw`, not pixels, so one configuration reads the same on a
1080p landscape panel and a portrait 4K one — signage is looked at from across a
room.

Content lines up with the corner it sits in (`alignmentFor` in `overlay.js`): a
`-center` position centres its text, `-right` right-aligns it, everything else
stays left. The QR row is flex, so `text-align` does not reach it and it gets the
same alignment spelled out as `justify-content`. Deliberately no separate
alignment control — a box pinned centre-bottom with left-aligned text is a
mistake, not a choice worth offering.

**The box style is stored structured, not as CSS.** `background_color` plus
`background_alpha`, `color` plus `color_alpha`, and a `plain` flag for "no box at
all" (its own flag rather than alpha 0, because it also drops the padding and the
corners). The free-text field this replaced had one failure mode worth
remembering: an invalid CSS declaration is dropped by the browser without a word,
so a typo meant a box with no background and no error anywhere — on a screen
nobody is standing in front of.

`background_css` remains as an escape hatch for a gradient, used verbatim when
set. `sanitize_css_value` keeps it a *value*: no `;`, no braces, no quotes, no
`@`, 200 characters. It is interpolated into a declaration in the overlay's own
stylesheet, and while a shadow-root style cannot reach the page, letting somebody
rewrite the rest of their own box from a text field is not worth the trouble.

Old configurations are migrated on load (`Overlay::migrate_legacy_style`): a
stored `rgba(...)`, `#rrggbb` or `#rrggbbaa` becomes colour plus alpha, an
unparseable value becomes `background_css` rather than being thrown away, and the
former whole-box `opacity` is folded into the background alpha. Both legacy fields
are `skip_serializing`, so they leave the API the first time the settings are
written back. The whole-box opacity is gone on purpose: two controls that both
read as "transparency" is a trap, and washed-out text on signage is rarely what
anyone wanted.

**The loop reads an item's overlay fresh, never from its playlist snapshot**
(`db::load_item_overlay`). That snapshot is read once per inner-loop pass and can
be a whole item duration old — the same trap documented for `pending_jump` — so a
layer edited while the previous item was up would appear one full rotation late.

`AppState::overlay_signal` is what makes an edit land on the item *already* on
screen; `PUT /api/playlist/{id}` pokes it too, because the item being edited may
be the one on screen. All three places the loop can be parked handle it: the per-item
`tokio::select!` (which recomputes the remaining time rather than restarting it,
so an overlay edit cannot extend an item), the idle-screen wait, and
`run_override_loop` — a cast or a pinned page can stand for hours, which is
exactly when a notice matters, and re-applying touches nothing else so a live
`RTCPeerConnection` survives it.

`settings::overlay_payload` inlines everything a layer needs and is the *only*
place that builds the runtime's configuration. The admin preview
fetches `/api/overlay` and runs the display's own runtime against the identical
payload rather than rebuilding the badge in the page: two renderings of the same
settings drift, and then somebody hunts a display bug that is really a UI bug.

An enabled overlay with nothing in it is a `400`, not a silently invisible
switch. Out-of-range numbers are clamped instead of rejected, and an unknown
corner falls back — the alternative is an error message on a screen nobody is
standing in front of.

### pdf.js

`web/pdf.min.js` and `web/pdf.worker.min.js` are vendored (v3.11.174) and served
from the same origin. Never point `workerSrc` at a CDN — the device is often
offline, and the operator page would work while the display silently failed.

### Screen casting (`src/cast.rs`, `src/tls.rs`)

Replaces a separate picklecast process. A sender on the LAN opens `cast.html`,
the display browser opens `cast_display.html`, and the two do WebRTC directly;
the controller only relays opaque `{sdp}` / `{ice}` blobs and owns the session
lifecycle. It never parses WebRTC payloads — not parsing them means it cannot
break them.

**A cast is an override.** Starting one pins `override_item` to
`http://127.0.0.1:<port>/cast_display.html`; ending one puts back whatever was
there before. `browser.rs` needed *no changes at all*, because two things there
already do the right thing, and both are load-bearing:

- `run_override_loop` skips a notification whose override is unchanged. Without
  that guard a redundant `notify_one()` would re-navigate the page and tear down
  the live `RTCPeerConnection` mid-cast.
- The per-item `tokio::select!` watches `override_signal`, so a cast interrupts
  the current item instead of waiting out its duration.

On teardown the previous override is only restored **if the one on screen is
still ours**. An operator who set a different override during the cast made a
newer decision, and silently reverting it would look like the UI ignoring them.

**Plain HTTP is loopback-only** (`--http-listen`, default `127.0.0.1`). That
listener exists for the display browser, which fetches assets from
`http://127.0.0.1` — already a secure context, so TLS there would buy nothing and
cost a certificate the kiosk browser has no way to trust. Everything a human
touches goes over TLS instead, which also stops basic-auth credentials from
crossing the network base64-encoded.

Do **not** be tempted to serve the display over HTTPS to drop this listener: the
self-signed certificate makes Chromium show an interstitial, and the fixes are
worse than the problem. `--ignore-certificate-errors` weakens the whole browser,
and the scoped `--ignore-certificate-errors-spki-list` breaks every time the
certificate is regenerated — which happens on a DHCP move, producing a blackout
nobody would connect to the certificate.

**The cast HTTPS port is bound up front** in `main`, before `AppState` exists, so
a clash is a startup error rather than a background task that logs and leaves
casting silently dead. An explicit `--cast-tls-port` that is taken is fatal (the
usual cause is an older instance of this binary still holding it — see the
restart note above); with no flag, the next free port after 3443 is taken and
logged. `AppState::cast_tls_port` is the port actually bound, which is what the
guest URL must be built from — not `args.cast_tls_port`.

**Operator credentials are runtime state too.** The basic-auth middleware is
always installed and decides per request, because auth can be switched on from
the admin UI. Stored passwords are PBKDF2-SHA256 hashes, so a copy of the
database is not a copy of the credentials; a password passed on the CLI stays
`Secret::Plain` and is never written. Because PBKDF2 is deliberately slow and the
admin page polls every two seconds, `AppState::auth_cache` remembers the last
`Authorization` header that verified — and **must** be cleared whenever the
credentials change, or the old password keeps working.

**HTTPS is not optional for the guest page.** `getDisplayMedia` and `RTCPeerConnection` only exist
in a secure context. The display is fine — it reaches the controller over
loopback, which counts as secure — but the sender is a laptop opening
`http://10.x.x.x:3000`, which does not. Hence the second listener on
`--cast-tls-port` with a self-signed cert. Both listeners serve the *same*
`Router` and the same `AppState`, so sender and display land in one signaling
registry and the sender never fetches cross-origin. Serving the sender page and
its API from different origins is what forced the mixed-content `/api/override`
proxy in the picklecast setup this replaces.

**`--public-url` decides what guests are told**, and `src/tls.rs` resolves it:
`none` (the LAN address), `mdns` (`<hostname>.local`, which needs Avahi —
`avahi-daemon` plus `nss-mdns` in `/etc/nsswitch.conf` — on this device *and* on
the guest's machine; startup resolves the name once and warns if it fails), a bare
host, or a full base URL for a device behind a proxy that owns the port. Whatever
name comes out is appended to the certificate SANs in `main.rs`, or guests get a
name mismatch on top of the unknown-issuer warning.

Avahi announces `<hostname>.local` by itself but nothing else, so a
`--public-url` ending in `.local` that is *not* this machine's hostname is
published by supervising `avahi-publish -a -R <name> <address>` (`src/mdns.rs`).
That is what makes a two-screen machine addressable as `kiosk2-links.local` and
`kiosk2-rechts.local` instead of one ambiguous `kiosk2.local`. Holding the record
for the lifetime of a child process is deliberate: it disappears when the
controller stops, which is the wanted behaviour, and it costs no D-Bus dependency.

Note that the machine doing the publishing may not resolve those names *itself*
if it runs Avahi for publishing and systemd-resolved for lookups — two mDNS
stacks that do not share a cache. Measured on the kiosk: `avahi-resolve` finds the
name, `curl` on the same box does not, and a guest on the LAN does. Only the last
one matters.

The QR code is rendered server-side as SVG (`GET /api/cast/qr.svg`). A
client-side library would have to be vendored for an offline device, and an SVG
scales to whatever the display is.

The idle screen (`empty_playlist.html`) shows the cast address, the QR code and —
in `code` mode — the standing code. That is the one moment the display has
nothing better to say, so it is the best place to explain how to use it. It reads
`/api/cast/state` rather than `/api/cast/info` because only the loopback endpoint
carries the code.

`src/tls.rs` regenerates the certificate when the recorded SAN list no longer
matches the machine's addresses (sidecar file `<cert>.sans`). After a DHCP move a
stale cert would fail *name* validation, which is a scarier browser warning than
an unknown issuer. The cert file holds the private key and is written `0600`.

`rustls` is pinned to the `ring` backend, not the default `aws-lc-rs`: the latter
needs a C toolchain that the armv7 `cross` image does not have.

**Session rules**, all enforced server-side (picklecast enforced none of them):

- **The code is checked, and the slot taken, before anything is shared.**
  `POST /api/cast/claim` validates and returns an opaque ticket; the socket
  carries only that ticket. Doing it the other way round — the obvious way —
  means a guest picks a window in their browser's screen picker and *then* hears
  the code was wrong, and lets two guests sit in the picker with one guaranteed
  to lose. A reservation holds for `RESERVATION_TTL`, is released on `pagehide`,
  and does **not** pin the display: the playlist keeps running until someone
  actually streams.
- Exactly one sender and one display. A second guest is refused at claim time,
  with a reason.
- Ticket admission happens *after* the WebSocket upgrade, deliberately. A socket
  rejected at the HTTP layer gives the page neither status nor body, so the
  reason would surface as nothing but "connection failed".
- `role=display` is loopback-only. That page is only ever opened by our Chromium.
- Ping every 15s, drop after 45s of silence. A laptop whose lid closes stops
  answering without ever sending a TCP FIN, so the socket looks healthy until
  something probes it — this is what keeps a dead cast from freezing the signage.
- Watchdogs: 5s grace after the sender's socket drops (so a page reload does not
  bounce the display back to the playlist), 30s for the display to connect back,
  30s TTL on a pairing code.

**A connection code on screen makes the overlay stand down.** The overlay lives in
the top layer, so it wins over anything the page draws — which means a badge at
`bottom-center` covers the pairing code rather than the other way round.
`cast_display.html` and `empty_playlist.html` therefore call `__ov.suspend()`
while they show a code and `__ov.resume()` afterwards; the configuration is kept,
so returning costs no round trip. In `code` mode the idle screen's standing code
keeps the overlay down for as long as it is up, which is the intended trade: the
one thing a guest needs to read beats a clock.

They also set `globalThis.__ovSuspend` before calling, and the runtime seeds
itself from that flag. This is load-bearing: a pairing code arrives on the socket
within a second of the page loading, while the controller injects the runtime only
after its readiness waits — so a page that could only call `suspend()` would be
covered by the very overlay it asked to stand down.

`cast_auth` picks how a guest proves themselves: `none` (trusted LAN), `code`
(fixed PIN, known out of band), or `pairing` (fresh code shown on the display for
30s, single-use).

**There is no standing pairing code.** It is minted by `POST /api/cast/pair` when
a guest asks, lives `PAIRING_TTL`, and is single-use — nothing rotates in the
background, so there is nothing permanent to display. The `code` mode's PIN is the
standing one, and the admin UI already edits it. While a pairing code *is* alive,
`/api/cast/state` carries it with its remaining seconds so the admin card can show
it: the person helping a guest by phone was otherwise the only one who could not
see what the display was showing. `/api/cast/info` still never carries it. Wrong codes are compared in constant time and lock the address
out after 5 tries.

**Settings are runtime state (see `src/settings.rs`), and the command line
always wins.** `cast_enabled`, `cast_auth`, `cast_code` and the operator
credentials live in the `settings` table and are edited from the admin UI — but a
flag that was actually passed pins that setting: `/api/settings` answers `409`
naming the flag, and the UI renders the control as locked. This is why the
CLI-settable `Args` fields are `Option<T>` with no clap default: `None` has to
mean "not given", not "given the default".

The precedence is not just deference to whoever wrote the unit file. It is the
**recovery path**: an operator who enables basic auth in the UI and forgets the
password would otherwise have locked themselves out of the only place that can
undo it. Adding `--basic-auth-user/--basic-auth-password` always gets them back
in, and `Locks` makes that visible rather than mysterious.

Read settings from `state.settings`, never from `state.args` — the args are the
input to resolution, not the answer.

`--disable-cast` additionally decides whether the HTTPS listener binds at all, so
it cannot be undone without a restart either way. It is the deployment-level kill
switch; `cast_enabled` is the operator-level one, and turning that off ends any
cast already running.

A code-auth misconfiguration (mode `code`, empty code) logs an error and refuses
every sender, but does **not** stop the process. Casting must never keep the
signage from booting, and the operator can fix it in the UI without a restart.

The sender page bounces itself from `http://` to the TLS origin when it is
reached from a non-loopback address, because `getDisplayMedia` does not exist on
the plain-HTTP origin and the failure would otherwise be silent.

Taken from the local picklecast patches: the `ontrack` guard (it fires once per
track, and the second `play()` aborts the first with `AbortError` — a naive catch
then re-mutes a stream that was playing fine) and the unmuted-first playback with
a muted fallback. Deliberately *not* taken: p2pt/WebTorrent trackers, the
tracker-vs-local transport probe, the YouTube iframe remote control, the
`--webhook` callback and the `/api/override` proxy (both unnecessary once this
lives in-process).

For unattended audio, launch the display Chromium with
`--autoplay-policy=no-user-gesture-required`; otherwise the receiver falls back
to muted playback because nobody is there to click.

**The display announces how large a frame it can show, and the sender obeys.**
A frame wider than the GPU's `MAX_TEXTURE_SIZE` decodes perfectly and then
composites as *nothing*: the receiving page is a black rectangle while
`getStats()` reports frames decoded, zero dropped, and a canvas `drawImage()` of
the same video element returns real pixels. Measured on the Raspberry Pi 3 kiosk
(Broadcom VC4, GLES 2.0, `MAX_TEXTURE_SIZE` 2048) against a 2880x1414 share.

So `cast_display.html` measures itself — `min(MAX_TEXTURE_SIZE, longest panel
edge x devicePixelRatio)` — and sends `{"type":"limits","max_edge":N}` on every
socket connect. `cast.rs` stores it, hands it to the sender in `welcome`, pushes
`display_limits` if the sender was already connected, and exposes it on
`/api/cast/info` (public) and `/api/cast/state` (operator). The sender constrains
`getDisplayMedia` up front and calls `applyConstraints` when a limit arrives
later.

Three details are deliberate:

- **The display measures, the server only relays.** Only the display knows its
  GPU and its panel. The server clamps the number to a sane range and refuses a
  `limits` frame from a *sender*, which would otherwise be uncapping its own
  stream.
- **The last known limit outlives the session** (`CastSession::display_limits`
  is not cleared on teardown). It is a property of the hardware, so remembering
  it is what lets the next sender pick the right size *before* its first frame —
  the sender captures before its socket exists, so on a first-ever cast the limit
  can only arrive after the picker has already handed over a stream.
- **The panel edge is in there too**, not just the texture limit. Pixels above
  the panel size are scaled away before anyone sees them, and on a Pi the
  bandwidth and decode time are worth more than the detail nobody can see.

### Invalid TLS certificates are accepted, on purpose

`browser_loop` connects with `ignore_https_errors: true`. Signage points at
internal dashboards on self-signed certificates, and whoever adds a playlist URL
is the one judging it trustworthy — there is no end user here to protect from
their own click. It also matches `chromiumoxide`'s own default, so this is
documenting an existing property rather than choosing a new one.

**Do not try to make this per playlist item.** It was attempted and measured:
sending `Security.setIgnoreCertificateErrors` to the adopted control page has no
effect whatsoever — the page still ends on `chrome-error://chromewebdata/`. Only
the handler's setting takes, and it is applied while its `NetworkManager`
initialises each target, i.e. once per connection.

Two traps found on the way, worth not rediscovering:

- Do **not** send `Security.enable` first. Enabling the domain switches Chromium
  into override mode, where a certificate error becomes a
  `Security.certificateError` event and the load blocks until someone answers
  `handleCertificateError`. Nobody does, so the interstitial stays up and the
  flag looks inert.
- Flipping the handler setting at runtime needs a reconnect *and* a page that is
  not already on the target URL, or `navigate_page` takes its same-document
  branch and reloads without re-running the certificate decision.

`examples/certtest.rs` is the reproduction: it takes a URL and a mode (`none`,
`explicit`, `adopt`, `adopt-explicit`, `wait`).

## Database

SQLite, path from `--database-path` (default `miniclient.db`, gitignored).
Tables: `assets`, `playlist_items`, and `settings` (key/value, for the cast
options the operator can change without a restart — key/value rather than columns
because they are a handful of unrelated scalars).
Schema lives **only** in `src/db.rs::run_migrations`, which is idempotent:
`CREATE TABLE IF NOT EXISTS` plus `pragma_table_info` probes before each
`ALTER TABLE ADD COLUMN`. Add new columns the same way. `main.rs` must not
create tables itself — a partial duplicate there caused schema drift.

`PRAGMA foreign_keys` is enabled per connection via `SqliteConnectOptions`
(it is off by default in SQLite, so `ON DELETE CASCADE` was previously a no-op
and deleting an asset left orphaned playlist rows).

Read paths use `COALESCE(p.scroll_config, '…')`: a `NULL` in that column makes the
whole `query_as` fail, and both call sites swallow the error into an empty
playlist, so one bad row would blank the screen.

## API

| Method | Path | Notes |
|---|---|---|
| GET/POST | `/api/assets` | POST is multipart; each field is one file |
| PUT/DELETE | `/api/assets/{id}` | PUT body `{ duration }` |
| GET/POST | `/api/playlist` | |
| PUT/DELETE | `/api/playlist/{id}` | PUT also takes `url` / `asset_id`, see below |
| POST | `/api/playlist/{id}/move` | `{ direction: "up" \| "down" }`, renumbers the list |
| GET/POST | `/api/control/current` | POST `{ item_id }` = play now |
| GET/POST/DELETE | `/api/override` | POST `{ asset_id? , url?, scroll_config? }` |
| GET | `/api/cast/ws` | WebSocket signaling, `?role=sender\|display[&code=]` |
| GET | `/api/cast/info` | Public: `{ enabled, auth, busy, sender_url }` |
| GET | `/api/cast/qr.svg` | Public: QR code for the guest URL |
| GET | `/api/cast/state` | Operator/loopback: who is casting, since when |
| DELETE | `/api/cast/session` | Operator: end the cast now |
| POST | `/api/cast/pair` | `--cast-auth=pairing` only; shows a code on the display |
| GET/PUT | `/api/settings` | Operator: runtime settings + which flags pinned them |
| GET/POST | `/api/audio` | Operator: room audio, whether or not a cast runs |
| GET | `/api/overlay` | The layers the display runtime wants: global + the item on screen |
| POST/DELETE | `/api/cast/claim` | Guest: reserve the session before sharing |

`PUT /api/playlist/{id}` also takes an `overlay` object (see the overlay section):
it is stored as SQL `null` unless it would actually draw something, so the read
paths never have to tell "switched off" from "empty". The column defaults to the
JSON string `'null'` rather than SQL `NULL`, for the same reason `scroll_config`
is `COALESCE`d — a real `NULL` fails to decode, which fails the whole query, and
both call sites swallow that into an empty playlist.

`PUT /api/playlist/{id}` may change an item's source, but only like for like: a
URL item takes a new `url`, an asset item a new `asset_id`. The opposite is a
`400` — a kind change would need the other column cleared in the same write, and
`playlist_target_url` picks one column over the other silently, so a half-changed
row plays the wrong thing with no error anywhere. `asset_id` is checked against
`assets` first; a dangling id yields a `NULL` `local_path` from the loop's
`LEFT JOIN` and a blank screen.

`POST /api/playlist/{id}/move` renumbers every row to `1..n` instead of swapping
two values. `play_order` is typed by hand in the UI, so duplicates and gaps
accumulate, and a pairwise swap between two rows sharing an order does nothing.

Handlers that can reject a request answer with `{ "error": "..." }`; the UI shows
that string inline. Everything else stays on the "swallow and log" rule below.

Nullable-clearable fields (`start_date`, `end_date`) use
`Option<Option<String>>` with `#[serde(default, deserialize_with = "double_option")]`
in `handlers.rs`. Without the custom deserializer a JSON `null` collapses to the
outer `None` and the field can never be cleared.

## Conventions

- All handler DB errors are swallowed (`let _ = …` / `unwrap_or_default`) so the
  display never dies on a bad request. Keep that, but `error!`-log first.
- The UI is dependency-free vanilla HTML/JS. Build rows with `textContent` /
  `createElement`, not `innerHTML` string interpolation — filenames and URLs are
  attacker-influenced. `playlist.html` funnels this through a small `el()` helper.
- `playlist.html` polls `/api/control/current` and `/api/override` every 2s and
  only updates badges and highlight classes from the poll. It must not re-render
  the item list on a tick: the cards *are* the edit form, so a re-render would wipe
  whatever the operator is typing. Cards with unsaved edits are tracked in a
  `dirty` set and carried over verbatim across list reloads for the same reason.
- `duration` is in seconds and comes from the DB as `i64`; clamp before casting to
  `u64` (a negative value became ~584 billion years of `Duration` and froze the
  playlist on one item).
- **`assets.duration` is the asset's own default, not the playlist entry's.**
  `browser.rs` resolves `item.duration.or(item.asset_duration).unwrap_or(10)`: the
  entry wins, the asset is the fallback, ten seconds is the last resort. It exists
  so a PDF carries its natural dwell time and nobody retypes it every time that
  asset is scheduled. Mostly dormant in practice — `playlist.html` always sends a
  duration with the item, so it only decides for items created through the API
  without one.
