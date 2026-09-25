# Overlay grid positioning — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Position the overlay by a region on the 24×24 grid (the same grid the layout widgets use) instead of a fixed corner.

**Architecture:** `Overlay.position: String` and `ItemOverlay.position: String` become an `OverlayRegion { x, y, w, h }` (item's is `Option`). The runtime places one absolutely-positioned container per region and centres the auto-sized box inside it. The layout editor gains a single-box mode with corner presets, reused on both the settings page and each playlist item card. Legacy corner configs migrate to preset regions on load.

**Tech Stack:** Rust (axum, serde), vanilla JS (`web/overlay.js`, `web/layout-editor.js`, `web/admin.html`, `web/playlist.html`), stdlib Python tests (`tests/cast/test_overlay.py`).

## Global Constraints

- Grid is **24×24** (`layout::GRID`), same units as `layout::Widget`.
- The box is **auto-size, centred in its region**; region width caps content (text wraps, QR/image scale). **No alignment control.**
- The region **subsumes** `position`, `margin`, `max_width` — those three are removed from `Overlay`.
- **One box** global overlay (clock/QR/text/image together); item overlay is still a second box. Not multi-box.
- Out-of-grid / zero region **clamps or falls back on read**, never errors (screen nobody stands in front of).
- `web/overlay.js` and `web/autoscroll.js` are served over HTTP **and** `include_str!`-ed — same file, **rebuild after editing**. A new `web/` file needs `touch src/web.rs`.
- The five overlay defences (shadow DOM + `all: initial`, constructable stylesheet, `popover="manual"`, re-attaching MutationObserver, nothing fetched) stay verbatim.
- `settings::overlay_payload` stays the only payload builder. `GET /api/overlay` stays the primary display's.
- Corner→region preset table is defined once in Rust (migration) and mirrored in JS (editor presets); a comment in each names the other so they cannot drift.
- Preset regions (the shared table):
  `top-left {0,0,6,4}`, `top-center {9,0,6,4}`, `top-right {18,0,6,4}`,
  `bottom-left {0,20,6,4}`, `bottom-center {9,20,6,4}`, `bottom-right {18,20,6,4}`,
  `Mitte {9,10,6,4}` (editor preset only, no legacy corner maps to it).
- UI is dependency-free vanilla JS, built with `createElement`/`textContent`, never `innerHTML` interpolation.
- Git: no `Co-Authored-By: Claude` / `Claude-Session` trailer.
- Before running the Python suite, stop any local instance (port/Chrome collisions read as regressions). `test_overlay.py` uses Chrome on **9232**.

---

### Task 1: `OverlayRegion` type, model fields, migration, payload (Rust)

**Files:**
- Modify: `src/settings.rs` (structs `Overlay`, `ItemOverlay`; `OVERLAY_POSITIONS`; `sanitized`; `migrate_legacy_style`; `overlay_payload`; unit tests at file end)

**Interfaces:**
- Produces: `struct OverlayRegion { x: u8, y: u8, w: u8, h: u8 }` (serde, Clone, PartialEq); `OverlayRegion::PRESETS: &[(&str, OverlayRegion)]`; `OverlayRegion::sanitized(self) -> Self`; `Overlay.region: OverlayRegion`; `ItemOverlay.region: Option<OverlayRegion>`. Payload emits `"region": {x,y,w,h}` per layer instead of `"position"`, and no longer emits `margin`/`max_width`.

- [ ] **Step 1: Write the failing Rust unit tests**

Add at the end of `src/settings.rs` `#[cfg(test)] mod` (or a new one):

```rust
#[test]
fn a_region_is_pulled_inside_the_grid() {
    let r = OverlayRegion { x: 20, y: 20, w: 10, h: 10 }.sanitized();
    assert!(r.x + r.w <= 24 && r.y + r.h <= 24 && r.w >= 1 && r.h >= 1);
    let z = OverlayRegion { x: 0, y: 0, w: 0, h: 0 }.sanitized();
    assert!(z.w >= 1 && z.h >= 1);
}

#[test]
fn a_legacy_corner_migrates_to_its_preset_region() {
    let mut o = Overlay { position: "top-left".into(), margin: 5.0, max_width: 50.0, ..Overlay::default() };
    o.migrate_legacy_style();
    assert_eq!(o.region, OverlayRegion { x: 0, y: 0, w: 6, h: 4 });
    // The default/unknown corner lands on bottom-right's region.
    let mut u = Overlay { position: "nowhere".into(), ..Overlay::default() };
    u.migrate_legacy_style();
    assert_eq!(u.region, OverlayRegion { x: 18, y: 20, w: 6, h: 4 });
}

#[test]
fn an_item_with_no_region_joins_the_global_box() {
    let item = ItemOverlay { enabled: true, text: "x".into(), region: None, ..ItemOverlay::default() };
    assert!(item.region.is_none()); // payload test in Python asserts the shared box
}
```

Note: this requires `Overlay`/`ItemOverlay` to keep the legacy `position`/`margin`/`max_width` fields as `#[serde(skip_serializing)] Option`/plain for migration input. Define them as inputs only (see Step 3).

- [ ] **Step 2: Run and watch it fail to compile** — `cargo test 2>&1 | tail` → fails (`OverlayRegion` undefined, `region` field missing).

- [ ] **Step 3: Add the type, preset table, fields, migration, sanitize**

In `src/settings.rs`:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayRegion {
    pub x: u8,
    pub y: u8,
    pub w: u8,
    pub h: u8,
}

impl Default for OverlayRegion {
    // bottom-right, the corner the overlay defaulted to.
    fn default() -> Self { Self { x: 18, y: 20, w: 6, h: 4 } }
}

impl OverlayRegion {
    /// The corner-name -> region table. The migration below reads it, and
    /// `web/layout-editor.js` mirrors it for its preset buttons -- keep the two
    /// in step. (The editor adds "Mitte" {9,10,6,4}, which no legacy corner uses.)
    pub const PRESETS: &'static [(&'static str, OverlayRegion)] = &[
        ("top-left", OverlayRegion { x: 0, y: 0, w: 6, h: 4 }),
        ("top-center", OverlayRegion { x: 9, y: 0, w: 6, h: 4 }),
        ("top-right", OverlayRegion { x: 18, y: 0, w: 6, h: 4 }),
        ("bottom-left", OverlayRegion { x: 0, y: 20, w: 6, h: 4 }),
        ("bottom-center", OverlayRegion { x: 9, y: 20, w: 6, h: 4 }),
        ("bottom-right", OverlayRegion { x: 18, y: 20, w: 6, h: 4 }),
    ];

    /// Clamp into the grid rather than reject: nonsense here is visible on a
    /// screen nobody is standing in front of, so it is pulled into range.
    pub fn sanitized(self) -> Self {
        let x = self.x.min(23);
        let y = self.y.min(23);
        let w = self.w.clamp(1, 24 - x);
        let h = self.h.clamp(1, 24 - y);
        Self { x, y, w, h }
    }

    fn for_corner(name: &str) -> Self {
        Self::PRESETS.iter().find(|(n, _)| *n == name).map(|(_, r)| *r)
            .unwrap_or_default()
    }
}
```

In `Overlay`: replace `pub position: String,` with `pub region: OverlayRegion,`, delete `pub margin: f32,` and `pub max_width: f32,`. Add legacy inputs (migration-only):

```rust
    /// Legacy corner, migrated into `region` on load and never written again.
    #[serde(default, skip_serializing)]
    pub position: Option<String>,
    #[serde(default, skip_serializing)]
    pub margin: Option<f32>,
    #[serde(default, skip_serializing)]
    pub max_width: Option<f32>,
```

Update `Overlay::default()`: drop `position`/`margin`/`max_width` value lines that set the old `String`/`f32`, add `region: OverlayRegion::default(),`, and `position: None, margin: None, max_width: None,`.

In `migrate_legacy_style` (add near the box-style migration): if `region` is still default *and* a legacy `position` was given, set `region = OverlayRegion::for_corner(name)`; then `self.position = None`. (Region is `Copy`; treat "still default and legacy present" as the migration trigger. Because `region` deserialises to its `Default` when absent, an old stored config that has `position` but no `region` migrates; a new config with a real `region` and no `position` is left alone.)

```rust
    pub fn migrate_legacy_style(&mut self) {
        // ... existing colour/opacity migration stays ...
        if let Some(corner) = self.position.take() {
            // Only when this config predates regions: a new one carries `region`
            // and no `position`, so nothing to migrate.
            if self.region == OverlayRegion::default() {
                self.region = OverlayRegion::for_corner(&corner);
            }
        }
        self.margin = None;
        self.max_width = None;
    }
```

In `Overlay::sanitized`: delete the `OVERLAY_POSITIONS`/`position` fallback and the `margin`/`max_width` clamps; add `self.region = self.region.sanitized();`. Keep `migrate_legacy_style()` call (it must run so a freshly-loaded legacy config migrates before `region` is read).

In `ItemOverlay`: replace `pub position: String,` with `pub region: Option<OverlayRegion>,` and add a legacy input:

```rust
    #[serde(default, skip_serializing)]
    pub position: Option<String>,
```

In `ItemOverlay::sanitized`: replace the `position` corner check with:
```rust
    if let Some(corner) = self.position.take() {
        if self.region.is_none() && !corner.is_empty() {
            self.region = Some(OverlayRegion::for_corner(&corner));
        }
    }
    self.region = self.region.map(OverlayRegion::sanitized);
```

Delete `OVERLAY_POSITIONS` (now unused) — grep first; it is used only in `settings.rs`.

- [ ] **Step 4: Update `overlay_payload`**

In `src/settings.rs::overlay_payload`:
- `recolour` target: change `item.position` / `overlay.position` comparisons to regions. The recolour target key is now the region. Compute:
```rust
    let recolour = item.filter(|item| item.recolours()).map(|item| {
        let target = if item.draws() { item.region } else { None }
            .unwrap_or(overlay.region);
        (target, item.color.clone())
    });
```
- Global layer: after `serde_json::to_value(&overlay)`, the layer already carries `region` (serialises from the struct). The recolour stamp: `if *target == overlay.region { object.insert("color", ...) }`.
- Item layer: replace the `position` block with:
```rust
        let region = item.region.unwrap_or(overlay.region);
        layers.push(json!({
            "region": region,
            "text": item.text,
            "image_data": image_data_uri(state, item.image_asset_id).await,
            "qr_modules": qr_modules(&item.qr_text),
            "qr_label": item.qr_label,
            "size": overlay.size,
            "qr_size": overlay.qr_size,
            "background_color": overlay.background_color,
            "background_alpha": overlay.background_alpha,
            "plain": overlay.plain,
            "background_css": overlay.background_css,
            "color": if item.recolours() { item.color.clone() } else { overlay.color.clone() },
            "color_alpha": overlay.color_alpha,
        }));
```
  (Note: `margin`/`max_width` are gone from the item layer JSON.)

- [ ] **Step 5: Build and test**

Run: `cargo test 2>&1 | tail -6`
Expected: the three new tests pass; whole suite `ok`. Fix any other `settings.rs` reference to the removed fields the compiler flags.

- [ ] **Step 6: Commit**

```bash
git add src/settings.rs
git commit -m "Overlay position becomes a 24x24 grid region"
```

---

### Task 2: `overlay.js` places boxes by region (JS runtime)

**Files:**
- Modify: `web/overlay.js` (`CORNERS`, `ensureHost`, `styles`, `alignmentFor`, `apply` grouping, `DEFAULTS`)
- Modify: `src/web.rs` — only if `cargo` fails to notice the change (it will notice an edit; no `touch` needed for an edit).

**Interfaces:**
- Consumes: payload layers each with `region: {x,y,w,h}` (Task 1).
- Produces: the runtime draws a container per region and centres the box.

- [ ] **Step 1: Replace corner CSS with a region container.** Delete the `CORNERS` map and `alignmentFor`. Add:

```js
  function regionKey(r) { return `${r.x},${r.y},${r.w},${r.h}`; }
  function regionCss(r) {
    // The region is a fixed, invisible frame; the box centres inside it.
    return `position: fixed;`
      + ` left: ${(r.x / 24) * 100}%; top: ${(r.y / 24) * 100}%;`
      + ` width: ${(r.w / 24) * 100}%; height: ${(r.h / 24) * 100}%;`
      + ` display: flex; align-items: center; justify-content: center;`;
  }
```

- [ ] **Step 2: Key boxes by region, not corner.** In `ensureHost(position)` rename the parameter to `region` (an object), key `boxes` by `regionKey(region)`, and set the host container style from `regionCss(region)` (keep `all: initial` first, keep the `z-index` fallback). The host is the region frame; the shadow content is the auto-sized box.

- [ ] **Step 3: `styles(style, region)`** — the inner box's stylesheet. Remove the `${corner}` and `--m` margin, remove `text-align`/`justify-content` alignment (always centred). The box is `position: static; max-width: ${(region.w/24)*100}vw;` and centres its own content (`text-align: center; align-items: center;`, `.qrwrap { justify-content: center; }`). Keep the colour/background/padding/rounded-corner rules.

- [ ] **Step 4: `apply` grouping by region.** Where it groups layers, group by `regionKey(layer.region)` (fall back to `DEFAULTS.region` when a layer has no region). `dropBox`/reconcile loops key by the same string. `ensureHost` is called with the region object (parse it back from the first layer of the group, which carries `region`).

- [ ] **Step 5: `DEFAULTS`.** Replace `position: 'bottom-right'` and the `margin`/`max_width` defaults with `region: { x: 18, y: 20, w: 6, h: 4 }`. In the merge loop drop `margin`/`max_width` keys; keep `size`, `qr_size`. Where it validated `CORNERS[merged.position]`, instead ensure `merged.region` is an object with numeric `x,y,w,h`, else set `DEFAULTS.region`.

- [ ] **Step 6: Rebuild and eyeball** — `cargo build 2>&1 | tail -1`. (Browser assertion is Task 5.)

- [ ] **Step 7: Commit**

```bash
git add web/overlay.js
git commit -m "Overlay runtime draws boxes by grid region"
```

---

### Task 3: `layout-editor.js` box mode with presets (JS)

**Files:**
- Modify: `web/layout-editor.js` (`create` options; preset table; single-box path; `read`)

**Interfaces:**
- Produces: `LayoutEditor.create({ boxMode: true, region, aspect, allowEmpty, el })` → `{ root, read() }` where `read()` returns `{x,y,w,h}` (or `null` when `allowEmpty` and no box). `LayoutEditor.OVERLAY_PRESETS` (name → `[x,y,w,h]`).

- [ ] **Step 1: Add the preset table** near `TEMPLATES`:

```js
  // Mirrors OverlayRegion::PRESETS in src/settings.rs -- keep in step.
  const OVERLAY_PRESETS = {
    'Oben links': [0, 0, 6, 4], 'Oben mitte': [9, 0, 6, 4], 'Oben rechts': [18, 0, 6, 4],
    'Unten links': [0, 20, 6, 4], 'Unten mitte': [9, 20, 6, 4], 'Unten rechts': [18, 20, 6, 4],
    'Mitte': [9, 10, 6, 4],
  };
```

- [ ] **Step 2: Box mode in `create`.** Accept `{ boxMode, region, allowEmpty }`. When `boxMode`:
  - Seed `widgets` from `region` (a single `{ box: {x,y,w,h}, source: null }`) or empty when `allowEmpty` and `region` is null.
  - `MAX = 1`.
  - Skip the source/scroll/fit field editors entirely (guard the per-widget `editors` build with `if (!boxMode)`).
  - Render a **presets row**: one button per `OVERLAY_PRESETS` entry that sets the single box to that rectangle (create it if empty) and re-renders. When `allowEmpty`, add an `Aus (global)` button that removes the box.
  - The "add widget" affordance is replaced: in box mode there is at most one box; if empty, a single "Box hinzufügen" button (or the presets create it).
  - `read()`: in box mode return the single box `{x,y,w,h}`, or `null` when empty.

- [ ] **Step 3: Export presets** — `window.LayoutEditor.OVERLAY_PRESETS = OVERLAY_PRESETS;` beside the existing export, so callers can label buttons without duplicating the table.

- [ ] **Step 4: Sanity build** — `cargo build 2>&1 | tail -1` (file is bundled; a JS syntax error will not fail cargo, so also open the page in Task 4's manual check).

- [ ] **Step 5: Commit**

```bash
git add web/layout-editor.js
git commit -m "Layout editor gains a single-box mode with overlay presets"
```

---

### Task 4: Wire the box editor into the settings and item overlay UIs (JS/HTML)

**Files:**
- Modify: `web/admin.html` (overlay card markup + `ov` map + `applyOverlay`/collect; fetch primary display aspect)
- Modify: `web/playlist.html` (`overlayEditor`)

**Interfaces:**
- Consumes: `LayoutEditor.create({ boxMode, ... })` (Task 3); `/api/overlay` payload `region`; `/api/displays` primary viewport.

- [ ] **Step 1: admin.html markup.** Remove the `ovPosition` `<select>` block and the `ovMargin`/`ovMaxWidth` inputs. Add a container `<div id="ovРegion"></div>` (ascii id `ovRegion`) where the box editor mounts, with a label "Position (Raster)".

- [ ] **Step 2: admin.html JS.** In the `ov` field map remove `position`, `margin`, `max_width`. After settings load, build the editor:
```js
    let overlayRegionEditor = null;
    function mountOverlayRegion(region, aspect) {
      const host = document.getElementById('ovRegion');
      host.textContent = '';
      overlayRegionEditor = LayoutEditor.create({
        boxMode: true, region: region || { x: 18, y: 20, w: 6, h: 4 },
        aspect, allowEmpty: false, el,
      });
      host.append(overlayRegionEditor.root);
    }
```
  `el` is the same helper admin.html uses elsewhere (define a tiny local `el` if admin.html lacks one — check first; if absent, add a minimal `el(tag, attrs, ...kids)`).
  The aspect: `fetch('/api/displays')` → primary is `displays[0]`; `aspect = d.viewport_width / d.viewport_height` (fallback `16/9`). Call `mountOverlayRegion(overlay.region, aspect)` inside `applyOverlay`.
  In the overlay collect (where the PUT body is built from `ov`), add `body.region = overlayRegionEditor.read();`.

- [ ] **Step 3: playlist.html `overlayEditor`.** Remove the `position` `<select>`. Mount a box editor with `allowEmpty: true`, seeded from `cfg.region || null`, aspect from `screenAspect()` (already used by the layout editor on this page). Replace the `field('Ecke', position)` cell with `field('Position (Raster)', regionEditor.root)`. In `read()` replace `position: position.value` with `region: regionEditor.read()`.

- [ ] **Step 4: Manual smoke test.** Start a local instance (own scratch DB, `--managed-cert off`), open `/admin.html` and `/playlist.html`, confirm the grid editor renders, presets set the box, and saving round-trips (`GET /api/overlay` shows the region). Stop it before the Python suite.

- [ ] **Step 5: Commit**

```bash
git add web/admin.html web/playlist.html
git commit -m "Place the overlay with the grid editor on both pages"
```

---

### Task 5: Rewrite `test_overlay.py` for regions + migration (Python)

**Files:**
- Modify: `tests/cast/test_overlay.py`

**Interfaces:**
- Consumes: the region API (`region` field), the runtime (`__ov.state()`), `getComputedStyle`/`getBoundingClientRect`.

- [ ] **Step 1: Settings cases [40].** Replace `position` asserts: send `"region": {"x":0,"y":0,"w":6,"h":4}` and assert it round-trips; send an out-of-grid region (`{"x":20,"y":20,"w":10,"h":10}`) and assert it clamps inside the grid (read back `x+w<=24`). Remove the `margin`/`max_width` asserts.

- [ ] **Step 2: Migration case (new, [40c]).** `PUT` a legacy body `{"overlay": {"enabled": true, "show_clock": true, "position": "top-left", "margin": 5, "max_width": 50}}`; assert the stored overlay has `region == {"x":0,"y":0,"w":6,"h":4}` and no `position`/`margin`/`max_width` fields.

- [ ] **Step 3: Resolved config [41].** Assert the payload layer carries `region`. The item-with-no-region case: `"region": null` (or omitted) shares the global box.

- [ ] **Step 4: Badge on page [43].** The box's `getBoundingClientRect` sits inside the region: for `region {18,20,6,4}` on viewport `W×H`, assert box centre `left > W*0.5` and `top > H*0.5`. Keep "one box in use" (`state.boxes count == 1`).

- [ ] **Step 5: Replace [44b] alignment.** Alignment is gone; replace with a "the box is centred in its region" assertion: mount `region {9,10,6,4}` (centre) and assert the box's centre is near viewport centre (`abs(cx - W/2) < W*0.15`).

- [ ] **Step 6: Item + override [45].** Item with its own region draws a second box; item with `region: null` shares the global box (assert `boxes count`). Port the recolour case ([45]/recolour) to regions: item `region` equal to global's recolours the shared box.

- [ ] **Step 7: Keep [44], [46a], [48], [48b]** — colour/alpha, the CSP computed-style case, seed count, top-frame guard. Only adjust the `position`→`region` in their fixtures; the defences and seeding are unchanged.

- [ ] **Step 8: Run the suite (stop local instance first)**

Run: `cd tests/cast && python3 test_overlay.py 2>&1 | tail -20`
Expected: `ALL PASSED`.

- [ ] **Step 9: Commit**

```bash
git add tests/cast/test_overlay.py
git commit -m "Test overlay grid regions and the corner migration"
```

---

### Task 6: Docs + full verification

**Files:**
- Modify: `CLAUDE.md` (overlay section), `docs/features.md` (overlay), the spec status line.

- [ ] **Step 1: CLAUDE.md.** In the overlay section, replace the corner rules (`alignmentFor`, "Deliberately no separate alignment control", the corner list, `margin`/`max_width` mentions) with the region rule: position is a 24×24 region, box auto-size centred, width caps content, one box global + one item, migration from corners via the shared preset table. Keep the five-defences and single-builder paragraphs verbatim. Note `web/overlay.js` dual-served rebuild rule stays.

- [ ] **Step 2: docs/features.md.** Update the overlay description to grid placement with presets.

- [ ] **Step 3: Flip the spec status** in `docs/superpowers/specs/2026-09-24-overlay-grid-design.md` to `implemented`.

- [ ] **Step 4: Full verification.**
```bash
cargo test 2>&1 | tail -3
cd tests/cast && for t in test_overlay test_settings; do echo "== $t"; python3 $t.py 2>&1 | grep -E 'FAILED|ALL PASSED'; done
```
Expected: cargo `ok`, both suites `ALL PASSED`. (`test_settings` touches the settings round-trip.)
Then cross-compile check, since no new deps but the model changed:
```bash
cross build --target armv7-unknown-linux-gnueabihf 2>&1 | tail -3
```
Expected: `Finished`.

- [ ] **Step 5: Commit**

```bash
git add CLAUDE.md docs/features.md docs/superpowers/specs/2026-09-24-overlay-grid-design.md
git commit -m "Document overlay grid positioning"
```

---

## Self-Review

**Spec coverage:** region type + fields (T1), subsumes margin/max_width (T1), migration + shared preset table (T1/T3), runtime centred-in-region (T2), no alignment (T2/T5), editor box mode + presets (T3), settings+item wiring, primary-display aspect (T4), payload single builder + region grouping + recolour (T1), tests incl. migration and centred (T5), docs (T6). All covered.

**Placeholder scan:** none — every code step carries the code or the exact edit target.

**Type consistency:** `OverlayRegion {x,y,w,h}` (Rust) ↔ `region {x,y,w,h}` (JSON/JS) ↔ `read()` returns `{x,y,w,h}` throughout. `PRESETS` (Rust) ↔ `OVERLAY_PRESETS` (JS), names differ by language convention, both documented as mirrors. `region: Option` (item) ↔ `null`/omitted (JSON) ↔ `allowEmpty` (editor).
