# Media fit, silent video, and an overlay that does not blink

**Status:** designed
**Date:** 2026-09-22

## What and why

Three complaints from the same afternoon in front of a real screen, and they all
come back to the same two places: how an asset reaches the display, and when the
overlay reaches the page.

**An image cannot be told how to sit on the screen.** A playlist item that is an
image or a video is navigated to directly — `http://127.0.0.1:<port>/uploads/…` —
and what happens next belongs to Chromium, not to us. Chromium builds an image
document or a media document with its own layout, so an operator whose poster is
the wrong aspect ratio has exactly one option, which is to re-export the poster.
There is no setting because there is no page of ours to put one on.

**A video shows its controls.** The same media document comes with a control bar
that fades in on load and again on any pointer movement. On a desk that is
correct behaviour. On signage it is a black strip across the bottom of the
picture for the first seconds of every play, and nothing on the device will turn
it off: `controls` is the media document's, not ours.

**The overlay blinks on every item change.** `apply_overlay` runs *after*
`navigate_page`, after the attached-target drain (700 ms) and after
`wait_for_scroll_readiness`, which is allowed twelve seconds. The badge is a DOM
node in the target document, so the navigation destroys it and the new document
spends that whole window without one. What an operator sees is the clock
disappearing and coming back at every transition, which reads as a fault.

The first two are one fix — our own page instead of Chromium's — and the third is
a separate one.

## Decisions taken

1. **Fit is per playlist item, not per asset.** It joins `duration`,
   `scroll_config` and `overlay_config` on the axis that already exists. The
   same poster can be `cover` on the foyer screen's playlist and `contain` on the
   workshop's, and nothing has to explain an inheritance rule.

2. **`scroll` is a fit value, not an implicit rule.** The obvious alternative —
   "if `scroll_config` is set, ignore the fit" — hides a layout decision inside
   an unrelated field. A fifth value in the dropdown is a thing the operator can
   see and choose, at the cost of two fields that must agree. The disagreement is
   one-directional and harmless, and the UI says so (see *UI*).

3. **The background colour is per item.** A contained image leaves bars, and what
   belongs in them depends on the image. One global colour would be wrong for the
   item that prompted the question, which is the same reasoning that gave
   `ItemOverlay::color` its exception.

4. **A video loops.** The item's `duration` ends it either way; without `loop` a
   short video parks on its last frame for the remainder, which looks like a
   stall. `loop` is unconditional rather than a comparison against the item
   duration — the comparison needs the asset's duration to be known and correct,
   and the attribute achieves the same thing without knowing anything.

5. **The overlay is seeded into the next document rather than applied faster.**
   Moving `apply_overlay` up to just after `navigate_page` would shrink the gap
   but not close it, and would put an evaluate against a document that may not
   have one yet. Seeding closes it: the badge exists before the new document's
   first paint.

## The media viewer

A new page, `web/media_viewer.html`, served like every other page under `web/`
and therefore compiled into the binary by `include_dir!`. It is the same shape
`pdf_viewer.html` already has: our page, our origin, its instructions in the
query string.

```
http://127.0.0.1:<port>/media_viewer.html?asset=<local_path>&kind=image|video&fit=contain&bg=%23000000
```

`playlist_target_url` and `override_target_url` route an asset to it when the
mimetype starts with `image/` or `video/`. PDFs keep `pdf_viewer.html` — they
have their own layout and their own scrolling — and anything else keeps the
direct `/uploads/` navigation it has today. A URL item is untouched: there is no
asset and no page of ours in the path.

The page is a black-by-default body with one element in it:

- `kind=image` → `<img>`
- `kind=video` → `<video autoplay loop playsinline>`, **without `controls`**

No control bar is the whole point, and `--autoplay-policy=no-user-gesture-required`
is already in `chromium::BASE_FLAGS`, so the video starts on its own and keeps
its sound. A browser started outside the controller may lack the flag, so the
page tries unmuted first and falls back to muted only on `NotAllowedError` — a
silent video beats a black screen. A video that is on a `keep_loaded` tab plays in the background exactly
as the media document does today.

### The five fit values

Four are `object-fit` and one is not:

| Value | What it does |
|---|---|
| `contain` | The whole asset is visible, bars where the aspect ratio does not match. **Default.** |
| `cover` | Fills the screen, crops what does not fit. |
| `fill` | Fills the screen, distorts. |
| `none` | Natural size, centred, clipped if larger than the screen. |
| `scroll` | Not `object-fit`: `width: 100%; height: auto`, the document scrolls. |

`contain` is the default because it is the closest thing to what Chromium's image
document does today, and an upgrade must not silently recrop a venue's playlist.

**`scroll` is what makes a tall image scrollable**, and it is the reason the media
viewer must *not* be added to `is_internal_pdf_viewer_url`'s exemption from
`start_scrolling`. `web/autoscroll.js` is injected into it like any other page and
scrolls the document; the PDF viewer is the exception because it drives its own
scrolling from query parameters, and this page does not.

`scroll` on a video is meaningless — a video has no overflow worth scrolling — and
renders as `contain`.

**An unknown value falls back to `contain`**, and an unparseable `bg` falls back
to black. Same rule as the overlay's out-of-range numbers and unknown corners:
what reaches a screen nobody is standing in front of gets clamped, not refused.

## Storage and API

Two columns on `playlist_items`, added the only way this project adds columns —
in `db::run_migrations`, behind a `pragma_table_info` probe, with a default:

```sql
ALTER TABLE playlist_items ADD COLUMN fit_mode       TEXT DEFAULT 'contain';
ALTER TABLE playlist_items ADD COLUMN fit_background TEXT DEFAULT '#000000';
```

Both are read as `COALESCE(p.fit_mode, 'contain')` and
`COALESCE(p.fit_background, '#000000')` in the loop's playlist query in
`browser.rs` and in `get_playlist` in `handlers.rs` — the only two queries that
decode a `PlaylistItemWithAsset`. These are `TEXT`, not JSON, so a `NULL` decodes into a
`String` field as a failure of the whole query exactly as the JSON columns do —
and both call sites swallow that error into an empty playlist, which blanks the
screen. The `COALESCE` is not defensive decoration; it is the rule that column
already broke once.

`PlaylistItemWithAsset` gains `fit_mode: String` and `fit_background: String`,
both `#[sqlx(default)]`. The row field is a `String` rather than the enum: the
enum would need a `sqlx::Type` impl to decode, and the parse has to be total
anyway, so it is done where the value is used.

`FitMode` is a new enum in `models.rs`, serialised as its lowercase name, with a
`Default` of `Contain` and a total parse — `FitMode::from_value(&str)` — that
yields `Contain` for anything it does not recognise.

**Validation lives in the handlers**, not in the viewer: `fit_background` must
pass `settings::is_hex_colour` — `#rgb`, `#rrggbb` or `#rrggbbaa`, the same check
the overlay's colours use, so the two colour fields on one card accept the same
thing — or the write is a `400` with `{ "error": … }`, checked before anything
else in the request is written. Unlike the values
that reach the browser, this one is typed by an operator who is looking at the
page and can be told. (The viewer *also* falls back to black, because a row
written by an older binary or edited by hand must not paint an undefined colour.)

`POST /api/playlist` and `PUT /api/playlist/{id}` accept both fields as optional.
`fit_mode` arrives as a string and goes through `FitMode::from_value`, not
through serde's enum decoding: an unknown name must fall back, and a serde
failure would refuse the whole request instead.
Neither is clearable — there is always a fit and always a background — so plain
`Option<T>` is right and `double_option` is not.

`OverrideItem` gains the same two fields — not as `Option`, but defaulted
(`#[serde(default)]`) — filled in `set_override_of` from the request the way
`scroll_config` already is. Without them an asset override would reach the viewer
with no fit at all; with them the JSON stays backward compatible for every caller
that does not send them. Defaulted rather than optional because
`run_override_loop` compares two overrides to decide whether to re-navigate, and
`None` and `Some(Contain)` would draw the same thing while comparing unequal —
the cast override, which never has an asset, gets the defaults.

**`/media_viewer.html` joins `is_display_path`.** The display browser presents no
credentials, so a page it loads that is missing from that list works on a device
without basic auth and is a `401` on every screen the moment it is switched on.

## UI

`web/playlist.html`, on the card, next to the scroll controls:

- a `<select>` — *Einpassen* / *Füllen* / *Strecken* / *Original* / *Scrollen*
- an `<input type="color">` for the background

Both are built with the page's `el()` helper — never `innerHTML` interpolation —
and both are **shown only for an image or video asset item**. A URL item, a PDF
and an item with no asset do not get them, because the value would not reach
anything.

Both fields join the card's `dirty` set. The page polls every two seconds and the
cards *are* the edit form, so a field outside `dirty` is wiped under the
operator's hands mid-edit.

**One hint line**, shown when `scroll_config` is not `None` and `fit_mode` is not
`scroll`: the scroll setting has nothing to scroll. This is the cost of decision
2, paid where it is visible. The reverse — `fit_mode = scroll` with no scroll
configured — is a legitimate "fit to width" and says nothing.

## Overlay without a gap

`web/overlay.js` gains a seed step at the end of the IIFE, right after
`globalThis.__ov` is assigned: if `globalThis.__ovSeed` is set, apply it —
unless the controller has already applied a payload of its own in this document.

This is the same mechanism as `__ovSuspend`, for the same reason: a value the
page can carry *before* the runtime runs, because the runtime's own injection is
too late. `ensureHost` already appends to `document.body || document.documentElement`
and `watch()` already observes `document.documentElement`, so once there is a
root element the badge attaches to the still-empty document and the observer
keeps it attached as the parser builds the page underneath it.

**The one thing the seed path must not assume is that the root element exists.**
`Page.addScriptToEvaluateOnNewDocument` runs the script on document creation,
which can be before the parser has produced `<html>` — and `appendChild` on a
null root throws, while `MutationObserver.observe(null)` throws too, which would
take the whole runtime down before `__ov` is usable. So the seed is applied
immediately when `document.documentElement` is there and otherwise the moment it
appears, via a `MutationObserver` on the `document` node itself (which always
exists) — not on `readystatechange`, which fires only once parsing is done and
can come after the first paint. `__ov.apply` called by the controller later is
unaffected; by then there is always a document.

**The seed applies in the top frame only.** The registered script runs in every
frame, iframes included, and a seed honoured there would draw a second badge in
the middle of every dashboard that embeds something. `window.top === window` is
the guard; it does not throw across origins.

`__ov.state()` gains two read-outs, both for the tests: `seeds`, how many seeded
registrations ran in this document (a leaked registration shows up as more than
one), and `seededAt`, when the seed was applied in milliseconds since the
document started, or `null`.

`browser.rs` changes in three ways.

**The registered script carries the payload.** `seed_overlay_runtime(page, seed,
payload)` registers `globalThis.__ovSeeds++; globalThis.__ovSeed = <json>;`
followed by `overlay_runtime_script()` via `Page.addScriptToEvaluateOnNewDocument`.

**The previous registration is removed first.** The add returns a
`ScriptIdentifier`; it is kept per page and passed to
`Page.removeScriptToEvaluateOnNewDocument` before the next add. Skipping this is
not merely untidy: a registration per item change accumulates in the target for
as long as the controller runs, and every one of them executes on every
navigation. The identifier lives in an `OverlaySeed` held by the loop per CDP
connection — a registration belongs to the session that made it, so a reconnect
starts from nothing — together with the payload it carries, so re-seeding an
unchanged payload is skipped and calling it on every pass costs nothing.

**Only the control page is seeded.** The playlist, the idle screen and every
override all run on that one page. `keep_loaded` tabs keep the bare runtime
registration: they are brought to front rather than navigated, so their document
and the badge in it persist between showings and there is no gap to close.

**`apply_overlay` splits into three.** Building the payload
(`settings::overlay_payload`, unchanged and still the only place that builds one),
seeding it, and applying it to the live page. The item loop reorders around this:

```
load_item_overlay(item)          ← fresh, as today, but moved up
payload = overlay_payload(…)
seed_overlay_runtime(page, …)    ← before the navigation
navigate_page(…)
…drain, runtime install, scroll readiness…
apply_overlay_payload(page, …)   ← same payload, unchanged position
```

The second application stays. With an identical payload it is invisible, and it
is the CSP fallback that already justifies re-evaluating the runtime after
navigation: **a strict `script-src` can block the registered copy**, and on those
pages the behaviour is exactly what it is today, blink included. That is the
honest limit of this fix and it is not worth hiding.

`overlay_signal` handlers — the per-item `select!`, the idle-screen wait and
`run_override_loop` — rebuild the payload, re-seed and re-apply, so the seed is
never a stale copy of an edited overlay. That matters beyond the loop's own next
navigation: a page that navigates itself (a dashboard on a meta refresh) comes
back with whatever is registered, and the controller never re-applies to it. All
three must do it, for the reason they all already handle the signal.

The idle page and the override page are seeded right before their navigation,
exactly like a playlist item.

## Testing

**Rust, `#[cfg(test)]`:**

- `FitMode` parses its five names, and anything else yields `Contain`.
- `asset_target_url` — the routing half of `playlist_target_url`, split out so
  it needs no `AppState` — sends `image/png` and `video/mp4` to
  `media_viewer.html` with the fit and background in the query, `application/pdf`
  to `pdf_viewer.html`, and anything else to `/uploads/`; and the media viewer is
  not mistaken for the PDF viewer by `is_internal_pdf_viewer_url`.
- `is_display_path("/media_viewer.html")`.
- A pre-fit database gets `contain` and `#000000` on its existing rows.

**Python, `tests/cast/`:**

- `test_overlay.py` `[48]`: two items taking turns every two seconds; every
  document observed must report a `seededAt` under 500 ms — the controller's own
  apply cannot come before the 700 ms drain plus the readiness wait, so only the
  seed can pass this. Every registration runs at document creation, before
  `<html>`, so the same case covers the missing-root path, and it asserts
  `installed` to prove nothing threw. It also asserts `seeds == 1`: several item
  changes must not leave several registrations. This is the regression; against
  today's code it fails.
- `test_overlay.py` `[48b]`: a page with a `srcdoc` iframe has one badge on top
  and none inside the iframe.
- `test_media.py` (new — own Chrome on CDP 9252, controller on 3051): the API
  round trip, fallback and refusal (`[100]`–`[102]`); then in a real browser an
  image item with `cover` on green is drawn by the media viewer with
  `objectFit: cover` and a green body and no overflow, the same item switched to
  `scroll` makes the document taller than the screen, and a video override is a
  `<video>` without controls, looping and autoplaying, with the override's own
  fit (`[103]`–`[105]`).

These run against the single implicit display. The standing rule that a test
about a specific screen names a non-primary one is about *resolving* a screen,
and nothing here resolves one differently than before: the payload still comes
from `overlay_payload` with the display the loop was given.

## Not in scope

- **Per-asset fit defaults.** Decision 1; adding them later is additive.
- **Fit for URL items and PDFs.** Neither has a page of ours whose layout we own.
- **Ending an item when its video ends.** The item's `duration` stays the only
  clock. Making the video's length end the item means the loop waiting on a page
  event instead of a timer, which is a different change to `browser.rs` and a
  different risk.
- **Muting.** Video audio keeps working exactly as it does today; room audio is
  `audio.rs`'s business and is not touched.
- **Closing the CSP blink.** There is no CDP command that draws over a page, so
  a page that blocks our script blocks the badge. Unchanged from today.
- **Free CSS for the background.** A hex colour, validated. `background_css`
  exists in the overlay as an escape hatch and is not worth a second copy here.
