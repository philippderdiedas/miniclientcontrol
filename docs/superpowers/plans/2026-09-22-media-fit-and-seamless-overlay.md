# Media Fit, Controls-Free Video and Seamless Overlay Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a playlist item say how its image or video sits on the screen, play videos without Chromium's control bar, and keep the overlay on screen across navigations instead of blinking at every item change.

**Architecture:** Image and video assets stop being navigated to directly and go through a new page of ours, `web/media_viewer.html`, told its fit and background in the query string — the shape `pdf_viewer.html` already has. The overlay payload is seeded into the script registered with `Page.addScriptToEvaluateOnNewDocument`, so the runtime draws the badge the moment the next document exists; the registration is replaced, never stacked.

**Tech Stack:** Rust (axum, sqlx/SQLite, chromiumoxide 0.9), vanilla HTML/JS, stdlib-only Python end-to-end tests in `tests/cast/`.

**Spec:** `docs/superpowers/specs/2026-09-22-media-fit-and-seamless-overlay-design.md`

## Global Constraints

- `web/` is compiled into the binary via `include_dir!` — **rebuild (`cargo build`) after every change under `web/`** before running any Python test, or the test runs the old page.
- `cargo build` is the gate for Rust; `cargo test` runs the unit tests. No linter.
- Schema changes only in `src/db.rs::run_migrations`, behind a `pragma_table_info` probe. `main.rs` must not create tables.
- Every read of `fit_mode`/`fit_background` is `COALESCE`d: `COALESCE(p.fit_mode, 'contain')`, `COALESCE(p.fit_background, '#000000')`.
- Fit values, exactly: `contain` (default), `cover`, `fill`, `none`, `scroll`. Unknown → `contain`. `scroll` on a video → `contain`.
- Background default `#000000`. Accepted shapes are whatever `settings::is_hex_colour` accepts (`#rgb`, `#rrggbb`, `#rrggbbaa`); anything else on a write is a `400` with `{ "error": "…" }`.
- UI is dependency-free; build nodes with the page's `el()` helper — **never `innerHTML` interpolation**.
- Swallow handler DB errors with `let _ = …` like the neighbouring writes.
- `Display` notifies: `notify_one()`, never `notify_waiters()` (no new ones are added here, but do not change existing ones).
- Git: no `Co-Authored-By` / `Claude-Session` trailers in any commit (user's global rule). Commit as the machine's git user.
- Before any Python test: stop any locally running instance. `test_media.py` (new) uses HTTP `3051`, TLS `3494`, CDP `9252`; `test_overlay.py` uses `3041`/`3484`/`9232`.

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `src/models.rs` | modify | `FitMode` enum, `DEFAULT_FIT_BACKGROUND`, new fields on `PlaylistItemWithAsset` and `OverrideItem` |
| `src/db.rs` | modify | two `ALTER TABLE` probes; migration test |
| `src/settings.rs` | modify | `is_hex_colour` becomes `pub(crate)` |
| `src/handlers.rs` | modify | request fields, validation, writes, `COALESCE` in `get_playlist`, override fields |
| `src/cast/mod.rs` | modify | cast override gets the default fit fields |
| `src/browser.rs` | modify | `asset_target_url`, media routing, playlist query columns, overlay seeding, loop reorder |
| `src/main.rs` | modify | `/media_viewer.html` in `is_display_path` |
| `web/media_viewer.html` | create | the page that draws one image or video |
| `web/overlay.js` | modify | apply `__ovSeed` at document start, top frame only; `seeds`/`seededAt` in `state()` |
| `web/playlist.html` | modify | fit editor on image/video cards |
| `tests/cast/test_media.py` | create | API round trip, viewer in a real Chrome |
| `tests/cast/test_overlay.py` | modify | case `[48]`: the badge survives navigation without a gap; `[48b]` iframes |
| `README.md`, `docs/features.md`, `tests/cast/README.md`, `CLAUDE.md` | modify | document the feature and the new rules |

---

### Task 1: `FitMode`, the two columns, and the row fields

**Files:**
- Modify: `src/models.rs` (after `ScrollMode`'s `Default` impl, ~line 230; `OverrideItem` ~233; `PlaylistItemWithAsset` ~276; tests module ~475)
- Modify: `src/db.rs` (after the `end_date` probe, ~line 103; tests module ~281)
- Modify: `src/handlers.rs:1071` and `src/cast/mod.rs:443` (only to keep the build green — the fields get real values in Task 2)

**Interfaces:**
- Produces: `crate::models::FitMode` (`Contain | Cover | Fill | None | Scroll`, `Default = Contain`, serde lowercase), `FitMode::from_value(&str) -> FitMode`, `FitMode::as_str(self) -> &'static str`, `crate::models::DEFAULT_FIT_BACKGROUND: &str = "#000000"`, `PlaylistItemWithAsset { fit_mode: String, fit_background: String, .. }`, `OverrideItem { fit_mode: FitMode, fit_background: String, .. }`.

- [ ] **Step 1: Write the failing `FitMode` test**

Append inside the existing `#[cfg(test)] mod tests` in `src/models.rs`:

```rust
    #[test]
    fn fit_mode_parses_its_five_names_and_falls_back_on_anything_else() {
        use super::FitMode;
        assert_eq!(FitMode::from_value("contain"), FitMode::Contain);
        assert_eq!(FitMode::from_value("cover"), FitMode::Cover);
        assert_eq!(FitMode::from_value("fill"), FitMode::Fill);
        assert_eq!(FitMode::from_value("none"), FitMode::None);
        assert_eq!(FitMode::from_value("scroll"), FitMode::Scroll);
        assert_eq!(FitMode::from_value(" COVER "), FitMode::Cover);
        for junk in ["", "stretch", "object-fit: cover", "Scroll;"] {
            assert_eq!(FitMode::from_value(junk), FitMode::Contain, "{junk:?}");
        }
        for mode in [FitMode::Contain, FitMode::Cover, FitMode::Fill, FitMode::None, FitMode::Scroll] {
            assert_eq!(FitMode::from_value(mode.as_str()), mode);
        }
    }
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test fit_mode_parses`
Expected: compile error, `cannot find type FitMode`.

- [ ] **Step 3: Add `FitMode` and the constant**

In `src/models.rs`, directly after `impl Default for ScrollMode { … }`:

```rust
/// How an image or video asset sits on the screen. Stored per playlist item by
/// name; `web/media_viewer.html` is what turns it into a layout.
///
/// Four of these are CSS `object-fit`. `Scroll` is not: it draws the image at
/// full width and natural height and lets the document scroll, which is what
/// gives the scroll runtime something to move.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum FitMode {
    #[default]
    Contain,
    Cover,
    Fill,
    None,
    Scroll,
}

impl FitMode {
    /// Total on purpose. The value ends up on a screen nobody is standing in
    /// front of, so an unknown one falls back rather than failing -- the same
    /// rule the overlay applies to an unknown corner.
    pub fn from_value(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "cover" => Self::Cover,
            "fill" => Self::Fill,
            "none" => Self::None,
            "scroll" => Self::Scroll,
            _ => Self::Contain,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Contain => "contain",
            Self::Cover => "cover",
            Self::Fill => "fill",
            Self::None => "none",
            Self::Scroll => "scroll",
        }
    }
}

/// What fills the bars around a contained asset when its item names nothing.
pub const DEFAULT_FIT_BACKGROUND: &str = "#000000";

fn default_fit_background() -> String {
    DEFAULT_FIT_BACKGROUND.to_string()
}
```

- [ ] **Step 4: Add the fields**

In `OverrideItem`, after `scroll_config`:

```rust
    /// How an image or video override sits on the screen. Defaulted rather than
    /// optional, so two overrides compare equal exactly when they would draw the
    /// same thing -- `run_override_loop` relies on that comparison to not
    /// re-navigate a live cast.
    #[serde(default)]
    pub fit_mode: FitMode,
    #[serde(default = "default_fit_background")]
    pub fit_background: String,
```

In `PlaylistItemWithAsset`, after `overlay_config`:

```rust
    /// `FitMode` by name. A `String` rather than the enum, which would need a
    /// `sqlx::Type` impl to decode; parsed with `FitMode::from_value` where it is
    /// used, and that parse is total anyway.
    #[sqlx(default)]
    pub fit_mode: String,

    /// What fills the bars around a contained asset.
    #[sqlx(default)]
    pub fit_background: String,
```

- [ ] **Step 5: Keep the two `OverrideItem` constructors compiling**

`src/cast/mod.rs:443` — add after `scroll_config: scroll,`:

```rust
            fit_mode: crate::models::FitMode::default(),
            fit_background: crate::models::DEFAULT_FIT_BACKGROUND.to_string(),
```

`src/handlers.rs:1071` (`set_override_of`) — add after `scroll_config: …,` (Task 2 replaces these with the request's values):

```rust
        fit_mode: crate::models::FitMode::default(),
        fit_background: crate::models::DEFAULT_FIT_BACKGROUND.to_string(),
```

- [ ] **Step 6: Run the `FitMode` test**

Run: `cargo test fit_mode_parses`
Expected: `test models::tests::fit_mode_parses_its_five_names_and_falls_back_on_anything_else ... ok`

- [ ] **Step 7: Write the failing migration test**

Append inside `#[cfg(test)] mod tests` in `src/db.rs`:

```rust
    #[tokio::test]
    async fn an_item_from_before_fit_existed_gets_the_defaults() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:?cache=shared").await.unwrap();
        // The shape a device had before the fit columns.
        sqlx::query(
            "CREATE TABLE playlist_items (
                id INTEGER PRIMARY KEY AUTOINCREMENT, asset_id INTEGER, url TEXT,
                play_order INTEGER NOT NULL, duration INTEGER, is_enabled BOOLEAN DEFAULT 1,
                start_date TEXT, end_date TEXT, keep_loaded BOOLEAN DEFAULT 0,
                scroll_config TEXT DEFAULT '{\"type\":\"None\",\"options\":null}')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO playlist_items (url, play_order) VALUES ('https://a.test', 1)")
            .execute(&pool)
            .await
            .unwrap();

        run_migrations(&pool).await.unwrap();
        run_migrations(&pool).await.unwrap();

        let (fit, background): (String, String) =
            sqlx::query_as("SELECT fit_mode, fit_background FROM playlist_items")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(fit, "contain");
        assert_eq!(background, "#000000");
    }
```

- [ ] **Step 8: Run it to see it fail**

Run: `cargo test an_item_from_before_fit_existed`
Expected: FAIL, `no such column: fit_mode`.

- [ ] **Step 9: Add the two probes**

In `src/db.rs::run_migrations`, directly after the `has_end_date` block:

```rust
    // How an image or video asset sits on the screen, and what fills the bars
    // around it. Both carry a default so the rows an older binary wrote get one
    // too -- and every read COALESCEs anyway, because a NULL in a String field
    // fails the whole playlist query and blanks the screen.
    let has_fit_mode: bool = sqlx::query("SELECT count(*) FROM pragma_table_info('playlist_items') WHERE name='fit_mode'")
        .fetch_one(pool)
        .await
        .map(|row| row.get::<i32, _>(0) > 0)
        .unwrap_or(false);

    if !has_fit_mode {
        let _ = sqlx::query("ALTER TABLE playlist_items ADD COLUMN fit_mode TEXT DEFAULT 'contain'")
            .execute(pool)
            .await;
    }

    let has_fit_background: bool = sqlx::query("SELECT count(*) FROM pragma_table_info('playlist_items') WHERE name='fit_background'")
        .fetch_one(pool)
        .await
        .map(|row| row.get::<i32, _>(0) > 0)
        .unwrap_or(false);

    if !has_fit_background {
        let _ = sqlx::query("ALTER TABLE playlist_items ADD COLUMN fit_background TEXT DEFAULT '#000000'")
            .execute(pool)
            .await;
    }
```

- [ ] **Step 10: Run all Rust tests**

Run: `cargo test`
Expected: all pass, including the two new ones. (The in-memory DB tests share `cache=shared`; if the new one collides with a sibling that also creates `playlist_items`, rerun with `cargo test -- --test-threads=1` and note it — the existing tests have the same shape.)

- [ ] **Step 11: Commit**

```bash
git add src/models.rs src/db.rs src/cast/mod.rs src/handlers.rs
git commit -m "Store how an item's image or video sits on the screen"
```

---

### Task 2: The API reads and writes the fit

**Files:**
- Modify: `src/settings.rs:254` (`is_hex_colour` → `pub(crate)`)
- Modify: `src/handlers.rs` — `AddToPlaylistRequest` (~35), `UpdatePlaylistRequest` (~52) and `edits_besides_the_playlist` (~90), `SetOverrideRequest` (~162), `get_playlist` query (~379), `add_to_playlist` (~404), `update_playlist_item` (~459), `set_override_of` (~1040)
- Modify: `src/browser.rs:251` (the loop's playlist query)
- Create: `tests/cast/test_media.py` (API half)

**Interfaces:**
- Consumes: `FitMode`, `DEFAULT_FIT_BACKGROUND` (Task 1).
- Produces: JSON fields `fit_mode` (string) and `fit_background` (string) on `POST /api/playlist`, `PUT /api/playlist/{id}`, `GET /api/playlist`, `POST /api/override` (and the per-display override route, which shares `set_override_of`). `tests/cast/test_media.py` with helpers `upload(name, data, mimetype, port)` and `PNG` bytes, used by Task 3.

- [ ] **Step 1: Write the failing end-to-end test (API half)**

Create `tests/cast/test_media.py`:

```python
"""How an image or video sits on the screen, and a video with no control bar.

The first half is plain HTTP: the fit is stored per playlist item, falls back
when it is nonsense, and a background that is not a colour is refused. The
second half drives a real Chrome through the real browser_loop, because the
whole point of the feature is what the display draws -- and a stored value
proves nothing about that.
"""
import asyncio, base64, json, os, shutil, subprocess, sys, time, urllib.request, uuid
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cdp
from test_cast import Server, check, failures, http

SP = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.join(SP, "..", "..", "target", "debug", "miniclientcontrol")
CHROME = "/usr/bin/google-chrome-stable"
HTTP, TLS, CDP = 3051, 3494, 9252

# One transparent pixel. Enough for every case here: `scroll` draws it at full
# width, so a 1x1 image becomes 1280x1280 on a 1280x720 window and the document
# really is taller than the screen.
PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")

procs = []


def upload(name, data, mimetype, port=None):
    """POST one file to /api/assets and return the new asset's id."""
    port = port or 3021
    boundary = "----mcc" + uuid.uuid4().hex
    body = (f"--{boundary}\r\n"
            f"Content-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n"
            f"Content-Type: {mimetype}\r\n\r\n").encode() + data + f"\r\n--{boundary}--\r\n".encode()
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/api/assets", data=body, method="POST",
        headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    with urllib.request.urlopen(req, timeout=10) as res:
        json.load(res)
    rows = http("GET", "/api/assets", port=port)[1]
    return next(row["id"] for row in rows if row["filename"] == name)


def a_playlist(port=None):
    """A playlist, assigned to every declared display -- see test_overlay.py."""
    rows = http("GET", "/api/playlists", port=port)[1] or []
    playlist_id = (rows[0]["id"] if rows
                   else http("POST", "/api/playlists", {"name": "Test"}, port=port)[1]["id"])
    for row in http("GET", "/api/displays", port=port)[1] or []:
        if row.get("playlist_id") != playlist_id:
            http("PUT", f"/api/displays/{row['name']}", {"playlist_id": playlist_id}, port=port)
    return playlist_id


def item(item_id, port=None):
    return next(row for row in http("GET", "/api/playlist", port=port)[1] if row["id"] == item_id)


def api_flow():
    print("\n[100] a new item sits contained on black")
    with Server():
        asset = upload("poster.png", PNG, "image/png")
        playlist = a_playlist()
        status, _ = http("POST", "/api/playlist", {"asset_id": asset, "playlist_id": playlist})
        check("the item is created", status == 201, status)
        row = http("GET", "/api/playlist")[1][-1]
        check("its fit defaults to contain", row["fit_mode"] == "contain", row)
        check("and its background to black", row["fit_background"] == "#000000", row)

        print("\n[101] the fit and the background are stored per item")
        status, _ = http("PUT", f"/api/playlist/{row['id']}",
                         {"fit_mode": "cover", "fit_background": "#00ff00"})
        check("the edit saves", status == 200, status)
        saved = item(row["id"])
        check("cover is kept", saved["fit_mode"] == "cover", saved)
        check("and so is the colour", saved["fit_background"] == "#00ff00", saved)

        status, _ = http("POST", "/api/playlist",
                         {"asset_id": asset, "playlist_id": playlist,
                          "fit_mode": "scroll", "fit_background": "#123"})
        created = http("GET", "/api/playlist")[1][-1]
        check("a new item can carry both from the start",
              status == 201 and created["fit_mode"] == "scroll"
              and created["fit_background"] == "#123", created)

        print("\n[102] nonsense falls back, a colour that is not one is refused")
        http("PUT", f"/api/playlist/{row['id']}", {"fit_mode": "stretch-it"})
        check("an unknown fit becomes contain rather than an error",
              item(row["id"])["fit_mode"] == "contain", item(row["id"]))

        status, body = http("PUT", f"/api/playlist/{row['id']}",
                            {"fit_background": "red; display:none", "duration": 42})
        check("a background that is not a hex colour is a 400",
              status == 400 and "error" in (body or {}), (status, body))
        after = item(row["id"])
        check("and nothing else in that request was written",
              after["duration"] != 42 and after["fit_background"] == "#00ff00", after)

        status, body = http("POST", "/api/playlist",
                            {"asset_id": asset, "playlist_id": playlist, "fit_background": "blue"})
        check("the same refusal on create", status == 400 and "error" in (body or {}), (status, body))

        other = http("POST", "/api/playlists", {"name": "Zweite"})[1]["id"]
        status, _ = http("PUT", f"/api/playlist/{row['id']}",
                         {"playlist_id": other, "fit_mode": "cover"})
        check("a move carrying a fit edit is refused like any other combined edit",
              status == 400, status)


if __name__ == "__main__":
    try:
        api_flow()
    finally:
        for p in procs:
            p.terminate()
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
```

Check `test_cast.http`'s signature before relying on it: `grep -n "^def http" -A 12 tests/cast/test_cast.py`. It is called as `http(method, path, body=None, port=None)` everywhere in `test_overlay.py` and returns `(status, parsed_json_or_None)`; `Server()` listens on `3021`, which is also `http`'s default port — hence `upload`'s default.

- [ ] **Step 2: Run it to see it fail**

Run: `cargo build && cd tests/cast && python3 test_media.py; cd ../..`
Expected: `[100]` fails with a `KeyError: 'fit_mode'` (the GET does not return the column yet).

- [ ] **Step 3: Make `is_hex_colour` reachable**

`src/settings.rs:254`:

```rust
pub(crate) fn is_hex_colour(value: &str) -> bool {
```

- [ ] **Step 4: Add the request fields**

In `AddToPlaylistRequest`, after `overlay`:

```rust
    /// How an image or video sits on the screen, by name. A string rather than
    /// `FitMode` so an unknown name falls back instead of failing the whole
    /// request at deserialisation.
    pub fit_mode: Option<String>,
    /// What fills the bars around a contained asset. Must be a hex colour.
    pub fit_background: Option<String>,
```

In `UpdatePlaylistRequest`, after `overlay` — the same two fields with the same comments.

In `SetOverrideRequest`, after `scroll_config`:

```rust
    pub fit_mode: Option<String>,
    pub fit_background: Option<String>,
```

In `edits_besides_the_playlist`, add `fit_mode,` and `fit_background,` to the destructuring pattern and `|| fit_mode.is_some() || fit_background.is_some()` to the expression.

- [ ] **Step 5: Add the validation helper**

In `src/handlers.rs`, directly after `fn bad_request`:

```rust
/// A background colour as the operator sent it, or the refusal that tells them.
///
/// Refused rather than clamped, unlike the fit itself: this is typed by someone
/// looking at the page, who can be told, and a colour that silently became
/// black would read as the setting not working. The same check as the
/// overlay's colours, so the two colour fields on one card accept the same
/// thing.
fn checked_fit_background(raw: Option<String>) -> Result<Option<String>, axum::response::Response> {
    let Some(value) = raw else {
        return Ok(None);
    };
    let value = value.trim().to_string();
    if crate::settings::is_hex_colour(&value) {
        Ok(Some(value))
    } else {
        Err(bad_request(
            "Hintergrundfarbe muss eine Hex-Farbe sein, zum Beispiel #000000.",
        ))
    }
}
```

Add `FitMode` to the models import at the top of `src/handlers.rs`:

```rust
use crate::models::{AppState, Asset, Display, FitMode, OverrideItem, PlaylistItemWithAsset, ScrollMode};
```

- [ ] **Step 6: Read both columns in `get_playlist`**

In the `get_playlist` query, after the `overlay_config` line:

```sql
            COALESCE(p.fit_mode, 'contain') as fit_mode,
            COALESCE(p.fit_background, '#000000') as fit_background,
```

And the same two lines in `src/browser.rs:251`'s query, after `COALESCE(p.scroll_config, …) as scroll_config,`.

- [ ] **Step 7: Write both columns in `add_to_playlist`**

Change the signature's return type from `impl IntoResponse` to `axum::response::Response`, and each `return StatusCode::BAD_REQUEST;` / `return StatusCode::INTERNAL_SERVER_ERROR;` / final `StatusCode::CREATED` to `….into_response()` — a validation error is a JSON body, and both arms must have one type.

Directly after the "neither source" check, before the `MAX(play_order)` query:

```rust
    // Checked before anything is read or written, so a refusal leaves no trace.
    let fit_background = match checked_fit_background(payload.fit_background) {
        Ok(value) => value.unwrap_or_else(|| crate::models::DEFAULT_FIT_BACKGROUND.to_string()),
        Err(response) => return response,
    };
    let fit_mode = payload
        .fit_mode
        .as_deref()
        .map(FitMode::from_value)
        .unwrap_or_default();
```

Extend the `INSERT` to two more columns and two more binds:

```rust
        "INSERT INTO playlist_items (asset_id, url, play_order, duration, is_enabled, keep_loaded, start_date, end_date, scroll_config, overlay_config, playlist_id, fit_mode, fit_background) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
```

```rust
    .bind(payload.playlist_id)
    .bind(fit_mode.as_str())
    .bind(fit_background)
```

- [ ] **Step 8: Write both columns in `update_playlist_item`**

Directly after the `if let Some(target) = payload.playlist_id { … }` move block and **before** the source-edit block (the colour must be refused before the URL or asset is written, or a `400` would sit on top of a half-applied edit):

```rust
    let fit_background = match checked_fit_background(payload.fit_background.clone()) {
        Ok(value) => value,
        Err(response) => return response,
    };
```

After the `scroll_config` write:

```rust
    if let Some(raw) = payload.fit_mode.as_deref() {
        let _ = sqlx::query("UPDATE playlist_items SET fit_mode = ? WHERE id = ?")
            .bind(FitMode::from_value(raw).as_str())
            .bind(id)
            .execute(&state.pool)
            .await;
    }
    if let Some(value) = fit_background {
        let _ = sqlx::query("UPDATE playlist_items SET fit_background = ? WHERE id = ?")
            .bind(value)
            .bind(id)
            .execute(&state.pool)
            .await;
    }
```

(`notify_playlist_changed` at the end already covers it: the loop re-reads the playlist, and the item on screen picks the new fit up on its next navigation.)

- [ ] **Step 9: Carry both into the override**

In `set_override_of`, directly after the "neither asset nor url" check:

```rust
    let fit_background = match checked_fit_background(payload.fit_background) {
        Ok(value) => value.unwrap_or_else(|| crate::models::DEFAULT_FIT_BACKGROUND.to_string()),
        Err(response) => return response,
    };
```

and replace the two placeholder lines from Task 1 Step 5 with:

```rust
        fit_mode: payload.fit_mode.as_deref().map(FitMode::from_value).unwrap_or_default(),
        fit_background,
```

- [ ] **Step 10: Build and run the API half**

Run: `cargo build && cd tests/cast && python3 test_media.py; cd ../..`
Expected: `[100]`–`[102]` all `ok`, `ALL PASSED`.

- [ ] **Step 11: Commit**

```bash
git add src/settings.rs src/handlers.rs src/browser.rs tests/cast/test_media.py
git commit -m "Accept a fit and a background on playlist items and overrides"
```

---

### Task 3: The media viewer

**Files:**
- Create: `web/media_viewer.html`
- Modify: `src/browser.rs` — `playlist_target_url` / `override_target_url` (~794–825), new `asset_target_url` + `media_kind`, new `#[cfg(test)] mod tests` at the end of the file; import `FitMode`
- Modify: `src/main.rs:51` (`is_display_path`) and a new `#[cfg(test)]` module at the end of `src/main.rs`
- Modify: `tests/cast/test_media.py` (browser half)

**Interfaces:**
- Consumes: `FitMode::from_value`, `FitMode::as_str`, `PlaylistItemWithAsset.fit_mode/fit_background`, `OverrideItem.fit_mode/fit_background`.
- Produces: `fn asset_target_url(port: u16, local_path: &str, mimetype: Option<&str>, scroll: &ScrollMode, fit: FitMode, background: &str) -> String` in `browser.rs`; the URL shape `http://127.0.0.1:<port>/media_viewer.html?asset=<enc>&kind=image|video&fit=<name>&bg=<enc>`.

- [ ] **Step 1: Write the failing routing tests**

Append to the end of `src/browser.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::FitMode;

    #[test]
    fn an_image_goes_through_the_media_viewer_with_its_fit() {
        let url = asset_target_url(3000, "a b.png", Some("image/png"), &ScrollMode::None,
                                   FitMode::Cover, "#00ff00");
        assert_eq!(
            url,
            "http://127.0.0.1:3000/media_viewer.html?asset=a%20b.png&kind=image&fit=cover&bg=%2300ff00"
        );
    }

    #[test]
    fn a_video_goes_through_the_media_viewer_as_a_video() {
        let url = asset_target_url(3000, "clip.mp4", Some("video/mp4"), &ScrollMode::None,
                                   FitMode::Contain, "#000000");
        assert!(url.contains("/media_viewer.html?"), "{url}");
        assert!(url.contains("kind=video"), "{url}");
    }

    #[test]
    fn a_pdf_keeps_its_own_viewer() {
        let url = asset_target_url(3000, "doc.pdf", Some("application/pdf"), &ScrollMode::None,
                                   FitMode::Cover, "#000000");
        assert!(url.contains("/pdf_viewer.html?"), "{url}");
    }

    #[test]
    fn anything_else_is_navigated_to_directly() {
        let url = asset_target_url(3000, "page.html", Some("text/html"), &ScrollMode::None,
                                   FitMode::Cover, "#000000");
        assert!(url.starts_with("http://127.0.0.1:3000/uploads/page.html"), "{url}");
        let unknown = asset_target_url(3000, "blob", None, &ScrollMode::None,
                                       FitMode::Contain, "#000000");
        assert!(unknown.contains("/uploads/blob"), "{unknown}");
    }

    #[test]
    fn the_media_viewer_is_not_exempt_from_scrolling() {
        // `scroll` depends on the scroll runtime driving this page. Only the PDF
        // viewer scrolls itself and is skipped by `start_scrolling`.
        let url = asset_target_url(3000, "tall.png", Some("image/png"), &ScrollMode::None,
                                   FitMode::Scroll, "#000000");
        assert!(!is_internal_pdf_viewer_url(&url), "{url}");
    }
}
```

And to the end of `src/main.rs`:

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn the_media_viewer_is_reachable_by_the_display_browser() {
        // The display browser is driven over CDP and presents no credentials; a
        // page it loads that is not here is a 401 on every screen the moment
        // basic auth is configured.
        assert!(super::is_display_path("/media_viewer.html"));
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test media_viewer`
Expected: compile error, `cannot find function asset_target_url`.

- [ ] **Step 3: Route assets through one function**

In `src/browser.rs`, extend the models import:

```rust
use crate::models::{AppState, Display, FitMode, OverrideItem, PlaylistItemWithAsset, ScrollMode};
```

Replace the asset branch of `playlist_target_url`:

```rust
    if let Some(path) = &item.local_path {
        let full_path = state.args.assets_dir.join(path);
        if !full_path.exists() {
            return no_content_url(state.args.port);
        }
        return asset_target_url(
            state.args.port,
            path,
            item.mimetype.as_deref(),
            &item.scroll_config.0,
            FitMode::from_value(&item.fit_mode),
            &item.fit_background,
        );
    }
```

Replace the asset branch of `override_target_url`:

```rust
    if let Some(path) = &item.local_path {
        return asset_target_url(
            state.args.port,
            path,
            item.mimetype.as_deref(),
            &item.scroll_config,
            item.fit_mode,
            &item.fit_background,
        );
    }
```

Add, directly after `override_target_url`:

```rust
/// Where an asset is shown, given what it is and how its item wants it to sit.
///
/// Images and videos go through `media_viewer.html` rather than to `/uploads/`
/// directly: navigated to directly, Chromium builds its own image or media
/// document, whose layout no setting of ours can reach and whose video comes
/// with a control bar nothing turns off. PDFs keep their own viewer; anything
/// else is navigated to as before. The existence check stays with the caller,
/// so this is testable without an `AppState`.
fn asset_target_url(
    port: u16,
    local_path: &str,
    mimetype: Option<&str>,
    scroll: &ScrollMode,
    fit: FitMode,
    background: &str,
) -> String {
    if is_internal_pdf_mimetype(mimetype) {
        return internal_pdf_viewer_url(port, local_path, scroll);
    }
    if let Some(kind) = media_kind(mimetype) {
        return format!(
            "http://127.0.0.1:{}/media_viewer.html?asset={}&kind={}&fit={}&bg={}",
            port,
            encode(local_path),
            kind,
            fit.as_str(),
            encode(background)
        );
    }
    format!("http://127.0.0.1:{}/uploads/{}#toolbar=0&navpanes=0&view=FitH", port, local_path)
}

fn media_kind(mimetype: Option<&str>) -> Option<&'static str> {
    let m = mimetype.unwrap_or_default().to_ascii_lowercase();
    if m.starts_with("image/") {
        Some("image")
    } else if m.starts_with("video/") {
        Some("video")
    } else {
        None
    }
}
```

- [ ] **Step 4: Let the display browser load it without credentials**

`src/main.rs::is_display_path`, add beside `"/pdf_viewer.html"`:

```rust
        "/pdf_viewer.html"
            | "/media_viewer.html"
```

- [ ] **Step 5: Run the Rust tests**

Run: `cargo test`
Expected: all pass, including the six new ones.

- [ ] **Step 6: Create the page**

Create `web/media_viewer.html`:

```html
<!doctype html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <title>Media</title>
  <style>
    html, body { margin: 0; padding: 0; background: #000; }
    /* Every mode but `scroll` fills the window exactly and never scrolls. */
    body.fixed { width: 100vw; height: 100vh; overflow: hidden; }
    body.fixed #media { display: block; width: 100vw; height: 100vh; }
    /* `scroll`: full width, natural height. The document scrolls, and the scroll
       runtime the controller injects is what moves it -- which is why this page,
       unlike the PDF viewer, is not exempt from start_scrolling. */
    body.scroll #media { display: block; width: 100%; height: auto; }
  </style>
</head>
<body>
  <script>
    // Chromium's own image and media documents are what this page replaces:
    // navigated to directly, an asset gets a layout no setting can reach and a
    // video gets a control bar nothing turns off.
    (() => {
      const params = new URLSearchParams(location.search);
      const asset = params.get('asset');
      const kind = params.get('kind') === 'video' ? 'video' : 'image';

      // An unknown fit falls back rather than drawing nothing, and `scroll` on a
      // video has no overflow worth scrolling -- both land on contain.
      const FITS = ['contain', 'cover', 'fill', 'none', 'scroll'];
      let fit = FITS.includes(params.get('fit')) ? params.get('fit') : 'contain';
      if (kind === 'video' && fit === 'scroll') fit = 'contain';

      // Validated here as well as on write: a row from an older binary, or one
      // edited by hand, must not paint an undefined colour.
      const bg = params.get('bg') || '';
      if (/^#([0-9a-f]{3}|[0-9a-f]{6}|[0-9a-f]{8})$/i.test(bg)) {
        document.documentElement.style.background = bg;
        document.body.style.background = bg;
      }

      if (!asset) return;

      const media = document.createElement(kind);
      media.id = 'media';
      media.src = '/uploads/' + encodeURIComponent(asset);

      if (fit === 'scroll') {
        document.body.className = 'scroll';
      } else {
        document.body.className = 'fixed';
        media.style.objectFit = fit;
      }

      if (kind === 'video') {
        // No `controls`: the bar is the complaint this page exists for. `loop`
        // unconditionally -- the item's duration ends it either way, and without
        // it a short video parks on its last frame for the rest of the item.
        media.controls = false;
        media.autoplay = true;
        media.loop = true;
        media.playsInline = true;
        // Unmuted first, muted only if the browser refuses. The controller starts
        // Chromium with --autoplay-policy=no-user-gesture-required, but a browser
        // started outside it may not have the flag, and a silent video is better
        // than a black screen.
        media.addEventListener('canplay', () => {
          const started = media.play();
          if (started && started.catch) {
            started.catch((err) => {
              if (err && err.name === 'NotAllowedError') {
                media.muted = true;
                media.play().catch(() => {});
              }
            });
          }
        }, { once: true });
      }

      document.body.append(media);
    })();
  </script>
</body>
</html>
```

- [ ] **Step 7: Add the browser half of the test**

In `tests/cast/test_media.py`, add below `api_flow` (and above the `__main__` block):

```python
def spawn(cmd):
    p = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    procs.append(p)
    return p


def wait_for(fn, timeout=30, interval=0.3):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            value = fn()
            if value:
                return value
        except Exception:
            pass
        time.sleep(interval)
    return None


MEDIA = """(() => {
  const m = document.getElementById('media');
  if (!m) return JSON.stringify({media: false, href: location.href});
  const root = document.scrollingElement || document.documentElement;
  return JSON.stringify({
    media: true,
    tag: m.tagName.toLowerCase(),
    href: location.href,
    objectFit: getComputedStyle(m).objectFit,
    controls: m.controls === true || m.hasAttribute('controls'),
    loop: m.loop === true,
    autoplay: m.autoplay === true,
    background: getComputedStyle(document.body).backgroundColor,
    scrollable: root.scrollHeight > window.innerHeight + 2,
  });
})()"""


async def on_media(page, predicate, timeout=40):
    """Poll the display until the media viewer shows something `predicate` likes."""
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        try:
            last = json.loads(await page.eval(MEDIA))
            if last.get("media") and predicate(last):
                return last
        except Exception:
            pass  # mid-navigation: the context was just destroyed
        await asyncio.sleep(0.3)
    return last


async def browser_flow():
    print("\n[103] an image item is drawn by our page with the fit it asked for")
    if not os.path.exists(CHROME):
        print("  SKIP  no Chrome at " + CHROME)
        return

    shutil.rmtree(f"{SP}/media-display", ignore_errors=True)
    spawn([CHROME, "--headless=new", f"--remote-debugging-port={CDP}",
           f"--user-data-dir={SP}/media-display", "--no-first-run", "--no-sandbox",
           "--disable-gpu", "--window-size=1280,720",
           "--autoplay-policy=no-user-gesture-required", "about:blank"])
    check("display chrome up", wait_for(lambda: cdp.targets(CDP)) is not None)

    for leftover in ("m.db", "m.db-wal", "m.db-shm"):
        try:
            os.remove(os.path.join(SP, leftover))
        except FileNotFoundError:
            pass
    spawn([BIN, "--port", str(HTTP), "--cast-tls-port", str(TLS),
           "--database-path", f"{SP}/m.db", "--assets-dir", f"{SP}/assets",
           "--cast-cert-path", f"{SP}/cert.pem", "--no-launch-browser",
           "--managed-cert", "off", "--cdp-url", f"http://127.0.0.1:{CDP}"])
    check("controller up", wait_for(
        lambda: http("GET", "/api/cast/info", port=HTTP)[0] == 200) is not None)

    image = upload("fit.png", PNG, "image/png", port=HTTP)
    video = upload("clip.mp4", b"\x00\x00\x00\x18ftypmp42", "video/mp4", port=HTTP)
    playlist = a_playlist(port=HTTP)
    http("POST", "/api/playlist",
         {"asset_id": image, "playlist_id": playlist, "duration": 3,
          "fit_mode": "cover", "fit_background": "#00ff00"}, port=HTTP)
    image_item = http("GET", "/api/playlist", port=HTTP)[1][-1]["id"]

    ws_url, _ = cdp.page_ws(CDP)
    async with cdp.Session(ws_url) as page:
        shown = await on_media(page, lambda m: m["tag"] == "img")
        check("the display is on the media viewer, not /uploads/",
              shown and "/media_viewer.html?" in shown["href"], shown)
        check("with the item's fit", shown and shown["objectFit"] == "cover", shown)
        check("and its background colour",
              shown and shown["background"] == "rgb(0, 255, 0)", shown)
        check("and nothing to scroll", shown and shown["scrollable"] is False, shown)

        print("\n[104] `scroll` draws it full width and gives the document height")
        http("PUT", f"/api/playlist/{image_item}", {"fit_mode": "scroll"}, port=HTTP)
        # The edit lands on the item's next navigation; the item is three
        # seconds long and loops, so that is within a few seconds.
        # Waits for the height as well as the URL: until the image has loaded
        # it is zero pixels tall and the page is not scrollable yet.
        tall = await on_media(page, lambda m: "fit=scroll" in m["href"] and m["scrollable"])
        check("the page is taller than the screen",
              tall and tall["scrollable"] is True, tall)

        print("\n[105] a video has no control bar, and loops")
        status, _ = http("POST", "/api/override", {"asset_id": video, "fit_mode": "fill"}, port=HTTP)
        check("the override is up", status == 200, status)
        clip = await on_media(page, lambda m: m["tag"] == "video")
        check("it is a <video> on our page", clip and clip["tag"] == "video", clip)
        check("without controls", clip and clip["controls"] is False, clip)
        check("looping and autoplaying", clip and clip["loop"] and clip["autoplay"], clip)
        check("and the override's own fit", clip and clip["objectFit"] == "fill", clip)
        http("DELETE", "/api/override", port=HTTP)
```

and change the `__main__` block to run both and clean up the profile:

```python
if __name__ == "__main__":
    try:
        api_flow()
        asyncio.run(browser_flow())
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            try:
                p.wait(timeout=10)
            except Exception:
                p.kill()
        shutil.rmtree(f"{SP}/media-display", ignore_errors=True)
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
```

(`clip.mp4` is twelve bytes of an `ftyp` box and will not decode. It does not have to: the element, its attributes and its computed style are what is asserted, and they exist regardless.)

- [ ] **Step 8: Build and run**

Run: `cargo build && cd tests/cast && python3 test_media.py; cd ../..`
Expected: `[100]`–`[105]` all `ok`, `ALL PASSED`.

- [ ] **Step 9: Commit**

```bash
git add web/media_viewer.html src/browser.rs src/main.rs tests/cast/test_media.py
git commit -m "Draw image and video assets on a page of our own"
```

---

### Task 4: The fit editor on the card

**Files:**
- Modify: `web/playlist.html` — new `fitEditor` after `scrollEditor` (~line 330); `buildCard` (~782–880)

**Interfaces:**
- Consumes: `item.mimetype`, `item.fit_mode`, `item.fit_background` from `GET /api/playlist`; `PUT /api/playlist/{id}` accepting `fit_mode`/`fit_background`.
- Produces: nothing other tasks use.

- [ ] **Step 1: Add the editor**

In `web/playlist.html`, directly after the closing `}` of `function scrollEditor`:

```js
    // --- fit editor -----------------------------------------------------------
    // Image and video items only. A URL or a PDF has no page of ours whose layout
    // this could reach, so offering it there would be a setting that does nothing.
    const FIT_CHOICES = [
      ['contain', 'Einpassen – ganz sichtbar, mit Rand'],
      ['cover', 'Füllen – beschneidet, was übersteht'],
      ['fill', 'Strecken – verzerrt'],
      ['none', 'Original – 1:1, mittig'],
      ['scroll', 'Scrollen – volle Breite, für hohe Bilder'],
    ];

    function fitEditor(fitMode, background) {
      const mode = el('select', {},
        ...FIT_CHOICES.map(([value, text]) => el('option', { value, text })));
      mode.value = FIT_CHOICES.some(([value]) => value === fitMode) ? fitMode : 'contain';

      // A colour input only takes #rrggbb; anything else the server accepts
      // (#rgb, #rrggbbaa) shows as black here and is written back as black on save.
      const bg = el('input', {
        type: 'color',
        value: /^#[0-9a-f]{6}$/i.test(background || '') ? background : '#000000',
      });

      // The cost of `scroll` being a fit value rather than a rule: two fields that
      // must agree. The one direction that silently does nothing gets a hint; the
      // other (fit `scroll`, no scrolling) is a legitimate fit-to-width.
      const hint = el('p', {
        class: 'muted',
        hidden: true,
        text: 'Bildlauf ist eingestellt, aber die Anpassung ist nicht „Scrollen“ – '
          + 'das Bild passt ganz auf den Schirm, es gibt nichts zu scrollen.',
      });

      const root = el('fieldset', {},
        el('legend', { text: 'Darstellung' }),
        el('div', { class: 'grid' }, field('Anpassung', mode), field('Hintergrund', bg)),
        hint);

      return {
        root,
        onChange(handler) { mode.addEventListener('change', handler); },
        hintFor(scrollType) { hint.hidden = !(scrollType !== 'None' && mode.value !== 'scroll'); },
        read() { return { fit_mode: mode.value, fit_background: bg.value }; },
      };
    }
```

- [ ] **Step 2: Put it on image and video cards**

In `buildCard`, replace

```js
      const scroll = scrollEditor(item.scroll_config);
      card.append(scroll.root);
```

with

```js
      const scroll = scrollEditor(item.scroll_config);
      card.append(scroll.root);

      const isMedia = isAsset && /^(image|video)\//i.test(item.mimetype || '');
      const fit = isMedia ? fitEditor(item.fit_mode, item.fit_background) : null;
      if (fit) {
        card.append(fit.root);
        const syncHint = () => fit.hintFor(scroll.read().type);
        scroll.onInput(syncHint);
        fit.onChange(syncHint);
        syncHint();
      }
```

In the save handler, directly after `const payload = { … };`:

```js
        if (fit) Object.assign(payload, fit.read());
```

(Nothing to add for `dirty`: the card already marks itself dirty on any `input`/`change` that bubbles up from a descendant, and the fieldset is one.)

- [ ] **Step 3: Rebuild and check by hand**

Run: `cargo build`, then start a dev instance (`./target/debug/miniclientcontrol --managed-cert off`), open `/playlist.html`, upload an image, add it to a playlist. Verify:
- the card shows *Darstellung* with *Anpassung* and *Hintergrund*; a URL item's card does not;
- changing either marks the card *ungespeichert*, and it survives the 2-second poll;
- setting Scroll to *Step* with Anpassung *Einpassen* shows the hint; switching Anpassung to *Scrollen* hides it;
- *Speichern* persists both (`curl -s localhost:3000/api/playlist | grep fit_`).

**Stop the dev instance afterwards** — the Python suites need the ports.

- [ ] **Step 4: Commit**

```bash
git add web/playlist.html
git commit -m "Offer the fit and background on image and video cards"
```

---

### Task 5: The overlay is seeded into the next document

**Files:**
- Modify: `web/overlay.js` — `state()` (~475) and the end of the IIFE (~491)
- Modify: `src/browser.rs` — imports (line 6), the post-connect registration (~143), the idle branch (~318–333), the item loop (~394–490 and ~524–540), `run_override_loop` (~664–720) and its call site (~203), overlay helpers (~1427–1469)
- Modify: `tests/cast/test_overlay.py` — new cases `[48]` and `[48b]` at the end of `browser_flow`

**Interfaces:**
- Consumes: `crate::settings::overlay_payload(state, display, item) -> serde_json::Value` (unchanged), `crate::db::load_item_overlay` (unchanged).
- Produces: `struct OverlaySeed`, `async fn seed_overlay_runtime(page: &Page, seed: &mut OverlaySeed, payload: &Value) -> Result<(), CdpError>`, `async fn apply_overlay_payload(page: &Page, payload: &Value) -> Result<(), Box<dyn std::error::Error + Send + Sync>>`; in the page, `globalThis.__ovSeed`, `globalThis.__ovSeeds`, and `__ov.state().seeds` / `.seededAt`.

- [ ] **Step 1: Write the failing end-to-end cases**

In `tests/cast/test_overlay.py`, at the end of `browser_flow` (after `[47]`'s `check("every box is removed from the page", …)`, still inside the function, dedented to the function's body level):

```python
    print("\n[48] the badge is already there when the next item's page appears")
    # Two items taking turns, so the loop navigates every two seconds. The
    # controller's own apply comes after the attached-target drain (700 ms) and
    # the readiness wait (at least ~1 s of network idle), so a box that is up
    # within a few hundred milliseconds of the document starting can only have
    # come from the seed.
    put({"enabled": True, "text": "nahtlos", "position": "top-left"}, port=HTTP)
    for row in http("GET", "/api/playlist", port=HTTP)[1]:
        http("PUT", f"/api/playlist/{row['id']}", {"enabled": False}, port=HTTP)
    playlist = a_playlist(port=HTTP)
    for screen in ("a", "b"):
        http("POST", "/api/playlist",
             {"url": f"http://127.0.0.1:{HTTP}/empty_playlist.html?screen=seed-{screen}",
              "duration": 2, "playlist_id": playlist}, port=HTTP)

    seen = {}
    live_ws, _ = cdp.page_ws(9232)
    async with cdp.Session(live_ws) as live:
        deadline = time.time() + 40
        while time.time() < deadline and len(seen) < 4:
            try:
                raw = await live.eval("""(() => JSON.stringify({
                    href: location.href,
                    origin: performance.timeOrigin,
                    st: globalThis.__ov ? globalThis.__ov.state() : null,
                }))()""")
                row = json.loads(raw)
            except Exception:
                await asyncio.sleep(0.05)
                continue  # the document went away under the evaluate
            if "seed-" in row["href"] and row["origin"] not in seen and row["st"]:
                seen[row["origin"]] = row["st"]
            await asyncio.sleep(0.05)

        check("several navigations were observed", len(seen) >= 3, len(seen))
        states = list(seen.values())
        check("every one of them was seeded",
              all(s.get("seededAt") is not None for s in states), states)
        check("within the first half second of the document -- before the controller's apply",
              all((s.get("seededAt") or 1e9) < 500 for s in states), states)
        check("and nothing threw on the way in: the runtime is installed",
              all(s.get("installed") for s in states), states)
        check("one registration per document, not one per item change",
              all(s.get("seeds") == 1 for s in states), states)

        print("\n[48b] an iframe does not get a badge of its own")
        framed = upload_html(
            "framed.html",
            "<!doctype html><html><body><p>aussen</p>"
            "<iframe srcdoc='<p>innen</p>' style='width:400px;height:200px'></iframe>"
            "</body></html>", port=HTTP)
        http("POST", "/api/override", {"asset_id": framed}, port=HTTP)
        counts = None
        for _ in range(60):
            try:
                counts = json.loads(await live.eval("""(() => {
                    const f = document.querySelector('iframe');
                    const inner = f && f.contentDocument;
                    return JSON.stringify({
                      framed: !!f,
                      top: document.querySelectorAll('[id^="__mcc_overlay"]').length,
                      inner: inner ? inner.querySelectorAll('[id^="__mcc_overlay"]').length : null,
                    });
                })()"""))
                if counts["framed"] and counts["top"] == 1 and counts["inner"] is not None:
                    break
            except Exception:
                pass
            await asyncio.sleep(0.3)
        check("the page has its one badge", counts and counts["top"] == 1, counts)
        check("and the iframe inside it has none", counts and counts["inner"] == 0, counts)
        http("DELETE", "/api/override", port=HTTP)
```

and add this helper near the top of the file, after `assign`:

```python
def upload_html(name, html, port=None):
    """Upload a page as an asset. Served by the controller itself, so it is
    same-origin with the display and its iframe can be inspected."""
    import uuid
    boundary = "----mcc" + uuid.uuid4().hex
    body = (f"--{boundary}\r\n"
            f"Content-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n"
            "Content-Type: text/html\r\n\r\n").encode() + html.encode() \
        + f"\r\n--{boundary}--\r\n".encode()
    req = urllib.request.Request(
        f"http://127.0.0.1:{port or CAST_HTTP}/api/assets", data=body, method="POST",
        headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    with urllib.request.urlopen(req, timeout=10) as res:
        json.load(res)
    return next(row["id"] for row in http("GET", "/api/assets", port=port)[1]
                if row["filename"] == name)
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo build && cd tests/cast && python3 test_overlay.py; cd ../..`
Expected: `[48]` "every one of them was seeded" FAILS (`seededAt` is absent from `state()`). `[40]`–`[47]` still pass.

- [ ] **Step 3: Teach the runtime to start from a seed**

In `web/overlay.js`, inside `state()`'s returned object, after `suspended,`:

```js
        // How many seeded registrations ran in this document, and when the seed
        // was applied (ms since the document started), or null. The first is how
        // a leaked registration shows up -- each would run again on every
        // navigation -- and the second is how "the badge was there before the
        // controller got to it" is told apart from "the controller was quick".
        seeds: Number(globalThis.__ovSeeds) || 0,
        seededAt,
```

Directly after `let observer = null;` near the top:

```js
  let seededAt = null;
```

After the closing `};` of `globalThis.__ov = { … };` and before the IIFE's closing `})();`:

```js
  // The controller registers this runtime for the *next* document with the
  // payload it should start from, so the badge is drawn before that document's
  // first paint instead of after the readiness waits -- which is what made it
  // blink at every item change. Same shape as `__ovSuspend`: a value that
  // exists before this runtime does.
  //
  // Top frame only: the registered script runs in every frame, and an iframe
  // drawing its own copy is a second clock in the middle of a dashboard.
  //
  // And not before there is a root element. The registration runs on document
  // creation, which comes before the parser's `<html>`, and both `appendChild`
  // and `MutationObserver.observe` throw on a null root -- which would take the
  // runtime down before `__ov` could be used by anybody.
  if (globalThis.__ovSeed && window.top === window) {
    const seed = globalThis.__ovSeed;
    const applySeed = () => {
      // The controller may have applied a newer payload in the meantime.
      if (payload) return;
      globalThis.__ov.apply(seed);
      seededAt = Math.round(performance.now());
    };
    if (document.documentElement) {
      applySeed();
    } else {
      const rootWatch = new MutationObserver(() => {
        if (!document.documentElement) return;
        rootWatch.disconnect();
        applySeed();
      });
      rootWatch.observe(document, { childList: true });
    }
  }
```

- [ ] **Step 4: The registration carries the payload and is replaced, not stacked**

In `src/browser.rs`, extend the page import:

```rust
use chromiumoxide::cdp::browser_protocol::page::{AddScriptToEvaluateOnNewDocumentParams, EnableParams as PageEnableParams, NavigateParams, ReloadParams, RemoveScriptToEvaluateOnNewDocumentParams, ScriptIdentifier};
```

Replace `apply_overlay` (the whole function, ~1434–1469) with:

```rust
/// The overlay registration the control page carries for its next document.
///
/// Held per CDP connection: a registration belongs to the session that made
/// it, so a reconnect starts from nothing.
#[derive(Default)]
struct OverlaySeed {
    id: Option<ScriptIdentifier>,
    payload: Option<Value>,
}

/// Register the overlay runtime for `page`'s next document, carrying `payload`,
/// so the badge is drawn the moment that document exists rather than after the
/// readiness waits -- the gap that made it blink at every item change. It also
/// covers a page that navigates itself (a dashboard on a meta refresh), which
/// the controller never re-applies to.
///
/// Replaces the registration `seed` holds rather than adding beside it: every
/// registration runs on every navigation for as long as the session lives, so
/// one per item change is a leak that also runs N copies of the runtime.
/// Unchanged payloads are skipped, so the idle branch calling this every five
/// seconds costs nothing.
async fn seed_overlay_runtime(
    page: &Page,
    seed: &mut OverlaySeed,
    payload: &Value,
) -> Result<(), CdpError> {
    if seed.id.is_some() && seed.payload.as_ref() == Some(payload) {
        return Ok(());
    }
    if let Some(previous) = seed.id.take() {
        // Not fatal on its own: a registration that is already gone is exactly
        // the state we want. A lost connection fails the add below as well.
        if let Err(e) = page
            .execute(RemoveScriptToEvaluateOnNewDocumentParams::new(previous))
            .await
        {
            debug!("Could not remove the previous overlay registration: {}", e);
        }
    }
    let script = format!(
        "globalThis.__ovSeeds = (globalThis.__ovSeeds || 0) + 1;\nglobalThis.__ovSeed = {};\n{}",
        serde_json::to_string(payload).unwrap_or_else(|_| "null".to_string()),
        overlay_runtime_script()
    );
    let added = page
        .execute(AddScriptToEvaluateOnNewDocumentParams::new(script))
        .await?;
    seed.id = Some(added.result.identifier);
    seed.payload = Some(payload.clone());
    Ok(())
}

/// Put the operator's badge on the page on screen, or take it off again.
///
/// Evaluated after navigation as well as seeded before it, because a strict
/// CSP can block the registered copy -- on such a page this is the only thing
/// that draws the badge, and it blinks there exactly as before. With the seed
/// in place the payload is identical and re-applying it is invisible.
///
/// No-ops when the runtime is missing, the same way `apply_scroll_settings`
/// does: a page that blocked the injection must not stall the playlist. The
/// payload comes from `settings::overlay_payload`, so the display and the
/// operator's preview cannot render different things.
async fn apply_overlay_payload(
    page: &Page,
    payload: &Value,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = page.evaluate(overlay_runtime_script()).await;
    let has_api: bool = page
        .evaluate("(() => !!globalThis.__ov)()")
        .await?
        .into_value()
        .unwrap_or(false);
    if !has_api {
        debug!("Overlay runtime missing on this page (CSP?), leaving it alone");
        return Ok(());
    }

    let script = format!(
        "(() => globalThis.__ov.apply({}))()",
        serde_json::to_string(payload).unwrap_or_else(|_| "null".to_string())
    );
    let shown: bool = page.evaluate(script).await?.into_value().unwrap_or(false);
    debug!("Overlay applied (visible: {})", shown);
    Ok(())
}
```

Keep `register_overlay_runtime_script` as it is: `keep_loaded` tabs still use it (they are shown with `bring_to_front`, not navigated, so their document and its badge persist between showings and need no seed).

- [ ] **Step 5: Seed at connect**

Replace (~143):

```rust
        if let Err(e) = register_overlay_runtime_script(&page).await {
            debug!("Failed to register overlay runtime script: {}", e);
        }
```

with:

```rust
        // The payload a page with no item-specific layer gets. Every navigation
        // below re-seeds with its own before it navigates.
        let mut overlay_seed = OverlaySeed::default();
        {
            let payload = crate::settings::overlay_payload(&state, &display, None).await;
            if let Err(e) = seed_overlay_runtime(&page, &mut overlay_seed, &payload).await {
                debug!("Failed to register overlay runtime script: {}", e);
            }
        }
```

- [ ] **Step 6: The idle branch seeds before it navigates**

In the `if playlist.is_empty()` branch, at its start (before `let empty_url = …`):

```rust
                // Seeded before the navigation, so the idle page is born with
                // the badge. The idle screen is a page like any other, and the
                // one most likely to be up when somebody sets a notice.
                let overlay = crate::settings::overlay_payload(&state, &display, None).await;
                if let Err(e) = seed_overlay_runtime(&page, &mut overlay_seed, &overlay).await {
                    debug!("Failed to seed the overlay for the idle page: {}", e);
                    if is_connection_lost(&e) {
                        reconnect_needed = true;
                        break;
                    }
                }
```

and replace the later

```rust
                // The idle screen is a page like any other. It is also the one
                // most likely to be up when somebody sets a notice.
                if let Err(e) = apply_overlay(&state, &display, &page, None).await {
                    debug!("Failed to apply overlay on the idle page: {}", e);
                }
```

with

```rust
                if let Err(e) = apply_overlay_payload(&page, &overlay).await {
                    debug!("Failed to apply overlay on the idle page: {}", e);
                }
```

- [ ] **Step 7: The item loop builds the payload first**

In the item loop, directly before `if do_navigate {` (~446), insert:

```rust
                // Built before the navigation rather than after it: the control
                // page is seeded with it so the next document is born with the
                // badge. Read fresh rather than taken from the playlist
                // snapshot: that snapshot is read once per inner-loop pass and
                // can be a whole item duration old, so an overlay edited while
                // the previous item was up would otherwise appear one full
                // rotation late.
                let item_overlay = crate::db::load_item_overlay(&state.pool, item.id).await;
                let overlay =
                    crate::settings::overlay_payload(&state, &display, item_overlay.as_ref()).await;
```

and inside `if do_navigate {`, before `navigate_page`:

```rust
                    if let Err(e) = seed_overlay_runtime(&dynamic_page, &mut overlay_seed, &overlay).await {
                        debug!("Failed to seed the overlay: {}", e);
                        if is_connection_lost(&e) {
                            reconnect_needed = true;
                            break;
                        }
                    }
```

Replace the later block (~483–494)

```rust
                // Read fresh rather than taken from the playlist snapshot: …
                let item_overlay = crate::db::load_item_overlay(&state.pool, item.id).await;
                if let Err(e) =
                    apply_overlay(&state, &display, &active_page, item_overlay.as_ref()).await
                {
```

with

```rust
                if let Err(e) = apply_overlay_payload(&active_page, &overlay).await {
```

(keeping the body of the `if let Err` — the `error!` and the `is_connection_lost` break — as it is).

- [ ] **Step 8: An overlay edit mid-item re-seeds as well as re-applies**

In the per-item `select!`'s `overlay_signal` branch, replace

```rust
                            let fresh =
                                crate::db::load_item_overlay(&state.pool, item.id).await;
                            if let Err(e) =
                                apply_overlay(&state, &display, &active_page, fresh.as_ref())
                                    .await
                            {
```

with

```rust
                            let fresh =
                                crate::db::load_item_overlay(&state.pool, item.id).await;
                            let payload = crate::settings::overlay_payload(
                                &state, &display, fresh.as_ref()).await;
                            // Re-seeded too, when the item is on the control
                            // page: the page may navigate itself before the loop
                            // does, and it must come back with this badge, not
                            // the one from before the edit.
                            if do_navigate {
                                if let Err(e) = seed_overlay_runtime(
                                    &dynamic_page, &mut overlay_seed, &payload).await
                                {
                                    debug!("Failed to re-seed the overlay: {}", e);
                                }
                            }
                            if let Err(e) = apply_overlay_payload(&active_page, &payload).await
                            {
```

- [ ] **Step 9: The override loop does the same**

Change `run_override_loop`'s signature to take the seed, after `page: &Page,`:

```rust
    overlay_seed: &mut OverlaySeed,
```

and its call site (~203) to:

```rust
                if let Err(e) = run_override_loop(&state, &display, &browser, &mut attached_events, &page, &mut overlay_seed, override_item).await {
```

Inside `run_override_loop`, before `navigate_page(page, &target_url).await?;`:

```rust
        let overlay = crate::settings::overlay_payload(state, display, None).await;
        seed_overlay_runtime(page, overlay_seed, &overlay).await?;
```

Replace

```rust
        if let Err(e) = apply_overlay(state, display, page, None).await {
            error!("Failed to apply overlay on the override page: {}", e);
        }
```

with

```rust
        if let Err(e) = apply_overlay_payload(page, &overlay).await {
            error!("Failed to apply overlay on the override page: {}", e);
        }
```

and in the inner `select!`'s `overlay_signal` branch replace

```rust
                    if let Err(e) = apply_overlay(state, display, page, None).await {
                        error!("Failed to re-apply overlay: {}", e);
                    }
```

with

```rust
                    let payload = crate::settings::overlay_payload(state, display, None).await;
                    if let Err(e) = seed_overlay_runtime(page, overlay_seed, &payload).await {
                        debug!("Failed to re-seed the overlay: {}", e);
                    }
                    // Touches nothing but the badge, so a live RTCPeerConnection
                    // survives it.
                    if let Err(e) = apply_overlay_payload(page, &payload).await {
                        error!("Failed to re-apply overlay: {}", e);
                    }
```

- [ ] **Step 10: Build**

Run: `cargo build`
Expected: builds with no errors and no `unused` warning for `apply_overlay` (it is gone; `grep -n "apply_overlay(" src/browser.rs` must print nothing).

- [ ] **Step 11: Run the overlay suite**

Run: `cd tests/cast && python3 test_overlay.py; cd ../..`
Expected: `[40]`–`[48b]` all `ok`, `ALL PASSED`. In particular `[46a]` (CSP page still styled) and `[46b]`/`[46c]` (suspend, including the pre-seeded `__ovSuspend`) must still pass: a seed is applied through `__ov.apply`, which respects `suspended`.

- [ ] **Step 12: Run the other browser suites that touch the loop**

Run each on its own, never two of `test_display.py`/`test_webhook.py`/`test_castscreens.py` at once:

```bash
cd tests/cast
python3 test_browser.py
python3 test_display.py
python3 test_castscreens.py 86
python3 test_webhook.py
cd ../..
```

Expected: `ALL PASSED` for each. `[86]` is the one that asserts a cast on one screen leaves another's overlay untouched, which is the per-display seeding.

- [ ] **Step 13: Commit**

```bash
git add web/overlay.js src/browser.rs tests/cast/test_overlay.py
git commit -m "Seed the overlay into the next document so it stops blinking"
```

---

### Task 6: Documentation

**Files:**
- Modify: `README.md` (playlist API ~220, override API ~238, "Scroll Configuration" ~501)
- Modify: `docs/features.md` ("Items" ~26, "Assets" ~94, "Overlay" ~115)
- Modify: `tests/cast/README.md` (the list and the Chrome paragraph)
- Modify: `CLAUDE.md` ("Stop any locally running instance" paragraph; "Injected runtimes"; "HTTP: three audiences")
- Modify: `docs/superpowers/specs/2026-09-22-media-fit-and-seamless-overlay-design.md` (`**Status:** implemented`)

- [ ] **Step 1: README**

Under the playlist item fields (wherever `scroll_config` is listed for `POST`/`PUT /api/playlist`), add:

```markdown
  - optional `fit_mode`: `contain` (default), `cover`, `fill`, `none` or `scroll`
    — how an image or video asset sits on the screen. An unknown value is stored
    as `contain`. Ignored for URL and PDF items.
  - optional `fit_background`: hex colour behind a contained asset (default
    `#000000`); anything that is not a hex colour is a `400`
```

Under `POST /api/override`, after `optional scroll_config`:

```markdown
  - optional `fit_mode` and `fit_background`, as for a playlist item
```

After the "Scroll Configuration" paragraph about PDFs:

```markdown
Images and videos are rendered through `/media_viewer.html`, which applies the
item's `fit_mode` and `fit_background` and plays videos without controls, on a
loop, unmuted. `fit_mode: "scroll"` draws an image at full width so the scroll
modes above have something to scroll; with any other fit the whole image is on
screen and there is nothing to scroll.
```

- [ ] **Step 2: features.md**

Replace the "Assets" section's first paragraph with:

```markdown
Uploads land in `--assets-dir` and are served from `/uploads/`. Images, videos
and PDFs are what the feature exists for. A PDF is rendered by a bundled pdf.js
viewer rather than handed to Chromium's own, so scrolling can be driven; an image
or a video is drawn by a small page of ours for the same kind of reason —
Chromium's own image and video documents have a layout nothing can change and a
video control bar nothing turns off.

Each playlist item that plays an image or a video says **how it sits on the
screen**: *Einpassen* (whole picture, with bars), *Füllen* (fills, crops),
*Strecken* (fills, distorts), *Original* (1:1) or *Scrollen* (full width, for a
tall image the scroll modes then move). The colour of the bars is the item's too.
A video loops until its item's duration is up.
```

In the "Overlay" section, add a paragraph:

```markdown
The badge does not disappear between items. The controller hands the next page
its overlay before navigating to it, so the badge is there from the page's first
frame — and stays through a page that reloads itself. A page with a strict
content-security policy can refuse that, and on such a page the badge comes back
a moment after the page does, as it always has.
```

- [ ] **Step 3: tests/cast/README.md**

Add to the command list, after `test_overlay.py`:

```
python3 test_media.py       # image fit and a video without controls (needs Chrome)
```

Change "Four of them need a real Chrome — `test_browser.py`, `test_overlay.py`, `test_webhook.py` and `test_display.py`" to "Five of them need a real Chrome — `test_browser.py`, `test_overlay.py`, `test_media.py`, `test_webhook.py` and `test_display.py`", and add after that paragraph:

```markdown
`test_media.py` runs its own display Chrome on CDP port 9252 and its own
controller on 3051. The API cases (`[100]`-`[102]`) need no browser.
```

- [ ] **Step 4: CLAUDE.md**

In "Stop any locally running instance before the Python suite", change the list of browser-launching tests and ports to include `test_media.py` on `9252`:

```markdown
`test_browser.py`/`test_overlay.py`/`test_media.py`/`test_webhook.py`/`test_display.py`/`test_castscreens.py`
launch their own Chrome on `9222`+`9223`/`9232`/`9252`/`9242`/`9242`+`9243`/`9242`+`9243`.
```

In "## HTTP: three audiences, two predicates", after the `is_display_path` bullet, add:

```markdown
- **Every page the display browser loads must be in `is_display_path`** —
  `pdf_viewer.html`, `media_viewer.html`, `empty_playlist.html`. A page missing
  from it works on a device with no credentials and is a `401` on every screen
  the moment basic auth is switched on.
```

In "## Injected runtimes", after the first paragraph, add:

```markdown
**The overlay's registration carries its payload** (`globalThis.__ovSeed`), so
the badge is drawn when the next document is created instead of after the
readiness waits — that gap was the blink at every item change. Three rules:

- **Seed before every navigation of the control page**, and on every overlay
  change. `seed_overlay_runtime` skips an unchanged payload, so calling it too
  often is free; calling it too rarely leaves a self-reloading page with a stale
  badge that nothing corrects.
- **Replace the registration, never add beside it.** Each one runs on every
  navigation for the life of the session. `__ov.state().seeds` counts them, and
  case `[48]` of `tests/cast/test_overlay.py` asserts it is 1.
- **The seed applies in the top frame only, and only once a root element
  exists.** The registered script runs in every frame, and on document creation,
  before `<html>`.

`keep_loaded` tabs are not seeded: they are brought to front, not navigated, so
their document and its badge persist between showings. The after-navigation
`apply_overlay_payload` stays — it is the CSP fallback.
```

- [ ] **Step 5: Mark the spec implemented**

In `docs/superpowers/specs/2026-09-22-media-fit-and-seamless-overlay-design.md`, change `**Status:** designed` to `**Status:** implemented`.

- [ ] **Step 6: Commit**

```bash
git add README.md docs/features.md tests/cast/README.md CLAUDE.md docs/superpowers/specs/2026-09-22-media-fit-and-seamless-overlay-design.md
git commit -m "Document item fit, the media viewer and the seeded overlay"
```
