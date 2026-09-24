# A frozen screen is noticed, and the operator sees what is on screen

Status: design, approved in conversation on 2026-09-24.

## What and why

A Raspberry Pi 3 once showed the same frame for thirteen hours: its GPU wedged,
the compositor sat blocked in the kernel, and CDP answered every request
normally — so nothing in the controller noticed. And the operator pages say
*which item* is playing, never what the screen actually shows: an override, a
guest's page, the overlay on top.

Two things, one mechanism underneath: the controller watches that each screen is
still being painted, restarts its browser once when it is not, and can take a
picture of what is on screen for the operator pages.

## What was measured

On kiosk2 (Xorg, two screens, Chromium), with a `requestAnimationFrame` counter
injected into the page and Xorg stopped for fifteen seconds with `SIGSTOP`:

| | frames per ~2 s | `Runtime.evaluate` | `Page.captureScreenshot` |
|---|---|---|---|
| before | ~140 | answers | 0.3 s |
| Xorg stopped | **4 in 5 s** | **answers** | **times out** |
| after `SIGCONT` | ~140 | answers | 0.3 s |

The counter stopping while `evaluate` still answers is exactly the Pi's
signature — CDP healthy, picture frozen — and it is what `is_connection_lost`
can never see. Under Wayland the same should hold (Chromium paces
`requestAnimationFrame` on the compositor's frame callbacks), but it is **not
measured**; the docs say so.

A static page does not fool the counter: it counts paints the compositor lets
happen, not changes in the picture. A dashboard that shows the same numbers for
an hour still ticks sixty times a second.

## The heartbeat

- The overlay runtime (`web/overlay.js`), already injected into every page the
  display shows, keeps `globalThis.__ov.frames`, incremented in a
  `requestAnimationFrame` loop that starts when the runtime installs. Probed like
  every runtime: a page without it is not treated as frozen (a CSP can block the
  registered copy; the controller's re-evaluation still installs it).
- `browser.rs` reads it every **5 s** while an item or an override is on screen,
  in the loops that already park there (the per-item `select!`, the override
  loop, the idle screen). No progress for **`--freeze-timeout`** seconds
  (default 60) → frozen. The `keep_loaded` tab brought to front is the page
  read; a navigation resets the baseline.
- Pure decision in `src/freeze.rs`: given the samples (time, counter), the
  timeout and the brake state, say *fine*, *frozen*, or *frozen again*. Tested
  without a browser.

## The response, with a brake

- **First freeze:** restart that screen's Chromium and fire `display.frozen`
  (`{ "seconds": <since the last frame>, "restarted": true }`). The outer loop
  then reconnects as after any lost connection, and `display.connected` follows.
- **Recovered:** when the counter moves again, `display.recovered`
  (`{ "seconds": <frozen for> }`).
- **Frozen again within 30 minutes of a restart:** no second restart —
  `display.frozen` with `restarted: false`, and the screen shows as frozen on the
  operator pages until it recovers. A panel switched off by DPMS, or a GPU that
  is simply gone, must not become a restart loop.
- **Only a browser the controller started is restarted** (`chromium.rs` knows
  which it launched). One found already running on the CDP port is left alone and
  only reported, as `restarted: false`.
- The `display.*` rule in `CLAUDE.md` changes from "nothing here detects a frozen
  screen" to describing this, including what it cannot see (a panel that is off,
  a cable that is out: the compositor still paints).

## What is on screen

- `GET /api/displays/{name}/screenshot` — operator-only (in neither exemption
  list). `Page.captureScreenshot` of the page on screen as **JPEG** (quality 60),
  scaled to at most 640 px wide via `clip.scale`.
- **On request only, cached 10 s per screen**: nothing is captured while nobody
  looks, which matters on a Pi. A capture has a **5 s timeout** — the measurement
  shows it hangs on a frozen screen — and a timed-out capture answers with the
  last picture and its age (`X-Screenshot-Age: <seconds>` header, plus
  `X-Screen-Frozen: 1` when the heartbeat says so), or `503` when there is none.
- Shown where the operator already looks, fetched only while the page is visible
  (`document.visibilityState`) and refreshed every 10 s:
  - the admin page's status line, one thumbnail per screen beside "Jetzt läuft …";
  - each card on the displays page;
  - the playlist page, on the card of the item currently playing.
- A frozen screen's thumbnail carries a visible "eingefroren seit …" mark.

## Tests

- Rust: `freeze.rs` — steady counter fine; stalled beyond the timeout frozen;
  frozen again within the brake window gives no restart; a reset baseline after
  navigation; a missing runtime is not frozen.
- Python with a real Chrome (the display harness): stopping the page's counter
  (the test replaces the rAF loop) makes the controller fire `display.frozen`
  with `restarted: false` for a browser it did not start, then
  `display.recovered` when the counter resumes; with `--freeze-timeout` short.
  Restarting a controller-launched browser is covered by a harness that lets the
  controller launch its own headless Chrome.
- The screenshot endpoint: a JPEG, cached (two requests within 10 s return the
  same bytes), operator-only (`401` without credentials, not on the display or
  cast exemptions), `503` for a screen with no page yet.

## Not in this

Detecting a panel that is off or a cable that is out (the compositor still
paints), a history of screenshots, comparing screenshots to decide a freeze, and
anything compositor-specific.
