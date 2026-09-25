# Overlay grid positioning — design

**Status:** implemented

## Goal

Position the overlay by a place on a 24×24 grid (the same grid the layout
widgets use) instead of one of a handful of fixed corners. The corners are
replaced, not augmented.

## Motivation

The overlay is pinned to one of a few named corners
(`top-left`, `bottom-right`, `top-center`, …). Layouts already gave the project
a 24×24 grid and a grid editor; reusing them lets an operator drop the house
clock / QR / notice anywhere on the screen, with the same tool they place
widgets with.

## The model

**Region, not corner.** An overlay box's position becomes a grid region
`{ x, y, w, h }` on a 24×24 grid — identical units to `layout::Widget`. `x + w`
and `y + h` must stay inside the grid, `w` and `h` are at least 1.

**One box, freely placed.** The global overlay stays a single box (clock, QR,
text, image together), now sitting in a chosen region rather than a corner. An
item overlay can still contribute a second box. This is deliberately *not* a
multi-box overlay layout — that stays a possible future feature.

**Auto-size, centred in the region.** The region gives the box its place and
its bounds; the box itself keeps its natural width and height and centres inside
the region. Consequences:

- The region's **width caps the content**: text wraps at it, a QR or image
  scales down to fit it. This is what the old `max_width` setting did, so
  `max_width` is removed and the region subsumes it.
- The region's **height positions the vertical centre**. Content larger than the
  region overflows it (centred) rather than being clipped — the content is
  small, signage has nobody standing in front of it, and this matches the
  existing "clamp, don't reject" rule.
- There is **no alignment control** — content is always centred. The old
  corner-derived alignment (`alignmentFor`, `-center`/`-right`) is dropped. This
  keeps the discipline the corner model had ("a box pinned centre-bottom with
  left-aligned text is a mistake").

**The region subsumes `position`, `margin` and `max_width`.** A region names an
absolute place, so `margin` (distance from a screen edge) has nothing to
anchor to and is removed with `max_width`.

**Grouping is by region.** "Layers naming the same corner share one box" becomes
"layers naming the same region share one box". An item overlay whose region
equals the global one lands in the global box, exactly as an item naming the
same corner does today. An item overlay with **no** region (`None`) joins the
global box — the replacement for today's empty `position` string.

**Colour recolour is unchanged.** `ItemOverlay::color` still recolours the whole
box it lands in (the one bright-page-in-a-dark-playlist case), resolved in
`settings::overlay_payload` as now. Only the *position* representation changes.

## Data model (`src/settings.rs`)

- New `OverlayRegion { x: u8, y: u8, w: u8, h: u8 }` with a `check()` mirroring
  `layout::Layout::check` for a single box (inside the grid, non-zero size).
  Out-of-grid or zero values **clamp/fall back** on the read path rather than
  erroring, the same treatment an unknown corner gets today.
- `Overlay`: `position: String` → `region: OverlayRegion`; remove `margin` and
  `max_width`.
- `ItemOverlay`: `position: String` → `region: Option<OverlayRegion>`
  (`None` = join the global box).
- **Migration on load** (in the spirit of `Overlay::migrate_legacy_style`): an
  old corner plus its `margin`/`max_width` maps to a preset region. This is the
  same corner→region table the editor's preset buttons use (Editor section), so
  the two cannot drift:
  - `top-left → {0,0,6,4}`, `top-center → {9,0,6,4}`, `top-right → {18,0,6,4}`
  - `bottom-left → {0,20,6,4}`, `bottom-center → {9,20,6,4}`,
    `bottom-right → {18,20,6,4}`
  - any other/unknown corner → `bottom-right`'s region (the current default)

  The legacy `position`/`margin`/`max_width` are read once, folded into
  `region`, and `skip_serializing` so they leave the API on the first
  write-back — the pattern the box-style migration already uses.

  Migration is approximate: a box centred in a small corner region is not
  pixel-identical to the old corner-hugging box, but with the small preset
  regions above it is visually the same. Accepted.

## Runtime (`web/overlay.js`, `web/autoscroll.js` unaffected)

- The payload field `position` (corner string) becomes `region {x,y,w,h}`.
- Placement: an absolutely-positioned container at `left = x/24·100%`,
  `top = y/24·100%`, `width = w/24·100%`, `height = h/24·100%` of the viewport,
  laid out `display:flex; align-items:center; justify-content:center`. The box
  inside auto-sizes and centres; its `max-width` is the region width.
- `alignmentFor` and the `-center`/`-right` handling are removed.
- The five defences stay exactly as they are (shadow DOM + `all: initial`,
  constructable stylesheet, `popover="manual"` top layer, re-attaching
  `MutationObserver`, nothing fetched). Only the positioning maths change.
- `web/overlay.js` is served over HTTP *and* `include_str!`-ed into the binary;
  both copies are the same file, so a rebuild is required after editing it.

## Payload & preview (`src/settings.rs`)

- `settings::overlay_payload` stays the **only** builder of the runtime config.
  It emits `region` instead of `position` and groups boxes by region equality.
- `GET /api/overlay` is still the primary display's, still the source the admin
  preview renders the real runtime against. Unchanged in principle.
- An enabled overlay that would draw nothing is still a `400`.

## Editor (`web/layout-editor.js`, `web/admin.html` or the settings page,
`web/playlist.html`)

- `LayoutEditor` gains a **box mode**: it places a *single* region on the 24×24
  canvas (drag / resize / snap, the existing mechanics) with no URL/asset source
  fields. The aspect ratio is the **primary** display's (the overlay is global),
  taken from the existing viewport plumbing / `screenAspect()`.
- **Presets**, like the layout editor's templates: a row of one-click buttons
  that set the region to a named place — the old corners
  (`Oben links`, `Oben mitte`, `Oben rechts`, `Unten links`, `Unten mitte`,
  `Unten rechts`) plus `Mitte`. They reuse the *same* corner→region table as the
  migration below, so the migration mapping and the buttons cannot drift; the
  buttons then read from one shared source of preset regions
  (`{top-left → {0,0,6,4}}` etc., with `Mitte → {9,10,6,4}`). A preset is just a
  starting point — the operator can still drag/resize afterwards.
- The settings/admin overlay panel: the corner dropdown is replaced by the box
  editor for the global overlay's region.
- `playlist.html` item card: the item-overlay position control is replaced by
  the same box editor; an empty/cleared editor means "join the global box"
  (`region: null`).
- The UI stays dependency-free vanilla JS; rows/controls are built with
  `createElement`/`textContent`, never `innerHTML` interpolation.

## Testing (`tests/cast/test_overlay.py`, stdlib-only)

- Rewrite the corner-based cases to assert region placement: the drawn box's
  computed position sits within the payload's grid region.
- Migration: a stored legacy corner (`+ margin/max_width`) loads as the mapped
  region and leaves the API on write-back.
- An item overlay with its own region draws a second box; with `region: null`
  it shares the global box.
- `ItemOverlay::color` still recolours the shared box (the existing
  recolour case, ported to regions).
- Keep the CSP/computed-style assertion (case that caught the `<style>`-vs-CSSOM
  bug) — it is about the defences, which do not change.
- Run the suite by hand after touching `settings.rs`/`overlay.js`; stop any
  local instance first (port/Chrome collisions read as regressions).

## Docs

- CLAUDE.md overlay section: the corner rules (`alignmentFor`, "no separate
  alignment control", the corner list) are replaced by the region rule; keep the
  five-defences and single-builder rules verbatim.
- `docs/features.md` overlay: describe grid placement.

## Out of scope (YAGNI)

- Multiple independent global overlay boxes (clock in one region, QR in
  another). One box, one region.
- Per-box style beyond today's (still one global style; only the item colour is
  negotiable).
- A per-display overlay. The overlay stays global.
