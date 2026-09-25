# Built-in widgets — design

**Status:** implemented

## Goal

Ship content the controller renders itself — a clock, a banner/message, a QR
code, a countdown — usable both as a **layout widget** (a third source type
beside URL and asset) and as a **standalone playlist item** (a fourth item type
beside url, asset and layout). No external page, no network.

## Motivation

Today a widget or item shows a URL or an asset. Common signage content — the
time, a "closed today" banner, a share-QR, a countdown to an event — needs
neither: it is a handful of parameters the controller can render. Built-ins make
that a first-class choice instead of forcing an operator to build and host a page.

## Architecture

**The server resolves a built-in to a self-contained payload and encodes it into
one controller-served page.** A built-in config resolves — in one place,
`src/builtin.rs` — to a URL `http://127.0.0.1:<port>/widget.html?c=<base64url
json>`, where the JSON carries the kind, its options, and anything only the
server knows: the QR module matrix (`cast::qr_matrix`), the resolved guest URL
(`cast::sender_url`), and the locale (from settings). `web/widget.html` decodes
`c`, renders the kind, fills the cell, and — for the clock and countdown — ticks
client-side. It fetches nothing.

Why URL-encoded and stateless rather than a fetch endpoint: the same page then
works identically in a layout iframe and full-screen, an edit changes the URL so
`browser.rs` re-navigates (and `widgets_for_display` rebuilds) with no cache to
bust, and the server stays the single resolver — the same rule the overlay
payload follows, so the QR/cast/locale resolution cannot drift between overlay
and widget.

Why same-origin and no defences: `widget.html` is our own page on the loopback
origin. It needs none of the overlay's five defences (shadow DOM, popover,
CSSOM, MutationObserver, no-fetch) — those exist because the overlay is injected
into pages we do not own. It also needs no `frames.rs` unlock (that is only for
cross-site dashboards) and no scroll runtime (a built-in fits its cell).

## A — Data model

- New `builtin::Builtin`, a tagged enum (`#[serde(tag = "kind")]`) with variants
  `Clock`, `Banner`, `Qr`, `Countdown`, each carrying its own options (below). It
  derives `Serialize, Deserialize, Clone, PartialEq`.
- `layout::Source` gains `Builtin(Builtin)` beside `Url` and `Asset`. (The enum
  is `#[serde(untagged)]` today; adding a variant that is itself internally
  tagged keeps untagged resolution unambiguous — a `{kind, …}` object matches
  only `Builtin`. If untagged proves ambiguous in practice, switch `Source` to
  internally tagged in the same change; the plan verifies round-trip either way.)
- `playlist_items` gains a `builtin TEXT DEFAULT 'null'` column (migration in
  `db::run_migrations`, `pragma_table_info` probe, `COALESCE`d on read like
  `layout`).
- An item is **exactly one of** url / asset / layout / builtin. The add/update
  handlers count sources and refuse anything but one; a builtin item replaces a
  builtin only, never a url/asset/layout and back (like the layout rule).
- `advance` for a builtin item is **time-only** (`Passes` has no meaning);
  `advance::check` refuses `Passes` for a builtin, as it does for a layout.

## B — Resolution and rendering

- `src/builtin.rs::resolve(state, &Builtin) -> String` returns the
  `/widget.html?c=…` URL. It builds the payload JSON, adding the server-only
  fields per kind (QR matrix + cast URL for `Qr`, locale for `Clock`/`Countdown`),
  base64url-encodes it, and caps the encoded length (a `Banner` text is bounded
  at the API; see limits).
- `browser::playlist_target_url` returns `resolve(...)` for a builtin item.
- `layout::widgets_for_display` returns `resolve(...)` for a `Source::Builtin`
  widget (no scroll payload — built-ins do not scroll).
- `web/widget.html`: decodes `c`, switches on `kind`, renders into a cell-filling
  box, auto-fits where the kind asks for it, ticks a `setInterval` for
  `Clock`/`Countdown`. Registered in `is_display_path` and `include_dir`; a new
  file needs `touch src/web.rs`.
- QR is drawn as inline SVG from the matrix in the payload (the same shape the
  overlay uses), so nothing is fetched and no `img-src` has to allow it.

## C — The four kinds

Shared by all: `background_color` (+ alpha), `text_color`. Content is centred and
scaled to the cell unless a kind says otherwise.

- **Clock** — `format_24h: bool`, `show_seconds: bool`, `show_date: bool`,
  `timezone: Option<String>` (IANA name; empty = the device's zone). Time and
  date are formatted client-side with `Intl` using the payload's locale. The time
  auto-fits the cell; the date is a smaller line under it.
- **Banner** — `text: String`, `mode: fill | normal`.
  - `fill`: text auto-scales to fill the cell (headline).
  - `normal`: a readable fixed size (`small | medium | large`), text **wraps**
    across lines, optionally left-aligned (`align: center | left`); overflow past
    the cell is clipped (`overflow: hidden`).
- **Qr** — `source: text | cast`. `text` uses `qr_text`; `cast` resolves the
  guest URL at resolve time (drawing nothing when casting is off, like the
  overlay's cast QR). Optional `label` under the code. The code fills the cell
  (minus the label).
- **Countdown** — `target: String` (ISO datetime), `label: String`,
  `done_text: String`. Shows the largest sensible units ("noch 3 T 4 Std 12 Min")
  and switches to `done_text` once the target passes. Ticks once a minute (or a
  second when under a minute). Auto-fits.

Out-of-range / unparseable values fall back rather than error (an unknown
timezone → device zone, a bad colour → the default, a bad `target` → show
`done_text`), the same forgiving rule the overlay uses — a screen has nobody in
front of it.

## D — Editors

- **Shared editor** `web/builtin-editor.js`: `BuiltinEditor.create({ builtin, el
  })` → `{ root, read() }`. A kind dropdown (Uhr / Banner / QR / Countdown) and
  the fields for the chosen kind; `read()` returns the `Builtin` JSON (or the
  current kind's defaults). Dependency-free, built with `createElement` /
  `textContent`.
- **Layout widget** (`web/layout-editor.js`): the per-widget source picker gains
  a third choice "Built-in"; when chosen it mounts a `BuiltinEditor` in the
  fields panel and `read()` emits `source: { kind, … }`.
- **Playlist item** (`web/playlist.html`): the add-form and card kind selector
  (today Seite/Asset vs Layout) gain "Built-in", mounting the same
  `BuiltinEditor`; the add/save paths send `builtin` and the source-count rule
  accepts it.

## E — Limits, testing, docs

**Limits.** `Banner.text` and `Qr.qr_text` are capped at the API (e.g. 500 chars,
matching the overlay's text caps); `Countdown.label`/`done_text` at 100. The
resolver caps the encoded `c` length as a backstop so a widget URL cannot grow
without bound.

**Testing.**
- Rust unit tests (`src/builtin.rs`): each variant serde round-trips;
  `resolve` produces a `/widget.html?c=…` whose decoded payload carries the kind,
  options, and the server-only fields (QR matrix for `Qr`, locale for `Clock`);
  the item source-count rule accepts exactly one of the four.
- `tests/cast/test_builtin.py` (stdlib, real Chrome like `test_layouts`): a
  built-in **widget** in a layout is shown as a `/widget.html` frame at its grid
  place; a **standalone** built-in item is shown full-screen; the clock renders a
  time and ticks; the banner text is on the page; the QR draws real SVG modules.
  A `normal`-mode banner wraps and clips.
- Run by hand after touching `layout.rs`, `builtin.rs`, `browser.rs`, the route
  table or the widget page; stop any local instance first (port/Chrome
  collisions read as regressions).

**Docs.** CLAUDE.md gets a Built-ins section (the resolver-is-the-only-place rule,
the four kinds, the no-defences/no-unlock rationale); `docs/features.md` describes
them for operators; README's API overview notes the new item type and the
`/widget.html` display path.

## Out of scope (YAGNI)

- Weather or anything needing a network API or a key — the device is often
  offline, and a broken widget on a screen nobody watches is worse than not
  offering it.
- An image slideshow built-in — that is what a playlist already is.
- Per-kind fonts or full CSS — background/text colour and the per-kind options
  above are the whole surface, to keep the look consistent and the editor small.
- A general plugin/registration mechanism — four kinds are a `match`, not a
  framework.
