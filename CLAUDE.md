# CLAUDE.md

Guidance for Claude Code when working in this repository.

**Prose documentation for humans lives in [`docs/`](docs/README.md), and this file
does not repeat it.** What is here is the other kind: the invariants and the traps
— things that are cheap to break, expensive to notice, and usually invisible on a
screen nobody is standing in front of. Where the *reason* is longer than the rule,
the rule is here and the reason is a link.

| Want | Read |
|---|---|
| What the thing does | [docs/features.md](docs/features.md) |
| How it is put together, module map | [docs/architecture.md](docs/architecture.md) |
| Casting: signaling, auth, quality, audio | [docs/casting.md](docs/casting.md) |
| Building, cross-compiling, shipping, units | [docs/deployment.md](docs/deployment.md) |
| Device quirks and measurements | [docs/raspberry-pi.md](docs/raspberry-pi.md) |
| Symptoms and what they mean | [docs/troubleshooting.md](docs/troubleshooting.md) |
| The HTTP API | [README.md](README.md) |

## Working in this repository

**`web/` is compiled into the binary** via `include_dir!` (`src/web.rs`). After
changing anything under `web/`, rebuild — it is not read from disk, and an
unrebuilt change looks exactly like a change that did not work.

`cargo build` is the gate for the Rust side. There is no linting config.

**Stop any locally running instance before the Python suite.** `test_port.py`
needs the default `3443` free to test the fallback, and
`test_browser.py`/`test_overlay.py`/`test_webhook.py`/`test_display.py` launch
their own Chrome on `9222`+`9223`/`9232`/`9242`/`9242`+`9243`. A dev instance
holding those makes them fail in a way that reads exactly like a code regression —
they pass again the moment it is stopped. `test_display.py` and `test_webhook.py`
share `9242`, so they must not run concurrently.
A stray Chrome on `9222` is worse than none for `test_webhook.py`: the loop
attaches to it and injects `display.connected` and `playback.playlist_empty`
deliveries into receivers that are counting.

`tests/cast/` is stdlib-only Python, wired into no CI. Run it by hand after
touching `cast.rs`, `tls.rs`, `settings.rs`, `audio.rs`, `webhook/`,
`display.rs`, `playlists.rs` or the route table.

When testing casting locally, share a single *window* rather than the whole
screen, or the display shows an infinite mirror.

## HTTP: three audiences, two predicates

The most important thing to hold on to when touching routes or middleware. The
table of who reaches what is in
[docs/architecture.md](docs/architecture.md#three-audiences-for-http); these are
the rules that break it:

- **`is_display_path`** is loopback-scoped. The display browser is driven over CDP
  and cannot present credentials, so requiring them there blanks the signage.
- **`cast::is_cast_public_path`** is exempt **regardless of address**, because the
  guest is by definition not loopback.
- **Keep the two separate.** Widening `is_display_path` exposes display-only paths
  to the whole LAN; narrowing the cast list locks guests out.
- Both listeners must be started with
  `into_make_service_with_connect_info::<SocketAddr>()`. The loopback exemption in
  `basic_auth_middleware` reads the peer from `ConnectInfo<SocketAddr>`, and **the
  extractor panics without it** — including on the TLS listener.
- **`/api/displays*`, `/api/playlists*` and `/displays.html` are operator-only**,
  in neither list. The display browser never asks which screen it is — the
  controller drives it — and a guest has no business reading the venue's screens.
- **The site root is the guest page**, not the operator page; `/admin.html` is the
  operator landing page and `/cast.html` 301s to `/`. Moving a page between the
  two sets means moving it between `is_cast_public_path` and authenticated
  routing.

Room audio is the case that sits on both sides at once: `/api/cast/audio` is on
the cast list and checks `caster_only`; `/api/audio` is not, so basic auth
decides. **Do not widen `caster_only` to admit the operator** — that route is
exempt from auth, so it would admit the whole LAN. Both end in
`cast::apply_audio`, which is what keeps them from drifting.
**Loopback is not a way around auth**: the exemption covers `is_display_path`
only, so the admin page asks for credentials even on the device itself.

## Displays (`src/display.rs`)

The concept and the operator's side:
[docs/features.md](docs/features.md#several-screens); declaring them:
[docs/deployment.md](docs/deployment.md#declaring-the-screens-a-deployment-drives).
The rules:

**Displays are declared, not discovered.** Discovery was prototyped against sway
1.12 and *works* — `swaymsg -t get_outputs`, `[app_id="…"] move container to
output …`, and an unplugged output hands its window on rather than losing it. It
was dropped anyway, because it puts compositor-specific knowledge inside the
controller (`HDMI-A-1` under sway, `HDMI-1` under i3) and takes placement away
from the window manager, which is where this project already puts it. What the
prototype did establish is the thing this design rests on: **`--chromium-class`
becomes the Wayland `app_id`**, which is what lets the window manager tell our
windows apart with no help from us.

**One Chromium per display, not one Chromium with several windows.** Measured:
515 MB PSS for one browser with two windows against 494 MB for two browsers with
one each — Chromium's cost is per renderer, not per browser process. One browser
would also have cost the placement mechanism, because every window of one process
shares an `app_id`, and `Browser.setWindowBounds` does not move a window under
native Wayland: the size takes effect and the position does not.

**`--display` is the identity**, stored against the assignment so it survives a
restart — which is why it cannot be an index. A name reaches a window-manager
config *and* a filesystem path, so it is restricted to `[A-Za-z0-9_-]`.
The CDP port is `<name>:<port>` when given, otherwise `9222 + declaration index`,
counted from the base — so pinning one display's port does not shift another's.

**With no `--display`, behaviour is exactly as before**: one implicit display
named `default`, using `--cdp-url` and the port-derived class and profile. These
run unattended in venues; an upgrade must not move a CDP port.

**Alongside any `--display`, three flags are refused rather than ignored**:
`--chromium-class`, `--chromium-user-data-dir` and a non-default `--cdp-url`. The
declared branch derives all three and never reads them back, so honouring the
command line by silently doing nothing is the exact failure "a flag actually
passed pins that setting" exists to prevent. The `--cdp-url` comparison is whole,
not by port — `http://192.168.1.5:9222` names a different host on the default
port, and a port-only check waves it through to be silently rewritten to
loopback. `--class` inside `--chromium-arg` is refused for a
different reason: Chromium is last-wins for a repeated switch, so it would
collapse every display onto one `app_id`. All of this is **below** the no-display
early return on purpose: with one implicit screen there is nothing to collide
with, and refusing there would turn a working command line into a refusal to boot.

**An unknown `--cast-display` fails at startup**, not at use.
`AppState::cast_display()` falls back to the primary rather than panicking, so
without the check the mistake surfaces hours later as a guest scanning a QR code
and the picture landing on the wrong panel.

**An unscoped legacy API path refuses with `409` once several displays exist**
(`display::resolve` with `None`), naming the declared screens. It still resolves
to the one display while one is declared, because that is every venue running
today and an upgrade must keep their scripts working. Picking one of several
would be a coin flip a script cannot see.

**A playlist is never deleted with a display**, and the schema enforces it:
`displays.playlist_id` is `ON DELETE SET NULL`, so deleting a playlist unassigns
it and deleting a display cannot reach the playlist at all. The other direction is
the handler's, because no `ON DELETE` would do it: deleting a playlist that still
holds items is a `409` that says how many.

**`settings.rs` pokes every display's `overlay_signal`** — via
`AppState::notify_overlay_changed`, and playlist edits likewise go through
`notify_playlist_changed`. The overlay configuration is global and an edited item
can be in a playlist two screens share, so both have to reach every screen. Same
rule as ever: `notify_one`, never `notify_waiters`.

**Each loop re-reads its assignment every inner pass** (`SELECT playlist_id FROM
displays WHERE name = ?`), so a reassignment lands on the next item; the `PUT`
pokes `playlist_signal` as well so it lands now rather than at the end of a
ten-minute item. An assignment that *changed* restarts at the playlist's
beginning rather than resuming at a `play_order` that means nothing in the new
list.

**`displays.assignment_decided` separates "nobody has chosen yet" from "somebody
chose none".** Both are `playlist_id IS NULL`. It is stored rather than inferred
from the insert because the question outlives the process — a fresh install has no
playlist to inherit at startup, so `offer_first_playlist` picks the decision up
when the operator creates the first one — and it is written in the *same*
statement as every assignment, so a power loss cannot leave a screen that will
never be offered a playlist. Inheritance is only for **one** declared display: it
exists to carry an upgrade across, and handing the foyer a playlist made for the
workshop is wrong content, which reads as deliberate, where an idle screen reads
as "configure me".

## The control loop (`src/browser.rs`)

Shape of the three nested loops:
[docs/architecture.md](docs/architecture.md#the-control-loop). **`browser_loop` is
spawned once per declared display and takes its `Arc<Display>`**; everything below
is per screen, and the playlist it reads is the one that display is assigned. The
loop **owns what is on screen**; the API writes state and pokes a signal, and
never navigates.
Any `is_connection_lost` error breaks all the way back to the outer loop and
reconnects; `keep_loaded` tabs are reconciled once per inner pass.

**Always `notify_one()`, never `notify_waiters()`** on a `Display`'s notifies
(`skip_signal`, `playlist_signal`, `override_signal`, `overlay_signal` — they live
on `Display`, not on `AppState`). The loop is only parked for part of its cycle —
navigation and readiness waiting can take ten seconds or more — and
`notify_waiters()` drops a notification when nobody is parked, silently losing a
"Play now" click. `notify_one()` stores a permit.

**`POST /api/control/current` writes the resolved display's `pending_jump`, not
its `current_item_id`.** `current_item_id` is loop-owned and overwritten at the
top of every item, so a request writing there is clobbered.

**Never consume `pending_jump` before the target has been found in a freshly
fetched playlist.** The loop *peeks* on `skip_signal`: target in the current
snapshot, jump now; otherwise break out, re-read the playlist, resolve against the
fresh list, and only then clear it. `take()`ing it on the miss path silently
dropped the click and resumed on an unrelated item — that was "Play now plays
something random".

**The snapshot is stale by design.** It is read once per inner-loop pass and
iterated item by item, so it can be a whole item duration old. Anything that must
reflect an edit made *during* the current item has to be re-read, not taken from
it. Two things already depend on this: `pending_jump` above, and
`db::load_item_overlay`.

`item.duration.or(item.asset_duration).unwrap_or(10)` — the entry wins, the asset
is the fallback, ten seconds is the last resort.

## Injected runtimes

Both the scroll runtime and the overlay runtime are registered with
`Page.addScriptToEvaluateOnNewDocument` **and** re-evaluated after navigation.
That is not belt-and-braces: a strict CSP can block the registered copy. Both are
probed rather than assumed — `apply_scroll_settings` checks `!!globalThis.__as`,
`apply_overlay` checks `!!globalThis.__ov`, and both no-op when it is missing.

`web/autoscroll.js` is served over HTTP *and* `include_str!`-ed into the binary
(`scroll_runtime_script()`). Same for `web/overlay.js`. Editing one copy is
editing both; forgetting to rebuild ships neither.

PDFs are the exception: `web/pdf_viewer.html` drives its own scrolling from query
parameters, so `browser.rs` detects it with `is_internal_pdf_viewer_url` and skips
`start_scrolling`/`stop_scrolling`.

**A page that shows a connection code sets `globalThis.__ovSuspend` *before*
calling `__ov.suspend()`, and the runtime seeds itself from that flag.** This is
load-bearing, not defensive: a pairing code arrives on the socket within a second
of the page loading, while the controller injects the runtime only after its
readiness waits — so a page that could only call the method would be covered by
the very overlay it asked to move.

### pdf.js

`web/pdf.min.js` and `web/pdf.worker.min.js` are vendored (v3.11.174) and served
from the same origin. **Never point `workerSrc` at a CDN** — the device is often
offline, and the operator page would work while the display silently failed.

## The overlay (`web/overlay.js`)

What it does and why it looks the way it does:
[docs/features.md](docs/features.md#overlay). The two sources are
`settings.overlay_config` (global) and `playlist_items.overlay_config` (the item),
and they are **additive**: the runtime takes a list of layers, global first.
Layers naming the same corner share one box and the first of them decides how it
looks, so the style is not per layer. An item that names no corner joins the
global box.

**The one exception is `ItemOverlay::color`, and it recolours the box rather
than the layer.** One bright page in a dark playlist makes the *global*
overlay's white clock unreadable, so an override reaching only the item's own
text would miss the thing that prompted it. Resolved entirely in
`settings::overlay_payload`, which stamps the colour onto the first layer of the
target box — `overlay.js` needs no notion of it, and style resolution stays in
one place. Three rules hold it together:

- **`draws()` and `recolours()` are separate predicates.** A colour with no text
  or QR beside it is valid — a bright page often wants a readable house clock
  and no badge — and must not push an empty layer. `matters()` is the pair, and
  it is what the two storage sites filter on: an item overlay is SQL `null`
  unless it would draw *or* recolour.
- **A colour-only item recolours the global box whatever corner it names.** With
  nothing drawn it has no box of its own, so its `position` is meaningless and
  honouring it would be a stored setting that silently does nothing.
- **Only the hue is negotiable.** `color_alpha`, the background and the sizes
  stay global; an item that could restyle the box completely is the display
  changing character item by item, which the global-only style was protecting.
  An unparseable colour falls back to inherit, like an unknown corner.

The global QR has a **source**, not just a text: `qr_source` is `text` (use
`qr_text`) or `cast` (resolve the guest URL when drawing). With casting switched
off the `cast` source draws nothing — advertising a way to share a screen that
refuses every sender is worse than silence. The admin UI's third choice `keiner`
is **not a server-side value**; it clears `qr_text` with source `text`.

The code-level rules:

**There is no CDP command that draws over a page.** An overlay is always DOM in
the target document, which is what the five defences are for — remove one and the
failure is silent:

- **Shadow DOM plus `all: initial`**, or the target page's CSS decides how the
  notice looks and a stray `div { display: none }` makes it vanish.
- **The rules travel as a constructable stylesheet** (`new CSSStyleSheet()` +
  `shadow.adoptedStyleSheets`), never a `<style>` element. **A shadow root is not
  a CSP boundary.** A nonce-based `style-src` with no `'unsafe-inline'` — what
  Next.js sends by default — drops an appended `<style>` while leaving the
  element in the DOM: `styleEl.sheet` is `null`, the text is still on screen and
  every rule is gone, so the badge reads as unstyled on that one playlist item
  and nowhere else. CSP governs style elements and style attributes, not the
  object model, so CSSOM gets through. `host.style.cssText` in `ensureHost` is
  CSSOM for the same reason and was never affected — which is why the box stayed
  put while its contents lost everything. Covered by case [46a] of
  `tests/cast/test_overlay.py`, which asserts the *computed* style: the `<style>`
  node was present the whole time the bug existed.
- **The top layer, via `popover="manual"`.** A fullscreen element covers every
  z-index there is. `manual` and not `auto`: an auto popover closes on the next
  Escape or outside click, and this one is not the page's to dismiss. The
  `z-index` in the host style is only the fallback for a browser without popover
  support.
- **A `MutationObserver` that re-attaches it**, because SPAs replace whole
  subtrees.
- **Nothing is fetched.** Chromium's Local Network Access refuses a request to
  `127.0.0.1` from any origin that is not itself loopback, and a kiosk has nobody
  to click the prompt. Measured on Chrome 151 from `https://example.com`: `fetch`
  gives `TypeError`, an `<img>` gives `EncodingError`.

So the QR travels as a **module matrix** (`qr_modules`, one `0`/`1` string per
row, `cast::qr_matrix`) and is drawn as inline SVG, which also survives an
`img-src` CSP that would refuse even a `data:` URI. An image is the one thing the
overlay cannot draw itself, so it travels as a `data:` URI capped at
`OVERLAY_IMAGE_MAX_BYTES` (512 KB); `check_overlay_image` refuses an oversized or
non-image asset when it is *picked*, so the API and the payload builder cannot
disagree.

**The test for this must serve its foreign page from the machine's LAN address,
not `127.0.0.1`.** Loopback-to-loopback is not gated, so a foreign page on
`127.0.0.1` passes while a real display fails. That blind spot is how the first
version shipped a QR that only worked after a permission click.

**`GET /api/overlay` is the primary display's.** It resolves the item on screen
through `state.primary()` and is not display-scoped, so with several screens
declared the admin preview shows whatever the *first* one is showing. The overlay
configuration itself is global, so only that one line of the preview is affected —
do not "fix" it by widening `overlay_payload`.

`settings::overlay_payload` is the **only** place that builds the runtime's
configuration. The admin preview fetches `/api/overlay` and runs the display's own
runtime against the identical payload rather than rebuilding the badge in the
page — two renderings of the same settings drift, and then somebody hunts a
display bug that is really a UI bug.

`Display::overlay_signal` is what makes an edit land on the item *already* on
screen — poked for **every** screen through `AppState::notify_overlay_changed`,
because the global overlay is the building's and an edited item can be on any
screen showing its playlist. `PUT /api/playlist/{id}` pokes it too, because the
item being edited may be the one showing. **All three places the loop can park
must handle it**: the
per-item `select!` (which recomputes the remaining time rather than restarting it,
so an overlay edit cannot extend an item), the idle-screen wait, and
`run_override_loop` — a cast or a pinned page can stand for hours, which is
exactly when a notice matters, and re-applying touches nothing else so a live
`RTCPeerConnection` survives it.

The cast-related conditions: the switch is `hide_during_cast`, and the
cast-sourced QR is dropped independently of it. **The test is
`CastSession::is_active()`, not "a sender is connected"** — during the grace
period after a sender's socket drops the cast page is still on screen, and an
overlay blinking back for those seconds would look like a fault.

Content aligns with the corner it sits in (`alignmentFor`): a `-center` position
centres, `-right` right-aligns, everything else stays left. The QR row is flex, so
`text-align` does not reach it and the same alignment is spelled out as
`justify-content`. **Deliberately no separate alignment control** — a box pinned
centre-bottom with left-aligned text is a mistake, not a choice worth offering.

Storage rules: the box style is structured (`background_color` +
`background_alpha`, `color` + `color_alpha`, a `plain` flag), not a CSS string,
because the browser drops an invalid declaration without a word. `background_css`
survives as an escape hatch and `sanitize_css_value` keeps it a *value* — no `;`,
braces, quotes or `@`, 200 characters. `Overlay::migrate_legacy_style` converts
old configurations on load — a stored `rgba(...)`, `#rrggbb` or `#rrggbbaa`
becomes colour plus alpha, an unparseable value becomes `background_css` rather
than being thrown away, and the former whole-box `opacity` folds into the
background alpha. Both legacy fields are `skip_serializing`, so they leave the API
on the first write-back.

An enabled overlay with nothing in it is a `400`. Out-of-range numbers are
**clamped rather than rejected** and an unknown corner falls back — the
alternative is an error message on a screen nobody is standing in front of.

## Casting (`src/cast.rs`, `src/tls.rs`)

The subsystem in full: [docs/casting.md](docs/casting.md). The controller relays
opaque `{sdp}` / `{ice}` blobs and **never parses WebRTC payloads** — not parsing
them means it cannot break them.

**A cast is an override.** Starting one pins the *cast display's* `override_item`
to `http://127.0.0.1:<port>/cast_display.html`. Which screen that is comes from
`AppState::cast_display()` — `--cast-display` when given, else `primary()` — and
**every part of a cast resolves through it**, not through `primary()`: activation,
teardown, the `override.set` emit and `cast.started`. Casting is still one session
for the whole controller, not one per screen, and the guest URL, the QR code, the
idle screen's invitation and `is_active()` stay global with it — what that leaves
a second screen with is spelled out in
[docs/casting.md](docs/casting.md#which-screen-a-cast-lands-on).
`browser.rs` needed no changes for this, because two things there already do the
right thing and both are load-bearing:

- `run_override_loop` skips a notification whose override is unchanged. Without
  that guard a redundant `notify_one()` re-navigates the page and tears down the
  live `RTCPeerConnection` mid-cast.
- The per-item `select!` watches `override_signal`, so a cast interrupts the
  current item instead of waiting out its duration.

On teardown the previous override is restored **only if the one on screen is still
ours**. An operator who set a different override during the cast made a newer
decision.

### The managed certificate (`src/managed_cert.rs`)

What it is and why: [docs/casting.md](docs/casting.md#a-real-certificate-for-a-private-address).
The rules:

- **`cast::sender_url` is the only place that answers "what are guests told".**
  It branches on `AppState::managed_cert`. `main` logs the startup banner through
  it too — a second copy of that resolution drifts, and then the first line an
  operator reads disagrees with the QR code.
- **The name is recomputed, never stored.** `tls::managed_name()` follows the
  machine's current address, so a DHCP move renames the device and needs no new
  certificate — the wildcard already covers it. Caching the name would strand it.
- **A managed certificate skips `subject_alt_names`/`generate`/the `.sans`
  sidecar entirely.** Its SANs are not ours to choose, and running the
  regeneration check against them would throw the certificate away on every
  start.
- **`Bundle::covers` is checked before use**, on the fetched *and* the cached
  copy. Serving a certificate for the wrong name is worse than falling back: the
  browser warning is scarier and nothing says why.
- **A wildcard covers exactly one label.** That is why the address is encoded as
  one — `dashed_label` refuses an IPv4-mapped IPv6, which would render with dots.
- **Never fail hard.** No address, no network, a bad response: fall back to
  self-signed. Casting must not keep the signage from booting.
- Renewal uses `RustlsConfig::reload_from_pem`, not a restart — a restart
  navigates the display browser, and a failed swap must leave the old
  certificate serving.
- **Not `reqwest`**: its `rustls` feature hard-wires `aws-lc-rs`, which needs a C
  toolchain the armv7 `cross` image does not have. hyper + `tokio-rustls` +
  `webpki-roots` is the combination that cross-compiles. Verify with `cross
  build` before assuming any new HTTP dependency is free.
- The Python harness passes `--managed-cert off` by default. Without that every
  unrelated test in `tests/cast/` would need the network and a private address.

**`AppState::cast_tls_port` is the port actually bound — build the guest URL from
it, never from `args.cast_tls_port`.** With no flag the listener prefers 443 and
falls back to 3443, so the bound port is even less predictable than before;
`authority()` drops `:443` from the URL, which is the entire reason for
preferring it. The socket is bound in `main` before
`AppState` exists, so a clash is a startup error rather than casting silently
dead.

**Plain HTTP is loopback-only** (`--http-listen`). Do **not** be tempted to serve
the display over HTTPS to drop that listener: the self-signed certificate makes
Chromium show an interstitial, `--ignore-certificate-errors` weakens the whole
browser, and the scoped `--ignore-certificate-errors-spki-list` breaks every time
the certificate is regenerated — which happens on a DHCP move, producing a
blackout nobody would connect to the certificate.

Both listeners serve the **same** `Router` and the same `AppState`, so sender and
display land in one signaling registry and the sender never fetches cross-origin.
Splitting them is what forced the mixed-content `/api/override` proxy in the
picklecast setup this replaces.

The idle screen reads **`/api/cast/state`, not `/api/cast/info`**, because only
the loopback endpoint carries the code. The same split holds for a live pairing
code: `state` carries it with its remaining seconds, `info` never does.

The timers are named constants: `RESERVATION_TTL` for a claim that has not
streamed yet, `PAIRING_TTL` for a minted pairing code.

`--public-url` is resolved by `src/tls.rs`, and whatever name comes out **must be
appended to the certificate SANs** in `main.rs`, or guests get a name mismatch on
top of the unknown-issuer warning. `src/mdns.rs` publishes a `.local` name that is
not this machine's hostname by supervising `avahi-publish`; holding the record for
the lifetime of a child process is deliberate, so it disappears when the
controller stops and costs no D-Bus dependency.

The certificate is regenerated when the recorded SAN list stops matching the
machine's addresses (sidecar `<cert>.sans`). The file holds the private key and is
written `0600`.

`rustls` is pinned to the `ring` backend, not the default `aws-lc-rs`, which needs
a C toolchain the armv7 `cross` image does not have. The same reasoning keeps
`libpulse-binding` out of `src/audio.rs`.

### Guest pages

A guest may put a web page on the display instead of casting, when
`guest_pages_enabled` says so. Why and what it costs:
[docs/casting.md](docs/casting.md#a-guest-showing-a-page). The rules:

- **The claim carries the mode** (`cast` or `page`), not the socket.
  `register_peer` activates the display the moment a sender's socket arrives —
  deliberately, so a sender whose socket fails never interrupts the playlist —
  and a page-mode sender must not pin `cast_display.html` on its way to the
  guest's URL. **`watch_display_arrival` must not run for a page** either: there
  is no display peer coming, and it would tear the page down on its deadline.
- **`authorize_sender` gates on the capability the mode asks for**, never on
  `cast_enabled` alone. The two switches are independent, and a device too weak
  for WebRTC can still render a page.
- `activate_display`/`deactivate_display` take what they install. The
  still-ours check on teardown compares against `session.showing`, not against
  the cast page.
- **Credentials in a guest URL reach the browser and nothing else.** Everything
  that logs or displays one goes through `guest_page::redact`. Refusing them
  outright would be theatre (`?token=` is equivalent) and would break the
  internal-dashboard case that allowing LAN targets exists for.
- The grace period follows what is showing: `PAGE_GRACE`, not `SENDER_GRACE`.
  The keepalive is a protocol-level ping, so backgrounding a tab does not end a
  session; discarding it does.
- `is_active()` covers a page as well as a cast, which is what makes
  `hide_during_cast` and the cast-QR drop apply to both. Do not narrow it.
- The Python harness passes `--managed-cert off` and `--guest-pages off` by
  default, so unrelated tests need neither the network nor this feature.

### Display limits

`cast_display.html` measures `min(MAX_TEXTURE_SIZE, longest panel edge ×
devicePixelRatio)` and sends `{"type":"limits","max_edge":N}` on every socket
connect. A frame above the GPU's texture limit decodes perfectly and then
composites as *nothing* — a black rectangle while `getStats()` reports frames
decoded and zero dropped. Three rules:

- **The display measures, the server only relays.** `cast.rs` stores the number,
  hands it to the sender in `welcome`, and pushes `display_limits` if the sender
  was already connected; the sender constrains capture up front and calls
  `applyConstraints` when a limit arrives later. The server clamps to a sane range
  and **refuses a `limits` frame from a sender**, which would otherwise be
  uncapping its own stream.
- **The last known limit outlives the session** (`CastSession::display_limits` is
  not cleared on teardown). It is a property of the hardware, and the sender
  captures before its socket exists, so on a first-ever cast the limit can only
  arrive after the picker has already handed over a stream.
- **The panel edge is in there too**, not just the texture limit.

Why the receiver falls behind rather than failing, and the codec lever left
alone: [docs/casting.md](docs/casting.md#quality-who-decides-what) and
[docs/raspberry-pi.md](docs/raspberry-pi.md).

### Taken and not taken from picklecast

Taken: the `ontrack` guard (it fires once per track, and the second `play()`
aborts the first with `AbortError` — a naive catch then re-mutes a stream that was
playing fine), and unmuted-first playback with a muted fallback.

Deliberately **not** taken, so do not add them back: p2pt/WebTorrent trackers, the
tracker-vs-local transport probe, the YouTube iframe remote control, the
`--webhook` callback, and the `/api/override` proxy.

`src/webhook/` is not that callback returning. Picklecast's was glue between two
processes — picklecast telling the controller a cast had started so it could flip
an override — and it is dead because both sides live in one binary and the
override is now a function call. This is the same shape pointed the other way:
the controller telling a *third party*, re-introducing no second process and
putting nothing on the display path that waits.

## Webhooks (`src/webhook/`)

What they are and what an operator sees:
[docs/features.md](docs/features.md#webhooks). The credential question:
[docs/deployment.md](docs/deployment.md#a-webhook-target-may-hold-somebody-elses-secret).
The rules:

**The envelope names the screen**: `display` sits beside `device`, which is why
`fire` takes the display name as its first argument and every emit site has to
pass the screen the event is about — `browser.rs` its own loop's display,
`cast.rs` the cast display. It is in the envelope rather than in one event's
`data` because all ten events are about a particular screen, and it is additive,
so a target configured before several displays existed keeps working.
`api::catalogue()`'s `envelope` array carries it, so the admin page's chips offer
it without being told.

**`Dispatcher::fire` is synchronous, infallible, and returns `()`.** `browser.rs`
calls it from inside the control loop, so a version that could block, await or
error would put a stranger's HTTP server in the path of what is on the screen. It
builds the payload, spawns, and returns. Keep all three properties: an `async fn`
or a `Result` here would be an `.await` or a `?` at a call site that owns the
display.

**Past `MAX_INFLIGHT` an event is dropped, not queued.** `try_acquire_owned`
failing logs a `warn!` and skips that target. The alternative is unbounded
`spawn` on a Pi, and `playback.item_changed` against a receiver that has begun to
hang is exactly the shape that parks thousands of tasks holding sockets. Dropping
is honest — the contract is already one attempt and no retry.

**The permit is bound as the *first* statement of the delivery task**
(`let _permit = permit;`). `let _ = permit;` at the end does **not** work: a
wildcard `let` is not a read, so under Rust 2021 disjoint capture the async block
never captures the permit at all. It drops at the end of the loop iteration,
before the future is first polled, and the whole bound goes silently inert —
which shipped once and was found only by a reviewer reading the capture rules.
Nothing may be inserted above it.

**Redirects are refused, never followed.** Following one would send the target's
`Authorization` header to a host the operator never configured, chosen by whoever
controls the receiver. A `3xx` *with* a `Location` is reported as
`Outcome::Redirect` so the operator can fix the row; one without has nowhere to
send anyone and is just a status.

**Every URL reaching an event goes through `guest_page::redact`** — the emit site
does it, never the dispatcher, which is why `Event` takes `String` and not `Url`.
`browser.rs::redact_str` is the wrapper for a URL that is only ever a string, and
`playback.item_changed` redacts `title` as well as `url`, because a URL item's
title *is* its URL.

**Header *values* are templates too**, so `validate` compile-checks them exactly
like the body — otherwise a value that cannot compile saves with a `200` and then
fails on every delivery. The header *name* is never templated, but it is checked
with the same `HeaderName::from_bytes` hyper uses at send time, so the operator
gets a readable message instead of a dispatch log nobody is watching.
`content-length` is set last and is not overridable: a target naming it would
otherwise put two conflicting ones on the wire, which is the request-smuggling
shape if a proxy sits in between.

**A template is bounded three times, and the three are not interchangeable.**
`api::MAX_TEMPLATE_BYTES` (16 KB) is about the *stored row* — it keeps a
pathological request from growing the `webhooks` table on a device whose SD card
is the component that dies. `RENDER_FUEL` and `MAX_RENDER_BYTES` are about the
*device*: 78 bytes of nested `for` loops over `range` pass the row cap
comfortably and rendered 10 MB in 493 ms before fuel was turned on, with 10 GB
one digit away. Fuel bounds the work, the byte cap bounds the output, and one
cheap instruction can still emit megabytes — so removing either is not covered by
the other two. Fuel is also the *only* bound on how long a render runs: `render`
is synchronous, so no `tokio::time::timeout` around it could ever fire.

**Those three do not bound interpreter memory, and nothing here does.** A value
materialised and never written is invisible to both: fuel counts instructions and
the cap counts bytes *written*, so eight `{% set v = "x" * 100000000 %}` lines —
244 bytes, a few dozen fuel, four bytes of output — peak at 861 MB, measured,
which is an OOM on a 740 MB device with no swap. minijinja's own ceiling on a
repeated string is 100 MB, so this is bounded only by how many times an operator
repeats the trick. It takes a deliberately hostile template rather than a mistake,
and the control is the same one the rest of the operator API relies on — configure
basic auth (see
[docs/deployment.md](docs/deployment.md#a-webhook-target-may-hold-somebody-elses-secret)).
Do not read the three caps above as a complete account of what a template can
cost.

**`| tojson` is the rule for every field, not just the strings.** A bare boolean
renders Jinja2-style as `True`, which is invalid JSON, and a number that happens
to render correctly is exactly the "happens to" that drifts. The server publishes
`field_prefix` and `placeholder_suffix` from `/api/webhooks/events` so the admin
page *composes* a placeholder from the server's rule rather than reimplementing
it. Autoescape is off (HTML escaping turns `&` into `&amp;` inside a JSON string)
and undefined is lenient (a template written for one event is routinely
subscribed to another, and a missing title must not silence a
display-disconnected notice).

**`api::catalogue()` is the only source of event names and fields for the UI.** A
page offering placeholders the server does not send is the overlay-preview
mistake in a new place. `test` is deliberately absent from it: `envelope()` sets
that key only when it is true, so a `{{ test }}` chip would look right in the test
send and render empty on every delivery that mattered.

**`cast.started` is emitted where the sender registers, not where the display is
pinned.** `start_pairing` pins the cast page to show the code, so an emit inside
`activate_display` would announce a cast when a *code* appeared — and then stay
silent for the real cast, because `activate_display` early-returns once it
already holds the override. `CastSession::cast_announced` keeps it to one per
cast across a sender reconnect, and gates `cast.ended` so an unused code emits no
end without a beginning. `override.set` stays in `activate_display`, because
pinning the display is exactly what happened there.

**`playback.playlist_empty` is edge-triggered** by `announced_empty`, because the
idle branch re-runs every five seconds and would otherwise be a notification
every five seconds for as long as the playlist stays empty. `display.connected`
fires once per successful CDP attach — after the runtimes are installed, so a
connection that still bails out to the top of the outer loop is not announced —
and `display.disconnected` is gated on `connected_before`, so a controller that
never got a browser does not report losing one.

**`display.*` reports the CDP connection, not that anything is being painted**,
and nothing here detects a frozen screen. Measured on a Pi 3 whose V3D GPU
wedged: thirteen hours of the same frame, the kernel resetting the GPU once a
second, the compositor blocked in `vc4_wait_for_seqno` — while CDP answered every
request and both page targets were present, so `is_connection_lost` never fired.
Do not let the events grow a name that implies otherwise.

**The routes are operator-only.** Not in `is_display_path` (which would open them
to the whole LAN), not in `cast::is_cast_public_path` (which would open them to
every guest): a target's headers are where an API token lives.

**No global switch and no CLI flag**, unlike `--managed-cert` and
`--guest-pages`. A fresh database has no rows, so nothing is delivered and
nothing needs switching off — which is also why the Python harness needed no new
default.

Targets are read fresh on every event and never cached: an operator who disables
one expects the *next* event to respect it, and it is a table with single-digit
rows.

## Settings (`src/settings.rs`)

**Read settings from `state.settings`, never from `state.args`** — the args are
the input to resolution, not the answer.

**A flag actually passed pins that setting** (`409` naming the flag, control
rendered locked). This is why the CLI-settable `Args` fields are `Option<T>` with
no clap default: `None` has to mean "not given", not "given the default". It is
also the recovery path for a forgotten operator password — see
[docs/deployment.md](docs/deployment.md#runtime-settings-versus-flags).

`--disable-cast` is the deployment-level kill switch and additionally decides
whether the HTTPS listener binds at all, so it cannot be undone without a restart.
`cast_enabled` is the operator-level one, and turning it off ends a running cast.

**`AppState::auth_cache` must be cleared whenever the credentials change**, or the
old password keeps working. It remembers the last `Authorization` header that
verified. It exists because PBKDF2 is deliberately slow and the
admin page polls every two seconds. A password passed on the CLI stays
`Secret::Plain` and is never written.

A code-auth misconfiguration (mode `code`, empty code) logs an error and refuses
every sender but does **not** stop the process. Casting must never keep the
signage from booting.

## The display browser (`src/chromium.rs`)

The controller starts Chromium itself unless something is already listening on the
CDP port. It **deliberately does not kill the browser when the controller stops** —
a deploy or a crash should not blank the screen, and the next start reattaches.

The "translate this page?" bubble is not suppressible by any command-line flag and
has no CDP command. The fix is `chromium::write_preferences`, which sets
`intl.accept_languages` from `--browser-language` and `translate.enabled` to false
before **every** launch — every launch, because the profile directory is wiped on
boot. **The keys are merged into any existing `Preferences` rather than replacing
the file**, so window bounds and zoom levels are not thrown away. Full account,
including the three flags that look like they should work and do not:
[docs/raspberry-pi.md](docs/raspberry-pi.md#the-translate-this-page-bubble).

**Downloads are refused browser-wide.** `Browser.setDownloadBehavior` with
`deny`, sent once per CDP connection in `browser_loop` beside the certificate
decision. A URL need not be a page: anything served as an attachment would
otherwise write a file, and enough of those fill an SD card and take the
database, the certificate and the uploads with it. **Unconditional on purpose** —
a guard against resource exhaustion must not depend on a setting, and a signage
display never wanted a download. `eventsEnabled` is on so
`Browser.downloadWillBegin` can tell a guest their link was a file rather than
leaving them in front of a screen that did not change. Covered by case [12] of
`tests/cast/test_browser.py`, which is the only Python test with a real browser.

### Invalid TLS certificates are accepted, on purpose

`browser_loop` connects with `ignore_https_errors: true`. Signage points at
internal dashboards on self-signed certificates, and whoever adds a playlist URL
is the one judging it trustworthy.

**Do not try to make this per playlist item.** It was attempted and measured:
`Security.setIgnoreCertificateErrors` sent to the adopted control page has no
effect at all — the page still ends on `chrome-error://chromewebdata/`. Only the
handler's setting takes, applied while its `NetworkManager` initialises each
target, i.e. once per connection. Two traps found on the way:

- **Do not send `Security.enable` first.** It switches Chromium into override
  mode, where a certificate error becomes a `Security.certificateError` event and
  the load blocks until someone answers `handleCertificateError`. Nobody does, so
  the interstitial stays up and the flag looks inert.
- Flipping the handler setting at runtime needs a reconnect *and* a page that is
  not already on the target URL, or `navigate_page` takes its same-document branch
  and reloads without re-running the certificate decision.

`examples/certtest.rs` reproduces it: a URL plus a mode (`none`, `explicit`,
`adopt`, `adopt-explicit`, `wait`).

## Database

**Schema lives only in `src/db.rs::run_migrations`**, which is idempotent:
`CREATE TABLE IF NOT EXISTS` plus a `pragma_table_info` probe before each
`ALTER TABLE ADD COLUMN`. Add new columns the same way. `main.rs` must not create
tables — a partial duplicate there caused schema drift.

`playlists`, `displays` and `playlist_items.playlist_id` arrived through exactly
that path, plus **one** backfill: only when there are no playlists at all and
items exist without one, a playlist named `Standard` takes them. Gated that way
because a database that already has playlists and a stray item without one is not
an upgrade — sweeping it into a new `Standard` would be the migration inventing a
decision. Filling the `displays` table is *not* schema and is not here: it depends
on what the command line declared, which is `display::register`, called from
`main.rs` after the migration and before any loop reads an assignment.

`PRAGMA foreign_keys` is enabled per connection via `SqliteConnectOptions`. It is
off by default in SQLite, which made `ON DELETE CASCADE` a no-op and left orphaned
playlist rows behind.

**`COALESCE` the JSON columns on every read path.** A real SQL `NULL` fails to
decode, which fails the whole query, and both call sites swallow the error into an
empty playlist — so one bad row blanks the screen. This is also why
`overlay_config` defaults to the JSON string `'null'` rather than SQL `NULL`.

## API

The endpoint list is in [README.md](README.md#api-overview). The traps behind it:

- **`PUT /api/playlist/{id}` changes a source only like for like** — a URL item
  takes a new `url`, an asset item a new `asset_id`. The opposite is a `400`: a
  kind change would need the other column cleared in the same write, and
  `playlist_target_url` picks one column over the other silently, so a
  half-changed row plays the wrong thing with no error anywhere. `asset_id` is
  checked against `assets` first; a dangling id yields a `NULL` `local_path` from
  the loop's `LEFT JOIN` and a blank screen.
- **A move (`playlist_id`) stands alone and appends.** It is a different axis
  from the source, so like-for-like has nothing to say about it — but the rest of
  the handler is field-by-field writes with swallowed errors while the move is a
  transaction over two playlists, so a combined request could answer one status
  for a half-applied write, and an explicit `play_order` beside it is two answers
  to one question. The item takes `MAX(play_order) + 1` in the target and **both**
  playlists are renumbered `1..n`, for the same reason `/move` renumbers. The
  target is checked against `playlists` in the statement that writes it —
  `playlist_items.playlist_id` was added by `ALTER TABLE` and has **no** foreign
  key, so nothing below would catch a dangling id. `null` is refused rather than
  ignored (hence `double_option` for a field that is not clearable): an item in no
  playlist is the broken state the field exists to repair.
- The item `overlay` object is stored as SQL `null` unless it would actually draw
  something, so read paths never have to tell "switched off" from "empty".
- **`POST /api/playlist` requires `playlist_id`.** An item in no playlist is one
  no screen would ever play, and nothing would say so. `GET /api/playlist` with no
  `?playlist_id=` still returns everything, which is the legacy shape.
- **`DELETE /api/playlists/{id}` counts and deletes in one statement**
  (`DELETE … AND NOT EXISTS (SELECT 1 FROM playlist_items …)`). Counting first and
  deleting second leaves a window for a `POST /api/playlist` to land in between,
  which is exactly the dangling item the guard exists to prevent. The second query
  runs only on the already-refused path, to tell the operator how many are in the
  way.
- **`POST /api/playlist/{id}/move` renumbers every row to `1..n`** within the
  item's *own* playlist — `ordered_ids_in_playlist` uses `IS`, not `=`, because
  `playlist_id` is nullable and SQL never matches two `NULL`s — instead of
  swapping two values. `play_order` is typed by hand in the UI, so duplicates and
  gaps accumulate, and a pairwise swap between two rows sharing an order does
  nothing.
- Nullable-clearable fields (`start_date`, `end_date`) need
  `Option<Option<String>>` with
  `#[serde(default, deserialize_with = "double_option")]`. Without it a JSON
  `null` collapses to the outer `None` and the field can never be cleared.
- Handlers that can reject answer with `{ "error": "..." }` and the UI shows that
  string inline. Everything else follows the swallow-and-log rule below.

## Conventions

- **Swallow handler DB errors** (`let _ = …` / `unwrap_or_default`) so the display
  never dies on a bad request — but `error!`-log first.
- The UI is dependency-free vanilla HTML/JS. Build rows with `textContent` /
  `createElement`, **never `innerHTML` interpolation**: filenames and URLs are
  attacker-influenced. `playlist.html` funnels this through a small `el()` helper.
- **A page that polls must not re-render what it polls into.** `playlist.html`
  polls every 2 s and updates badges and highlight classes only; the cards *are*
  the edit form, so a re-render wipes whatever the operator is typing. Cards with
  unsaved edits are tracked in a `dirty` set and carried across list reloads.
  `displays.html` polls the same way and keeps that **per card**, not per page:
  every card but the ones being edited is rebuilt from what the server just said.
  Skipping the whole reload while one card is dirty would hide a display losing
  its `declared` status — the screen-was-removed case that page exists to carry
  out — for as long as an edit sits open elsewhere.
- **Clamp `duration` before casting `i64` to `u64`.** A negative value became
  about 584 billion years of `Duration` and froze the playlist on one item.
- Sizes that reach the screen are in `vmin`/`vw`, not pixels.
