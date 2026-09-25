# Built-in widgets — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Controller-rendered content — clock, banner, QR, countdown — usable as a layout widget source and as a standalone playlist item.

**Architecture:** A `builtin::Builtin` tagged enum resolves, in `src/builtin.rs`, to `http://127.0.0.1:<port>/widget.html?c=<base64url json>`, the payload carrying the kind, its options, and server-only bits (QR matrix, cast URL, locale). `web/widget.html` decodes it and renders, ticking client-side for clock/countdown. The same resolver feeds both `layout::widgets_for_display` (widget) and `browser::playlist_target_url` (standalone item).

**Tech Stack:** Rust (serde, sqlx, base64), vanilla JS (`web/widget.html`, `web/builtin-editor.js`, `web/layout-editor.js`, `web/playlist.html`), stdlib Python tests.

## Global Constraints

- A built-in resolves to `/widget.html?c=<base64url json>` in **one place** (`src/builtin.rs`); `widget.html` fetches nothing.
- QR matrix from `cast::url::qr_matrix(text) -> Option<Vec<String>>`, guest URL from `cast::url::sender_url(state, Option<&Display>) -> String`, locale from settings — the same sources the overlay uses, so nothing drifts.
- Item is **exactly one of** url / asset / layout / builtin. `advance` for a builtin is **time-only** (`advance::check` / handler refuse `Passes`).
- Schema only in `db::run_migrations` (`CREATE` + `pragma_table_info` probe); `builtin` column `TEXT DEFAULT 'null'`, `COALESCE`d on read like `layout`.
- `web/` is compiled in via `include_dir!`; after editing rebuild, and a **new** file needs `touch src/web.rs`. `widget.html` must be in `is_display_path` (loopback display loads it with no creds), like `layout.html`.
- Values clamp/fall back on read, never error on the display path.
- Caps: `Banner.text`/`Qr.qr_text` ≤ 500 chars, `Countdown.label`/`done_text` ≤ 100; resolver caps encoded `c`.
- UI dependency-free, `createElement`/`textContent`, never `innerHTML` interpolation.
- Git: no `Co-Authored-By: Claude` / `Claude-Session` trailer. Stop any local instance before the Python suite.

---

### Task 1: `Builtin` type, resolver, and the widget page

**Files:**
- Create: `src/builtin.rs`
- Create: `web/widget.html`
- Modify: `src/main.rs` (`mod builtin;`, add `/widget.html` to `is_display_path`), `src/web.rs` (`touch`)

**Interfaces:**
- Produces: `builtin::Builtin` (enum), `builtin::resolve(state: &AppState, b: &Builtin) -> String`, `builtin::widget_url(port: u16, payload: &serde_json::Value) -> String`.

- [ ] **Step 1: Write `src/builtin.rs` with the type, resolver, and unit tests**

```rust
//! Content the controller renders itself -- a clock, a banner, a QR code, a
//! countdown -- as a widget in a layout or a standalone playlist item. Both go
//! through `resolve`, which encodes a self-contained payload into the URL of one
//! page, `web/widget.html`. The server is the only place that resolves a
//! built-in, so the QR/cast/locale it needs cannot drift from the overlay's.

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::models::AppState;

/// The four built-in kinds. Internally tagged by `kind`, so a `Source::Builtin`
/// object is `{ "kind": "clock", ... }`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Builtin {
    Clock {
        #[serde(default = "yes")]
        format_24h: bool,
        #[serde(default)]
        show_seconds: bool,
        #[serde(default)]
        show_date: bool,
        #[serde(default)]
        timezone: String,
        #[serde(flatten)]
        style: Style,
    },
    Banner {
        #[serde(default)]
        text: String,
        #[serde(default = "fill")]
        mode: String, // "fill" | "normal"
        #[serde(default = "medium")]
        size: String, // normal mode: "small" | "medium" | "large"
        #[serde(default = "center")]
        align: String, // normal mode: "center" | "left"
        #[serde(flatten)]
        style: Style,
    },
    Qr {
        #[serde(default = "text_src")]
        source: String, // "text" | "cast"
        #[serde(default)]
        qr_text: String,
        #[serde(default)]
        label: String,
        #[serde(flatten)]
        style: Style,
    },
    Countdown {
        #[serde(default)]
        target: String, // ISO datetime
        #[serde(default)]
        label: String,
        #[serde(default)]
        done_text: String,
        #[serde(flatten)]
        style: Style,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Style {
    #[serde(default = "black")]
    pub background_color: String,
    #[serde(default = "white")]
    pub text_color: String,
}

impl Default for Style {
    fn default() -> Self { Self { background_color: black(), text_color: white() } }
}

fn yes() -> bool { true }
fn fill() -> String { "fill".into() }
fn medium() -> String { "medium".into() }
fn center() -> String { "center".into() }
fn text_src() -> String { "text".into() }
fn black() -> String { "#000000".into() }
fn white() -> String { "#ffffff".into() }

impl Builtin {
    /// Clamp the caps that would otherwise bloat the URL. Not an error: the
    /// display has nobody in front of it.
    pub fn sanitized(mut self) -> Self {
        let cap = |s: &mut String, n: usize| { if s.chars().count() > n { *s = s.chars().take(n).collect(); } };
        match &mut self {
            Builtin::Banner { text, .. } => cap(text, 500),
            Builtin::Qr { qr_text, label, .. } => { cap(qr_text, 500); cap(label, 100); }
            Builtin::Countdown { label, done_text, .. } => { cap(label, 100); cap(done_text, 100); }
            Builtin::Clock { .. } => {}
        }
        self
    }
}

/// The payload the page renders, with the fields only the server can supply.
pub async fn resolve(state: &AppState, display: Option<&crate::models::Display>, b: &Builtin) -> String {
    let mut payload = serde_json::to_value(b).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(object) = payload.as_object_mut() {
        // Locale for the clock/countdown, taken once from settings.
        let (locale, cast_enabled, cast_qr_target) = {
            let s = state.settings.read().await;
            (s.locale.clone(), s.cast_enabled, s.cast_qr_target)
        };
        object.insert("locale".into(), serde_json::json!(locale));
        if let Builtin::Qr { source, qr_text, .. } = b {
            let target = if source == "cast" {
                if cast_enabled {
                    match cast_qr_target {
                        crate::models::CastQrTarget::Screen => crate::cast::sender_url(state, display),
                        crate::models::CastQrTarget::Chooser => crate::cast::sender_url(state, None),
                    }
                } else {
                    String::new()
                }
            } else {
                qr_text.clone()
            };
            object.insert("qr_modules".into(), serde_json::json!(crate::cast::qr_matrix(&target)));
        }
    }
    widget_url(state.args.port, &payload)
}

pub fn widget_url(port: u16, payload: &serde_json::Value) -> String {
    let json = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    let c = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json.as_bytes());
    format!("http://127.0.0.1:{port}/widget.html?c={c}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_round_trips() {
        for b in [
            Builtin::Clock { format_24h: true, show_seconds: true, show_date: true, timezone: "Europe/Berlin".into(), style: Style::default() },
            Builtin::Banner { text: "Zu".into(), mode: "fill".into(), size: "medium".into(), align: "center".into(), style: Style::default() },
            Builtin::Qr { source: "cast".into(), qr_text: String::new(), label: "Teilen".into(), style: Style::default() },
            Builtin::Countdown { target: "2030-01-01T00:00:00".into(), label: "Bis".into(), done_text: "Vorbei".into(), style: Style::default() },
        ] {
            let v = serde_json::to_value(&b).unwrap();
            assert_eq!(serde_json::from_value::<Builtin>(v).unwrap(), b);
        }
    }

    #[test]
    fn the_kind_tag_is_present() {
        let v = serde_json::to_value(Builtin::Clock { format_24h: true, show_seconds: false, show_date: false, timezone: String::new(), style: Style::default() }).unwrap();
        assert_eq!(v["kind"], "clock");
    }

    #[test]
    fn a_long_banner_is_capped() {
        let long = "x".repeat(1000);
        let Builtin::Banner { text, .. } = (Builtin::Banner { text: long, mode: fill(), size: medium(), align: center(), style: Style::default() }).sanitized() else { panic!() };
        assert_eq!(text.chars().count(), 500);
    }

    #[test]
    fn widget_url_is_the_widget_page_with_a_payload() {
        let url = widget_url(3000, &serde_json::json!({"kind":"clock"}));
        assert!(url.starts_with("http://127.0.0.1:3000/widget.html?c="));
        let c = url.split("c=").nth(1).unwrap();
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(c).unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&json).unwrap()["kind"], "clock");
    }
}
```

- [ ] **Step 2: Confirm `base64` is a dependency**

Run: `grep -n '^base64' Cargo.toml || echo MISSING`
If MISSING: `cargo add base64` (it cross-compiles; it is pure Rust). Expected: present after.

- [ ] **Step 3: Register the module and the display path**

In `src/main.rs`: add `mod builtin;` beside `mod layout;`. In `is_display_path`, add `"/widget.html"` to the `matches!` list beside `"/layout.html"`.

- [ ] **Step 4: Write `web/widget.html`** (see full file below)

```html
<!doctype html>
<html lang="de">
<head>
<meta charset="utf-8" />
<title>Widget</title>
<style>
  :root { color-scheme: dark; }
  html, body { margin: 0; height: 100%; }
  body { display: flex; }
  #box { flex: 1; display: flex; flex-direction: column; align-items: center;
         justify-content: center; gap: 0.4em; overflow: hidden; text-align: center;
         font-family: system-ui, sans-serif; font-weight: 600; line-height: 1.2;
         padding: 2vmin; box-sizing: border-box; }
  #box.left { align-items: flex-start; text-align: left; }
  .main { font-variant-numeric: tabular-nums; }
  .sub { opacity: 0.85; }
  svg.qr { background: #fff; border-radius: 0.4em; width: min(80vw, 80vh); height: min(80vw, 80vh); }
</style>
</head>
<body>
<div id="box"><div class="main" id="main"></div><div class="sub" id="sub"></div></div>
<script>
  'use strict';
  // Decode the self-contained payload the server put in ?c=. Nothing is fetched.
  function config() {
    try {
      const c = new URLSearchParams(location.search).get('c') || '';
      const json = atob(c.replace(/-/g, '+').replace(/_/g, '/'));
      return JSON.parse(json);
    } catch (_) { return { kind: 'banner', text: '' }; }
  }
  const cfg = config();
  const box = document.getElementById('box');
  const main = document.getElementById('main');
  const sub = document.getElementById('sub');
  box.style.background = cfg.background_color || '#000';
  box.style.color = cfg.text_color || '#fff';

  // Scale the main line to fill the cell, within a few iterations.
  function fit(node, maxVmin) {
    let lo = 1, hi = maxVmin || 40;
    for (let i = 0; i < 12; i++) {
      const mid = (lo + hi) / 2;
      node.style.fontSize = mid + 'vmin';
      const over = node.scrollWidth > box.clientWidth - 8 || node.scrollHeight > box.clientHeight - 8;
      if (over) hi = mid; else lo = mid;
    }
    node.style.fontSize = lo + 'vmin';
  }

  function pad(n) { return String(n).padStart(2, '0'); }

  function renderClock() {
    const fmt = new Intl.DateTimeFormat(cfg.locale || undefined, {
      hour: '2-digit', minute: '2-digit',
      second: cfg.show_seconds ? '2-digit' : undefined,
      hour12: !cfg.format_24h,
      timeZone: cfg.timezone || undefined,
    });
    const dfmt = new Intl.DateTimeFormat(cfg.locale || undefined, {
      weekday: 'long', year: 'numeric', month: 'long', day: 'numeric',
      timeZone: cfg.timezone || undefined,
    });
    const tick = () => {
      const now = new Date();
      main.textContent = fmt.format(now);
      sub.textContent = cfg.show_date ? dfmt.format(now) : '';
      fit(main, 40); if (cfg.show_date) fit(sub, 8);
    };
    tick();
    setInterval(tick, cfg.show_seconds ? 1000 : 15000);
  }

  function renderBanner() {
    main.textContent = cfg.text || '';
    if (cfg.mode === 'normal') {
      main.style.whiteSpace = 'pre-wrap';
      main.style.overflowWrap = 'anywhere';
      main.style.fontSize = ({ small: 3, medium: 5, large: 8 })[cfg.size] || 5 + 'vmin';
      main.style.fontSize = (({ small: 3, medium: 5, large: 8 })[cfg.size] || 5) + 'vmin';
      if (cfg.align === 'left') box.classList.add('left');
    } else {
      main.style.whiteSpace = 'pre-wrap';
      fit(main, 40);
    }
  }

  function renderQr() {
    const rows = cfg.qr_modules;
    if (!Array.isArray(rows) || !rows.length) { main.textContent = ''; sub.textContent = cfg.label || ''; return; }
    const NS = 'http://www.w3.org/2000/svg';
    const n = rows.length, q = 2, span = n + q * 2;
    const svg = document.createElementNS(NS, 'svg');
    svg.setAttribute('class', 'qr');
    svg.setAttribute('viewBox', `0 0 ${span} ${span}`);
    const bg = document.createElementNS(NS, 'rect');
    bg.setAttribute('width', span); bg.setAttribute('height', span); bg.setAttribute('fill', '#fff');
    svg.appendChild(bg);
    rows.forEach((row, y) => {
      let x = 0;
      while (x < n) {
        if (row[x] === '1') {
          let w = 1; while (x + w < n && row[x + w] === '1') w++;
          const r = document.createElementNS(NS, 'rect');
          r.setAttribute('x', x + q); r.setAttribute('y', y + q);
          r.setAttribute('width', w); r.setAttribute('height', 1); r.setAttribute('fill', '#000');
          svg.appendChild(r); x += w;
        } else x++;
      }
    });
    main.replaceChildren(svg);
    sub.textContent = cfg.label || '';
    if (sub.textContent) fit(sub, 6);
  }

  function renderCountdown() {
    const target = new Date(cfg.target);
    const tick = () => {
      const ms = target - new Date();
      if (Number.isNaN(target.getTime()) || ms <= 0) {
        main.textContent = cfg.done_text || '';
      } else {
        const d = Math.floor(ms / 86400000);
        const h = Math.floor(ms % 86400000 / 3600000);
        const m = Math.floor(ms % 3600000 / 60000);
        main.textContent = d > 0 ? `noch ${d} T ${h} Std` : (h > 0 ? `noch ${h} Std ${m} Min` : `noch ${m} Min`);
      }
      sub.textContent = cfg.label || '';
      fit(main, 30); if (sub.textContent) fit(sub, 6);
    };
    tick();
    setInterval(tick, 30000);
  }

  ({ clock: renderClock, banner: renderBanner, qr: renderQr, countdown: renderCountdown }[cfg.kind] || renderBanner)();
  addEventListener('resize', () => location.reload());
</script>
</body>
</html>
```

  (Note: in `renderBanner` normal mode, keep the single correct `fontSize` line — the duplicated line above it is removed when you paste; the correct form is `(({ small: 3, medium: 5, large: 8 })[cfg.size] || 5) + 'vmin'`.)

- [ ] **Step 5: `touch src/web.rs`** (new `web/` file), then build and test.

Run: `touch src/web.rs && cargo test builtin 2>&1 | tail -6`
Expected: the four `builtin::tests` pass; whole build ok.

- [ ] **Step 6: Serve check** — start a scratch instance, `curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:<port>/widget.html` → `200`. Stop it.

- [ ] **Step 7: Commit**

```bash
git add src/builtin.rs web/widget.html src/main.rs src/web.rs Cargo.toml Cargo.lock
git commit -m "Built-in widgets: the four kinds, the resolver, and the widget page"
```

---

### Task 2: Built-in as a layout widget source

**Files:**
- Modify: `src/layout.rs` (`Source` enum, `widgets_for_display`, tests)

**Interfaces:**
- Consumes: `builtin::Builtin`, `builtin::resolve`.

- [ ] **Step 1: Add a failing test** in `src/layout.rs` `mod tests`:

```rust
#[test]
fn a_builtin_widget_round_trips_and_needs_no_asset() {
    let w = Widget {
        x: 0, y: 0, w: 12, h: 12,
        source: Source::Builtin(crate::builtin::Builtin::Clock {
            format_24h: true, show_seconds: false, show_date: false,
            timezone: String::new(), style: crate::builtin::Style::default(),
        }),
        scroll_config: Default::default(), fit_mode: None, fit_background: None,
    };
    let layout = Layout { widgets: vec![w] };
    assert!(layout.check().is_ok());
    assert!(layout.asset_ids().is_empty());
    let v = serde_json::to_value(&layout).unwrap();
    assert_eq!(serde_json::from_value::<Layout>(v).unwrap(), layout);
}
```

- [ ] **Step 2: Run — fails** (`Source::Builtin` undefined). `cargo test -p miniclientcontrol a_builtin_widget 2>&1 | tail`.

- [ ] **Step 3: Add the variant** to `layout::Source`:

```rust
#[serde(untagged)]
pub enum Source {
    Url { url: String },
    Asset { asset_id: i64 },
    Builtin(crate::builtin::Builtin),
}
```

`asset_ids()` already `match`es `Source`; add `Source::Builtin { .. } => None` (or the wildcard `_ => None`). `check()`'s URL guard is inside `if let Source::Url` so it already ignores built-ins.

- [ ] **Step 4: Resolve it in `widgets_for_display`** — in the `match &widget.source` add:

```rust
Source::Builtin(b) => crate::builtin::resolve(&state, None, b).await,
```

(No display scope here: the layout preview/display resolves against the primary; a `cast`-QR built-in in a layout uses the chooser/primary URL, which is acceptable and matches `/api/overlay` being the primary's.)

- [ ] **Step 5: Run tests** — `cargo test -p miniclientcontrol 2>&1 | tail -3`. Expected: ok. If `untagged` mis-resolves a `{kind}` object (it should not — Url needs `url`, Asset needs `asset_id`), add an explicit round-trip assertion for all three variants and, only if it fails, switch `Source` to `#[serde(tag="type")]`-free internal tagging; otherwise leave untagged.

- [ ] **Step 6: Commit**

```bash
git add src/layout.rs
git commit -m "A layout widget can be a built-in"
```

---

### Task 3: Built-in as a standalone playlist item

**Files:**
- Modify: `src/db.rs` (column + probe), `src/models.rs` (`builtin` field), `src/handlers.rs` (source count, INSERT, like-for-like, advance), `src/browser.rs` (`playlist_target_url`, SELECT), `src/advance.rs` (refuse `Passes` for builtin — via handler, see below)

**Interfaces:**
- Consumes: `builtin::Builtin`, `builtin::resolve`.

- [ ] **Step 1: DB column** — in `db::run_migrations`, in the `CREATE TABLE playlist_items` add `builtin TEXT DEFAULT 'null',` beside `layout`, and add the probe beside the layout one:

```rust
if !playlist_items_has(pool, "builtin").await {
    let _ = sqlx::query("ALTER TABLE playlist_items ADD COLUMN builtin TEXT DEFAULT 'null'")
        .execute(pool).await;
}
```

- [ ] **Step 2: Model field** — in `models.rs` `PlaylistItemWithAsset`, beside `layout`:

```rust
/// A standalone built-in item; `None` for url/asset/layout items.
pub builtin: sqlx::types::Json<Option<crate::builtin::Builtin>>,
```

- [ ] **Step 3: Request structs** — in `handlers.rs`, add to `AddToPlaylistRequest` and `UpdatePlaylistRequest`:

```rust
pub builtin: Option<crate::builtin::Builtin>,
```

Add `builtin` to the `edits_besides_the_playlist` destructure + `|| builtin.is_some()`.

- [ ] **Step 4: Source count + advance** — in `add_to_playlist`, add `payload.builtin.is_some()` to the `sources` array and update the message to `"… eine URL, ein Asset, ein Layout oder ein Built-in."`. After the layout block, add:

```rust
if payload.builtin.is_some()
    && matches!(payload.advance, Some(crate::advance::Advance::Passes { .. })) {
    return bad_request("Ein Built-in läuft nach Zeit, nicht nach Durchläufen.");
}
```

- [ ] **Step 5: INSERT** — add `builtin` to the INSERT column list and bind `sqlx::types::Json(&payload.builtin)` (mirroring how `layout` is bound). Read the current INSERT at `handlers.rs:610` and the layout binding to match exactly.

- [ ] **Step 6: Update handler like-for-like** — a builtin item takes a new `builtin` only; refuse turning a url/asset/layout into a builtin and back, the same shape as the layout rule already enforced around `handlers.rs:763-819`. Add the builtin branch beside the layout one.

- [ ] **Step 7: SELECT** — add `COALESCE(p.builtin, 'null') as builtin` to every `PlaylistItemWithAsset` query (grep `COALESCE(p.layout`): `browser.rs` (2 sites), and any handler SELECT that maps to `PlaylistItemWithAsset`.

- [ ] **Step 8: `playlist_target_url`** — beside the layout branch (`browser.rs:~965`):

```rust
if let Some(b) = item.builtin.0.as_ref() {
    return crate::builtin::resolve(state, Some(&*display_placeholder), b).await;
}
```

Note: `playlist_target_url` is not `async` today and has no display handle. Check its signature: it is `fn playlist_target_url(state: &AppState, item: &PlaylistItemWithAsset) -> String`. `resolve` is async and wants a display. Make `playlist_target_url` `async` and thread the loop's `&display` through its call sites (the item branch and the keep_loaded reconcile both call it) — pass `Some(display.as_ref())` OR, to avoid churn, add `builtin::resolve_blocking` is **not** allowed (no block_on in the loop). Preferred: make `playlist_target_url` async and pass `display`. The call sites are in `browser_loop`, already async.

- [ ] **Step 9: Build + Rust tests**

Run: `cargo test 2>&1 | tail -3`
Expected: ok. Fix any SELECT that now fails to decode (a missing `builtin` column selection is the usual cause).

- [ ] **Step 10: Commit**

```bash
git add src/db.rs src/models.rs src/handlers.rs src/browser.rs
git commit -m "A playlist item can be a built-in"
```

---

### Task 4: Editors — shared config editor, layout picker, item kind

**Files:**
- Create: `web/builtin-editor.js`
- Modify: `web/layout-editor.js`, `web/playlist.html`, `src/web.rs` (`touch`)

**Interfaces:**
- Produces: `BuiltinEditor.create({ builtin, el }) -> { root, read() }` where `read()` returns a `Builtin` object.

- [ ] **Step 1: Write `web/builtin-editor.js`** — a kind dropdown (Uhr/Banner/QR/Countdown) and per-kind fields; `read()` emits the tagged object. Dependency-free. Fields:
  - Clock: `format_24h` (24h checkbox), `show_seconds`, `show_date`, `timezone` (text, leer=Gerät), background/text colour.
  - Banner: `text` (textarea), `mode` (Füllen/Normal), and when Normal: `size` (klein/mittel/groß) + `align` (zentriert/links); colours.
  - Qr: `source` (text/cast), `qr_text` (when text), `label`; colours.
  - Countdown: `target` (datetime-local → ISO), `label`, `done_text`; colours.
  Export `globalThis.BuiltinEditor = { create }`. Follow `layout-editor.js`'s `el`-based style.

- [ ] **Step 2: Load it** — add `<script src="/builtin-editor.js"></script>` to `web/playlist.html` (beside `layout-editor.js`) and to `web/admin.html` only if needed (not needed; overlay uses layout-editor box mode). Playlist only.

- [ ] **Step 3: Layout widget picker** (`layout-editor.js`) — in `renderFields`, the source `kind` select (`Seite (URL)` / `Asset`) gains `Built-in`. When selected, mount a `BuiltinEditor` and in `commit()` set `selected.source = { ...builtinEditor.read() }` (the tagged object *is* the source). `read()` (top-level) already emits `source` verbatim for non-asset/url — adjust the widget serialisation so a builtin source round-trips: emit `w.source` as-is when it has a `kind`.

- [ ] **Step 4: Playlist item kind** (`playlist.html`) — the add-form `addKind` and the card `isLayout` branch gain a `builtin` kind: mount `BuiltinEditor`, and the add/save body sends `builtin: editor.read()`; `buildCard` shows the editor for a builtin item (`item.builtin`).

- [ ] **Step 5: `touch src/web.rs`, build, manual smoke** — start a scratch instance, open `/playlist.html`, add a built-in clock item, confirm it saves (`GET /api/playlist` shows `builtin.kind == "clock"`) and the display shows `/widget.html`. Stop it.

- [ ] **Step 6: Commit**

```bash
git add web/builtin-editor.js web/layout-editor.js web/playlist.html src/web.rs
git commit -m "Edit built-ins: a shared editor in the layout picker and the item kind"
```

---

### Task 5: Browser tests

**Files:**
- Create: `tests/cast/test_builtin.py`

- [ ] **Step 1: Write `test_builtin.py`** (stdlib, real Chrome, patterned on `test_layouts.py`), cases:
  - [190] API: a built-in clock item is stored and read back (`builtin.kind == "clock"`); source-count refuses url+builtin together (`400`); `Passes` advance on a builtin is `400`.
  - [191] a standalone built-in item is shown full-screen as `/widget.html`, and the clock rendered a time (digits on the page) and ticks (value changes after >1 s with seconds on).
  - [192] a built-in **widget** in a layout is shown as a `/widget.html` frame at its grid place.
  - [193] a QR built-in draws real SVG `rect` modules; a banner built-in shows its text.
  Use `cdp` + the display page like `test_layouts`/`test_overlay`. Reuse `a_playlist`/`assign` from `test_webhook`.

- [ ] **Step 2: Run** (stop any local instance first)

Run: `cd tests/cast && python3 test_builtin.py 2>&1 | tail -20`
Expected: `ALL PASSED`.

- [ ] **Step 3: Commit**

```bash
git add tests/cast/test_builtin.py
git commit -m "Test built-in widgets and items"
```

---

### Task 6: Docs and full verification

**Files:**
- Modify: `CLAUDE.md`, `docs/features.md`, `README.md`, the spec status line.

- [ ] **Step 1: CLAUDE.md** — a Built-ins subsection under Layouts: the four kinds; `builtin.rs::resolve` is the only resolver (QR/cast/locale from the same sources as the overlay); `widget.html` is same-origin so it needs none of the overlay defences and no `frames.rs` unlock; item is one of url/asset/layout/builtin, builtin advance is time-only; the payload travels base64 in `?c=`.
- [ ] **Step 2: docs/features.md** — operator description of the four built-ins, as widget and as item.
- [ ] **Step 3: README.md** — note the built-in item type and the `/widget.html` display path in the API overview.
- [ ] **Step 4: Spec status** → `implemented` in `docs/superpowers/specs/2026-09-25-builtin-widgets-design.md`.
- [ ] **Step 5: Full verification**

```bash
cargo test 2>&1 | tail -3
cd tests/cast && for t in test_builtin test_layouts test_overlay test_advance; do echo "== $t"; python3 $t.py 2>&1 | grep -E 'FAILED|ALL PASSED'; done
cd ../.. && cross build --target armv7-unknown-linux-gnueabihf 2>&1 | tail -3
```
Expected: cargo ok, all suites `ALL PASSED`, cross `Finished`.

- [ ] **Step 6: Commit**

```bash
git add CLAUDE.md docs/features.md README.md docs/superpowers/specs/2026-09-25-builtin-widgets-design.md
git commit -m "Document built-in widgets"
```

---

## Self-Review

**Spec coverage:** Builtin type + resolver + widget page (T1); layout widget source (T2); standalone item incl. db/model/handlers/target-url/advance (T3); shared editor + both editors (T4); tests (T5); docs + verification incl. cross build (T6). Caps in T1 `sanitized` + handler; QR/cast/locale in `resolve`. All spec sections covered.

**Placeholder scan:** the widget.html `renderBanner` has one deliberately-noted duplicate `fontSize` line to delete on paste — flagged inline; no other placeholders.

**Type consistency:** `Builtin` (tag `kind`, snake_case) ↔ JSON `{kind: "clock"|"banner"|"qr"|"countdown"}` ↔ widget.html `cfg.kind` switch ↔ `BuiltinEditor.read()`. `Style` flattened (`background_color`, `text_color`) ↔ widget.html `cfg.background_color`/`cfg.text_color`. `resolve(state, display, b)` async, used by T2 (`None`) and T3 (`Some(display)`). `builtin` column/field/JSON consistent across db/model/handlers/SELECT.

**One risk flagged:** `playlist_target_url` becomes `async` (T3 Step 8) — a mechanical change threading `&display` through two call sites in `browser_loop`; the plan says to verify the signature and update both.
