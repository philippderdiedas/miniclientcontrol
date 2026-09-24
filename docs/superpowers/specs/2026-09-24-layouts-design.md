# Layouts: a screen split into widgets

Status: design, approved in conversation on 2026-09-24. Part 1 of three; see
*The parts* below.

## What and why

A screen shows one thing at a time today. Signage is often several at once: two
dashboards side by side, a main page with a ticker below, a main page with a
column beside it, a grid of pictures. A playlist item can now be a **layout**: a
fixed 24×24 grid on which **widgets** sit as rectangles, each showing a URL or an
asset.

### The parts

1. **Layouts** (this spec): the grid, widgets showing a URL or an asset, the
   editor, and what makes framed dashboards work.
2. **Widgets of our own**, one at a time: ticker, clock and date, countdown,
   later RSS, weather, room booking — content that is not somebody else's page.
3. A free-form editor beyond the templates, if it is still wanted — the data
   model below already describes any grid, so it would add UI only.

## How people lay out screens

Most signage products offer templates first; a free designer (Xibo's regions) is
what beginners struggle with and mostly use to rebuild the same few layouts:
fullscreen, main + bottom bar, main + side column, L-shape (main + column +
bar), 50/50, 2×2. The design follows that: **a grid like Grafana's**, with
templates as starting points, and widgets that can be dragged and resized on it.

## The grid

- **24 columns × 24 rows**, each cell a fixed share of the screen's width and
  height. Not 16×9: that is square only on 16:9 and ties a layout to one kind of
  screen. 24 rows make a bar of one row about 45 px on 1080p.
- A widget is `{ x, y, w, h }` in grid units — Grafana's `gridPos` without the
  scrolling, since a screen does not scroll. It maps 1:1 onto CSS Grid.
- Valid: every widget inside the grid, at least 1×1, **no overlaps**, at least
  one widget, at most 12. Refused with a `400` naming the problem otherwise.

## The screen's aspect ratio

The controller drives the display browser, so it knows the window: on every CDP
connection it reads `innerWidth`/`innerHeight` and stores them on the display
(`displays.viewport_width`, `viewport_height`). `GET /api/displays` returns them.
The editor draws its canvas in that ratio, so a widget that is square on the grid
shows as wide as it will be on a 16:9 screen, and portrait screens work without
anything special. A display never connected yet: 16:9, and the editor says so. A
playlist on screens of different ratios stretches on each; the editor lets the
operator pick which screen to preview and says the layout stretches.

## The data

- `playlist_items.layout` — JSON, `COALESCE`d to `'null'` like the other JSON
  columns: `{ "widgets": [ { "x", "y", "w", "h", "source": { "url" } | { "asset_id" },
  "scroll_config", "fit_mode", "fit_background" } ] }`.
- An item is exactly one of: a URL, an asset, a layout. `POST`/`PUT
  /api/playlist…` take `layout`; a kind change is refused as today (like for
  like). Asset ids are checked against `assets`.
- `advance` for a layout: **time only**. *Passes* has no single meaning across
  several widgets; refused with a `400`.
- `POST /api/playlist/{id}/duplicate` copies an item (any kind) to the end of its
  playlist — the answer to "the same L-shape for ten items" without introducing
  named layout templates.

## Showing a layout

- `GET /layout.html?item=<id>` (display path: loopback only) builds a CSS grid
  with one `<iframe>` per widget, from `GET /api/layout/<id>` (also a display
  path). URL widgets frame the URL; asset widgets frame the existing
  `media_viewer.html` / `pdf_viewer.html` with the widget's fit.
- The overlay (global and the item's own), the freeze heartbeat and the
  screenshot all belong to the layout page itself — the top frame — and work
  unchanged.
- **`keep_loaded` stays an item setting.** A layout item kept loaded keeps the
  whole layout page in its tab, every widget frame with it: all its dashboards
  stay signed in and live, and the next showing is instant. There is no per-widget
  `keep_loaded`: a frame belongs to one page and cannot move between layouts, so
  keeping a widget loaded could only mean keeping its layout loaded.
- **Scroll per widget:** a cross-site frame is its own renderer and its own CDP
  target, and the registered runtimes do not reach it. The controller
  auto-attaches to the layout page's frame targets, installs the scroll runtime
  in each, and applies that widget's `scroll_config` (a frame knows its widget by
  its URL; the layout page gives each iframe a `name` of its widget index, which
  the frame target reports). In a kept layout the runtime is installed once, when
  it loads, and stays — as in a kept page today.

## Making framed pages work

Measured in a local lab (a page that forbids framing and logs in with a
`SameSite=Lax` cookie, like checkmk):

| step | result |
|---|---|
| nothing | blocked |
| response replaced (`Fetch.getResponseBody` + `fulfillRequest`), framing headers dropped | shown, not logged in |
| rewriting `Set-Cookie` | no effect: cookies are processed before interception, and `Fetch` does not show `Set-Cookie` |
| holding the response until `Network.responseReceivedExtraInfo` reports the cookie blocked for SameSite, setting it with `Storage.setCookies`, then releasing | **logged in, script-loaded data arrives, on the first load** |
| the same cookie set without a partition | **another site in the same browser got the session** |
| set with `partitionKey` = the layout page's site | logged in; **the other site did not** |

So, while a layout is on screen:

- **Interception is scoped to frames of our layout pages, not to a host list.**
  `Fetch.enable` on the browser session for `Document` responses; a response is
  touched only when its frame is a descendant of a layout page's main frame —
  the one on screen *and* every kept-loaded one, so a dashboard renewing its
  session in a background tab gets its cookie too.
  A short link, an SSO redirect to an identity provider, a dashboard's own
  redirect all land in that frame and work. The layout page itself and every
  top-level page — a playlist URL, a guest's page — are never touched.
- **Framing headers are removed** — `X-Frame-Options`, and the
  `frame-ancestors` directive of `Content-Security-Policy` (the rest of the CSP
  is kept) — by replacing the response. A redirect has no body and is fulfilled
  with an empty one.
- **Cookies blocked for SameSite are set again as `SameSite=None; Secure`,
  partitioned to the layout page's site**, before the response is released: held
  until its `responseReceivedExtraInfo` has arrived, at most one second. The
  partition is what keeps a guest's page from riding the kiosk's dashboard
  sessions; an unpartitioned cookie would be sent to the dashboard from any page
  in that browser.
- **Third-party cookies are allowed in the kiosk profile**
  (`chromium::write_preferences`, beside the translate keys), which a framed
  page needs to keep its session at all.
- Script frame-busting that *redirects* the top page is already blocked by
  Chromium (measured: a cross-origin frame may not navigate the top without a
  user gesture). A page that *hides itself* when framed is not handled; rare,
  and left until a real page does it.

This is a deliberate weakening of clickjacking protection, confined to frames
the operator put into a layout on a screen nobody clicks.

## The editor

On the playlist page, "Neues Layout", and on a layout item's card:

- a canvas in the screen's aspect ratio with the 24×24 grid;
- templates as starting points: fullscreen, L-shape, main + bar, main + column,
  50/50, 2×2 — they fill in widgets, which can then be moved;
- widgets dragged and resized with the pointer, snapping to the grid;
- per widget: source (URL, or asset with the existing preview), scroll mode,
  fit;
- "Duplizieren" on every item card.

Built with `createElement`, like every operator page.

## Cost

Every cross-site widget is its own Chromium renderer process. Four dashboards
side by side are nothing on kiosk2 and a lot for a Raspberry Pi 3, and a kept
layout holds its renderers permanently; there is no artificial limit — the docs
say what a layout costs.

## Tests

- Rust: grid validation (inside, at least 1×1, no overlap, count), a layout's
  `advance` refused as passes, kind changes refused, `layout` round trip.
- Python with a real Chrome: a layout with two widgets on a harness page that
  forbids framing and logs in with a Lax cookie shows logged in; a page of
  another site loaded afterwards does not get that session; a short-link redirect
  in a widget is followed; a widget scrolls with its own mode; the layout on a
  portrait window; the viewport is stored on the display; duplicating an item.
- The lab script this design came from is kept as a test, so a Chromium update
  that changes any of the measured behaviour fails loudly.

## Not in this

Widgets of our own (part 2), named layout templates, a free-form editor (part 3),
per-widget playlists, pages that hide themselves when framed.
