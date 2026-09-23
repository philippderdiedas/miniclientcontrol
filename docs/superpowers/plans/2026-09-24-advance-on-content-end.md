# Advance on Content End Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A playlist item moves on after a time or after its content has run through N times, replacing the per-item `duration`.

**Architecture:** A new `src/advance.rs` holds the `Advance` enum, its validation and the pure decision the loop makes. The three page runtimes (scroll runtime, PDF viewer, media viewer) publish one interface, `globalThis.__advance` with `state()` and `reset(count)`, built from one counter factory in `web/autoscroll.js`. `browser.rs` polls it every 500 ms for a `Passes` item and moves on when the count is reached or the content stalls.

**Tech Stack:** Rust (axum 0.8, sqlx/SQLite, chromiumoxide), vanilla JS, stdlib Python tests driving headless Chrome.

Spec: `docs/superpowers/specs/2026-09-24-advance-on-content-end-design.md`.

## Global Constraints

- JSON shape: `{"on":"time","seconds":N}` and `{"on":"passes","count":N}`.
- `seconds` clamped to `1..=604800` (the existing `MIN_DURATION_SECS`/`MAX_DURATION_SECS`), `count` to `1..=100`.
- `Passes` is valid only for a video asset or a scroll mode other than `None`; otherwise `400` with a German message.
- `duration` in `POST`/`PUT /api/playlist…` is a `422` (`deny_unknown_fields`).
- `--advance-stall-timeout` seconds, default `120`, flag only (no runtime setting).
- `MIN_PASS` 3000 ms for scrolled content; a page that fits the screen needs `max(MIN_PASS, top_delay + return_delay)` per pass; a video has no minimum.
- Poll interval 500 ms.
- Schema only in `db::run_migrations`; `web/` is compiled in (`touch src/web.rs` for a new file under `web/`).
- `notify_one()`, never `notify_waiters()`.
- UI: `createElement`/`textContent` only, no `innerHTML` interpolation.
- Commits: no Claude co-author or session trailers.
- Stop the local instance on port 3000 before running Python suites that need free ports (see CLAUDE.md).

---

### Task 1: `src/advance.rs` — the enum, its check, the loop's verdict

**Files:**
- Create: `src/advance.rs`
- Modify: `src/main.rs` (add `mod advance;`), `src/models.rs` (flag on `Args`)

**Interfaces:**
- Produces:
  - `pub enum Advance { Time { seconds: u32 }, Passes { count: u16 } }` (serde `tag = "on"`, `rename_all = "snake_case"`), `Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize`
  - `impl Advance { pub fn clamped(self) -> Self; pub fn default_for(asset_seconds: Option<i64>) -> Self }`
  - `pub fn check(advance: &Advance, mimetype: Option<&str>, scroll: &ScrollMode) -> Result<(), &'static str>`
  - `#[derive(Deserialize)] pub struct RuntimeState { pub passes: u32, pub idle_ms: u64 }`
  - `pub enum Verdict { Wait, Done, Stalled(&'static str) }`
  - `pub fn verdict(state: Option<&RuntimeState>, count: u16, missing_for: Duration, stall: Duration) -> Verdict`
  - `pub const POLL: Duration` (500 ms)
  - `Args::advance_stall_timeout: u64`

- [ ] **Step 1: Write the module with its tests**

```rust
//! When a playlist item moves on: after a time, or after its content has run
//! through a number of times.
//!
//! What one pass *is* follows from the content and is not stored: a video
//! played to its end; anything scrolled reached the bottom (a paged PDF, its
//! last page). Storing the kind beside the count would allow "last page" on a
//! video -- a setting that silently does nothing.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::models::ScrollMode;

const MIN_SECONDS: u32 = 1;
const MAX_SECONDS: u32 = 7 * 24 * 60 * 60;
const MIN_COUNT: u16 = 1;
const MAX_COUNT: u16 = 100;

/// How often the loop asks the page how far it is.
pub const POLL: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum Advance {
    Time { seconds: u32 },
    Passes { count: u16 },
}

impl Advance {
    /// Clamped rather than refused, like every number that reaches the screen:
    /// zero seconds spins the loop, and an absurd count is a stuck screen.
    pub fn clamped(self) -> Self {
        match self {
            Advance::Time { seconds } => Advance::Time { seconds: seconds.clamp(MIN_SECONDS, MAX_SECONDS) },
            Advance::Passes { count } => Advance::Passes { count: count.clamp(MIN_COUNT, MAX_COUNT) },
        }
    }

    /// What an item gets when it names none: its asset's length if it has
    /// one (a video's is measured at upload), else ten seconds -- exactly what
    /// the loop used to fall back to.
    pub fn default_for(asset_seconds: Option<i64>) -> Self {
        let seconds = asset_seconds.unwrap_or(10).clamp(MIN_SECONDS as i64, MAX_SECONDS as i64) as u32;
        Advance::Time { seconds }
    }
}

/// Whether `advance` can work for this content. `Passes` needs something that
/// ends: a video, or a scroll mode that moves.
pub fn check(advance: &Advance, mimetype: Option<&str>, scroll: &ScrollMode) -> Result<(), &'static str> {
    let Advance::Passes { .. } = advance else {
        return Ok(());
    };
    let video = mimetype.is_some_and(|m| m.to_ascii_lowercase().starts_with("video/"));
    if video || !matches!(scroll, ScrollMode::None) {
        Ok(())
    } else {
        Err("Durchläufe brauchen etwas, das endet: ein Video oder einen Scroll-Modus.")
    }
}

/// What the page reports through `globalThis.__advance.state()`.
#[derive(Debug, Deserialize)]
pub struct RuntimeState {
    pub passes: u32,
    /// How long the content has not moved on schedule, in the page's clock.
    pub idle_ms: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Wait,
    Done,
    Stalled(&'static str),
}

/// The loop's decision for a `Passes` item on one poll. `missing_for` is how
/// long the page has had no runtime at all; `stall` is `--advance-stall-timeout`.
pub fn verdict(state: Option<&RuntimeState>, count: u16, missing_for: Duration, stall: Duration) -> Verdict {
    match state {
        None if missing_for >= stall => Verdict::Stalled("no advance runtime on the page"),
        None => Verdict::Wait,
        Some(s) if s.passes >= u32::from(count) => Verdict::Done,
        Some(s) if Duration::from_millis(s.idle_ms) >= stall => Verdict::Stalled("the content stopped moving"),
        Some(_) => Verdict::Wait,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ScrollOptions;
    use serde_json::json;

    #[test]
    fn the_json_shape_is_tagged_by_on() {
        assert_eq!(serde_json::to_value(Advance::Time { seconds: 30 }).unwrap(), json!({"on": "time", "seconds": 30}));
        assert_eq!(serde_json::to_value(Advance::Passes { count: 2 }).unwrap(), json!({"on": "passes", "count": 2}));
        let parsed: Advance = serde_json::from_value(json!({"on": "passes", "count": 3})).unwrap();
        assert_eq!(parsed, Advance::Passes { count: 3 });
        assert!(serde_json::from_value::<Advance>(json!({"on": "never"})).is_err());
    }

    #[test]
    fn numbers_are_clamped() {
        assert_eq!(Advance::Time { seconds: 0 }.clamped(), Advance::Time { seconds: 1 });
        assert_eq!(Advance::Time { seconds: u32::MAX }.clamped(), Advance::Time { seconds: MAX_SECONDS });
        assert_eq!(Advance::Passes { count: 0 }.clamped(), Advance::Passes { count: 1 });
        assert_eq!(Advance::Passes { count: 5000 }.clamped(), Advance::Passes { count: 100 });
    }

    #[test]
    fn the_default_is_the_assets_length_or_ten_seconds() {
        assert_eq!(Advance::default_for(Some(42)), Advance::Time { seconds: 42 });
        assert_eq!(Advance::default_for(None), Advance::Time { seconds: 10 });
        assert_eq!(Advance::default_for(Some(-5)), Advance::Time { seconds: 1 });
    }

    #[test]
    fn passes_need_something_that_ends() {
        let passes = Advance::Passes { count: 2 };
        let scrolling = ScrollMode::Continuous(ScrollOptions::default());
        assert!(check(&passes, Some("video/mp4"), &ScrollMode::None).is_ok());
        assert!(check(&passes, Some("application/pdf"), &scrolling).is_ok());
        assert!(check(&passes, None, &scrolling).is_ok());
        assert!(check(&passes, None, &ScrollMode::None).is_err());
        assert!(check(&passes, Some("image/png"), &ScrollMode::None).is_err());
        assert!(check(&Advance::Time { seconds: 5 }, None, &ScrollMode::None).is_ok());
    }

    #[test]
    fn the_verdict() {
        let stall = Duration::from_secs(120);
        let state = |passes, idle_ms| RuntimeState { passes, idle_ms };
        assert_eq!(verdict(Some(&state(1, 0)), 2, Duration::ZERO, stall), Verdict::Wait);
        assert_eq!(verdict(Some(&state(2, 0)), 2, Duration::ZERO, stall), Verdict::Done);
        assert_eq!(verdict(Some(&state(0, 120_000)), 2, Duration::ZERO, stall),
                   Verdict::Stalled("the content stopped moving"));
        assert_eq!(verdict(None, 2, Duration::from_secs(5), stall), Verdict::Wait);
        assert_eq!(verdict(None, 2, stall, stall), Verdict::Stalled("no advance runtime on the page"));
        // A count reached wins over a stall reported in the same poll.
        assert_eq!(verdict(Some(&state(2, 500_000)), 2, Duration::ZERO, stall), Verdict::Done);
    }
}
```

- [ ] **Step 2: Wire the module and the flag**

In `src/main.rs`, beside the other `mod` lines: `mod advance;`

In `src/models.rs`, inside `pub struct Args`, after `port`:

```rust
    /// How long a "passes" item may show content that has stopped moving --
    /// a stuck video, a page whose runtime never arrived -- before the loop
    /// moves on anyway. Fault handling, not a content choice, so a flag only.
    #[arg(long, env, default_value_t = 120)]
    pub advance_stall_timeout: u64,
```

- [ ] **Step 3: Run the tests**

Run: `cargo test advance`
Expected: 5 tests pass. (`ScrollOptions` must be `pub` with `Default`; it is.)

- [ ] **Step 4: Commit**

```bash
git add src/advance.rs src/main.rs src/models.rs
git commit -m "Add the advance model, its check and the loop's verdict"
```

---

### Task 2: Storage and API — `advance` replaces `duration`

**Files:**
- Modify: `src/db.rs` (CREATE TABLE, migration, migration test), `src/models.rs` (`PlaylistItemWithAsset`), `src/handlers.rs` (structs, add/update/asset handlers, get_playlist query, the destructure test), `src/browser.rs` (the loop's SELECT only — the loop logic is Task 4), `src/webhook/mod.rs`, `src/webhook/api.rs`
- Test: `tests/cast/test_advance.py` (create, API half), and migrate `duration` in `tests/cast/test_users.py`, `test_display.py`, `test_webhook.py`, `test_overlay.py`, `test_media.py`

**Interfaces:**
- Consumes: `crate::advance::{Advance, check}`
- Produces: `PlaylistItemWithAsset::advance: sqlx::types::Json<Advance>` (no `duration`, no `asset_duration`); `Event::ItemChanged { .., advance: Advance }`

- [ ] **Step 1: Failing Python API test**

Create `tests/cast/test_advance.py` (API half; Task 6 adds the browser half):

```python
"""When an item moves on: after a time, or after its content ran N times.

The API half is plain HTTP. The browser half drives a real Chrome through the
real browser_loop, because a stored count proves nothing about the screen.
"""
import asyncio, base64, json, os, shutil, subprocess, sys, time, urllib.request, uuid
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cdp
from test_cast import Server, check, failures, http

SP = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.join(SP, "..", "..", "target", "debug", "miniclientcontrol")
CHROME = "/usr/bin/google-chrome-stable"
HTTP, TLS, CDP = 3061, 3504, 9262
PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
procs = []


def upload(name, data, mimetype, port=None):
    port = port or 3021
    boundary = "----mcc" + uuid.uuid4().hex
    body = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n"
            f"Content-Type: {mimetype}\r\n\r\n").encode() + data + f"\r\n--{boundary}--\r\n".encode()
    req = urllib.request.Request(f"http://127.0.0.1:{port}/api/assets", data=body, method="POST",
                                 headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    with urllib.request.urlopen(req, timeout=10) as res:
        json.load(res)
    return next(r["id"] for r in http("GET", "/api/assets", port=port)[1] if r["filename"] == name)


def a_playlist(port=None):
    rows = http("GET", "/api/playlists", port=port)[1] or []
    playlist_id = (rows[0]["id"] if rows
                   else http("POST", "/api/playlists", {"name": "Test"}, port=port)[1]["id"])
    for row in http("GET", "/api/displays", port=port)[1] or []:
        if (row.get("schedule") or {}).get("default_playlist_id") != playlist_id:
            http("PUT", f"/api/displays/{row['name']}/schedule",
                 {"default_playlist_id": playlist_id, "windows": []}, port=port)
    return playlist_id


def item(item_id, port=None):
    return next(r for r in http("GET", "/api/playlist", port=port)[1] if r["id"] == item_id)


SCROLL = {"type": "Continuous", "options": {"speed": 400.0, "top_delay": 0, "return_delay": 500}}


def api_flow():
    print("\n[140] an item says when it moves on")
    with Server():
        pl = a_playlist()
        status, body = http("POST", "/api/playlist", {"url": "https://a.test/", "playlist_id": pl})
        check("an item naming nothing is created", status == 201, (status, body))
        check("and moves on after ten seconds",
              item(body["id"])["advance"] == {"on": "time", "seconds": 10}, item(body["id"]))
        check("the item no longer has a duration", "duration" not in item(body["id"]), item(body["id"]))

        status, body = http("POST", "/api/playlist", {"url": "https://b.test/", "playlist_id": pl,
                                                      "advance": {"on": "time", "seconds": 0}})
        check("seconds are clamped, not refused", item(body["id"])["advance"]["seconds"] == 1, body)

        status, body = http("POST", "/api/playlist", {"url": "https://c.test/", "playlist_id": pl,
                                                      "duration": 30})
        check("a request still sending duration is refused", status == 422, (status, body))

        print("\n[141] passes need something that ends")
        status, body = http("POST", "/api/playlist", {"url": "https://d.test/", "playlist_id": pl,
                                                      "advance": {"on": "passes", "count": 2}})
        check("a page that does not scroll cannot count passes", status == 400 and "error" in body,
              (status, body))
        status, body = http("POST", "/api/playlist", {"url": "https://e.test/", "playlist_id": pl,
                                                      "scroll_config": SCROLL,
                                                      "advance": {"on": "passes", "count": 2}})
        check("a scrolling page can", status == 201, (status, body))
        scrolling = body["id"]
        status, body = http("PUT", f"/api/playlist/{scrolling}", {"scroll_config": {"type": "None", "options": None}})
        check("switching its scroll off under the passes is refused", status == 400, (status, body))
        check("and nothing was written",
              item(scrolling)["scroll_config"]["type"] == "Continuous", item(scrolling))
        status, _ = http("PUT", f"/api/playlist/{scrolling}", {"advance": {"on": "time", "seconds": 5},
                                                              "scroll_config": {"type": "None", "options": None}})
        check("together with a switch to time it goes through", status == 200, status)
        video = upload("clip.mp4", b"\x00\x00\x00\x18ftypmp42", "video/mp4")
        status, body = http("POST", "/api/playlist", {"asset_id": video, "playlist_id": pl,
                                                      "advance": {"on": "passes", "count": 3}})
        check("a video can count passes without scrolling", status == 201, (status, body))
        status, _ = http("PUT", f"/api/playlist/{body['id']}", {"duration": 4})
        check("duration on an update is refused too", status == 422, status)


if __name__ == "__main__":
    try:
        api_flow()
    finally:
        for p in procs:
            p.terminate()
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
```

Run: `cargo build && python3 tests/cast/test_advance.py`
Expected: FAIL (no `advance` field; `duration` accepted).

- [ ] **Step 2: Schema — CREATE TABLE and the migration**

In `src/db.rs`, the `CREATE TABLE IF NOT EXISTS playlist_items`: replace the line `duration      INTEGER,` with

```rust
            advance       TEXT NOT NULL DEFAULT '{"on":"time","seconds":10}',
```

Directly after that `CREATE TABLE` statement (before the `scroll_config` probe), add:

```rust
    // `advance` replaced `duration`: a time *or* a number of passes. The old
    // value becomes a time, resolved exactly as the loop used to resolve it
    // (the item's own, else its asset's, else ten seconds) so every playlist
    // plays as before. One transaction, so a failure cannot leave the column
    // dropped with nothing backfilled -- and a failure is an error, because
    // every read selects `advance` and a half-migrated table blanks the screen.
    let has_column = |name: &'static str| async move {
        sqlx::query("SELECT count(*) FROM pragma_table_info('playlist_items') WHERE name = ?")
            .bind(name)
            .fetch_one(pool)
            .await
            .map(|row| row.get::<i32, _>(0) > 0)
            .unwrap_or(false)
    };
    let has_advance = has_column("advance").await;
    if has_column("duration").await {
        let mut tx = pool.begin().await?;
        if !has_advance {
            sqlx::query(r#"ALTER TABLE playlist_items ADD COLUMN advance TEXT NOT NULL DEFAULT '{"on":"time","seconds":10}'"#)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "UPDATE playlist_items SET advance = json_object('on', 'time', 'seconds',
                    MAX(1, MIN(604800, COALESCE(duration,
                        (SELECT a.duration FROM assets a WHERE a.id = playlist_items.asset_id), 10))))",
            )
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("ALTER TABLE playlist_items DROP COLUMN duration").execute(&mut *tx).await?;
        tx.commit().await?;
    } else if !has_advance {
        sqlx::query(r#"ALTER TABLE playlist_items ADD COLUMN advance TEXT NOT NULL DEFAULT '{"on":"time","seconds":10}'"#)
            .execute(pool)
            .await?;
    }
```

Add a test in the `#[cfg(test)]` module of `src/db.rs`, following the existing ones there (they create an old-shape table on an in-memory pool, then call `run_migrations`). Copy the pool setup the neighbouring `fit_mode` test uses:

```rust
    #[tokio::test]
    async fn duration_becomes_a_time_to_advance() {
        let pool = memory_pool().await; // the helper the neighbouring tests use
        sqlx::query("CREATE TABLE assets (id INTEGER PRIMARY KEY, filename TEXT NOT NULL,
                     local_path TEXT NOT NULL UNIQUE, mimetype TEXT NOT NULL,
                     duration INTEGER DEFAULT 10, created_at DATETIME)")
            .execute(&pool).await.unwrap();
        sqlx::query("CREATE TABLE playlist_items (id INTEGER PRIMARY KEY AUTOINCREMENT, asset_id INTEGER,
                     url TEXT, play_order INTEGER NOT NULL, duration INTEGER, is_enabled BOOLEAN DEFAULT 1)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO assets (id, filename, local_path, mimetype, duration)
                     VALUES (1, 'v.mp4', 'v.mp4', 'video/mp4', 37)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO playlist_items (id, asset_id, url, play_order, duration) VALUES
                     (1, NULL, 'https://own.test', 1, 25),
                     (2, 1, NULL, 2, NULL),
                     (3, NULL, 'https://none.test', 3, NULL),
                     (4, NULL, 'https://neg.test', 4, -3)")
            .execute(&pool).await.unwrap();
        run_migrations(&pool).await.unwrap();
        let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, advance FROM playlist_items ORDER BY id")
            .fetch_all(&pool).await.unwrap();
        let parsed: Vec<(i64, crate::advance::Advance)> =
            rows.into_iter().map(|(id, raw)| (id, serde_json::from_str(&raw).unwrap())).collect();
        use crate::advance::Advance::Time;
        assert_eq!(parsed, vec![(1, Time { seconds: 25 }), (2, Time { seconds: 37 }),
                                (3, Time { seconds: 10 }), (4, Time { seconds: 1 })]);
        let has_duration: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pragma_table_info('playlist_items') WHERE name = 'duration'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(has_duration, 0, "the old column is gone");
        run_migrations(&pool).await.unwrap(); // idempotent
    }
```

If the neighbouring tests build their pool inline rather than through a helper, build it the same way here instead of `memory_pool()`.

- [ ] **Step 3: The model and the reads**

In `src/models.rs`, `PlaylistItemWithAsset`: delete `pub duration: Option<i64>,` and the `asset_duration` field with its attribute; add after `play_order`:

```rust
    /// When this item moves on. See `crate::advance`.
    pub advance: sqlx::types::Json<crate::advance::Advance>,
```

In both SELECTs (`handlers.rs::get_playlist` and the loop's in `browser.rs`): replace `p.duration,` with
`COALESCE(p.advance, '{"on":"time","seconds":10}') as advance,` and delete `a.duration as asset_duration, ` (keep `a.filename`).

- [ ] **Step 4: The handlers**

In `src/handlers.rs`:

`AddToPlaylistRequest` and `UpdatePlaylistRequest` get `#[serde(deny_unknown_fields)]` under `#[derive(Deserialize)]`; in both replace `pub duration: Option<i64>,` with

```rust
    /// When the item moves on. Absent on create: the asset's length or ten
    /// seconds (`Advance::default_for`).
    pub advance: Option<crate::advance::Advance>,
```

In `edits_besides_the_playlist`, replace `duration,` with `advance,` in the destructure and `|| duration.is_some()` with `|| advance.is_some()`. In the test `a_move_travelling_with_any_other_edit_is_recognised`, replace `duration: None,` with `advance: None,` (and any other `duration` there with `advance: Some(Advance::Time { seconds: 5 })`).

`add_to_playlist`: replace the `fit_mode` block's asset lookup so the asset is read once, up front, right after the `fit_background` check:

```rust
    // The asset's type and length, read once: the fit default and the advance
    // default and check all depend on them.
    let asset: Option<(String, Option<i64>)> = match payload.asset_id {
        Some(asset_id) => sqlx::query_as("SELECT mimetype, duration FROM assets WHERE id = ?")
            .bind(asset_id)
            .fetch_optional(&state.pool)
            .await
            .unwrap_or_else(|e| {
                error!("Failed to read asset {}: {}", asset_id, e);
                None
            }),
        None => None,
    };
    let mimetype = asset.as_ref().map(|(m, _)| m.as_str());
    let fit_mode = match payload.fit_mode.as_deref() {
        Some(raw) => FitMode::from_value(raw),
        None => FitMode::default_for(mimetype),
    };
    let scroll_config = payload.scroll_config.unwrap_or(ScrollMode::None);
    let advance = payload
        .advance
        .map(crate::advance::Advance::clamped)
        .unwrap_or_else(|| crate::advance::Advance::default_for(asset.as_ref().and_then(|(_, d)| *d)));
    if let Err(message) = crate::advance::check(&advance, mimetype, &scroll_config) {
        return bad_request(message);
    }
```

(delete the later `let scroll_config = …` line.) In the INSERT, replace the column `duration` with `advance` and `.bind(payload.duration.map(clamp_duration))` with `.bind(sqlx::types::Json(advance))`.

`update_playlist_item`: directly after the `fit_background` check, before the source edit:

```rust
    // Checked against the item as it will be after this request -- a new scroll
    // mode or asset can pull the ground from under an existing `Passes` just as
    // a new advance can -- and before anything is written.
    if payload.advance.is_some() || payload.scroll_config.is_some() || payload.asset_id.is_some() {
        let current: Option<(sqlx::types::Json<crate::advance::Advance>, sqlx::types::Json<ScrollMode>, Option<String>)> =
            sqlx::query_as(
                r#"SELECT COALESCE(p.advance, '{"on":"time","seconds":10}'),
                          COALESCE(p.scroll_config, '{"type":"None","options":null}'),
                          a.mimetype
                   FROM playlist_items p LEFT JOIN assets a ON a.id = COALESCE(?, p.asset_id)
                   WHERE p.id = ?"#,
            )
            .bind(payload.asset_id)
            .bind(id)
            .fetch_optional(&state.pool)
            .await
            .unwrap_or_else(|e| {
                error!("Failed to read playlist item {} before an advance check: {}", id, e);
                None
            });
        if let Some((advance, scroll, mimetype)) = current {
            let advance = payload.advance.map(crate::advance::Advance::clamped).unwrap_or(advance.0);
            let scroll = payload.scroll_config.clone().unwrap_or(scroll.0);
            if let Err(message) = crate::advance::check(&advance, mimetype.as_deref(), &scroll) {
                return bad_request(message);
            }
        }
    }
```

Replace the `payload.duration` write with:

```rust
    if let Some(val) = payload.advance {
        let _ = sqlx::query("UPDATE playlist_items SET advance = ? WHERE id = ?")
            .bind(sqlx::types::Json(val.clamped()))
            .bind(id)
            .execute(&state.pool)
            .await;
    }
```

(`ScrollMode` must be `Clone`; it is.)

`update_asset`: keep writing `duration` (the measured length) but delete `state.notify_playlist_changed();` and its comment — nothing inherits it any more. Replace the comment above with `// The measured length of a video. Nothing plays by it; the playlist page prefills a time from it.`

- [ ] **Step 5: The webhook**

`src/webhook/mod.rs`: in `Event::ItemChanged`, replace `duration: u64,` with `advance: crate::advance::Advance,` and in the payload builder replace `"duration": duration,` with `"advance": advance,` (and the pattern binding). `src/webhook/api.rs`: in the `playback.item_changed` field list, replace `"duration"` with `"advance"`. In `browser.rs` the emit site gets `advance: item.advance.0.clamped(),` instead of `duration: duration_secs,` — Task 4 rewrites that block; for now make it compile with that one-line change and keep the old `duration_secs` computation replaced by:

```rust
                let advance = item.advance.0.clamped();
                let duration_secs = match advance {
                    crate::advance::Advance::Time { seconds } => u64::from(seconds),
                    crate::advance::Advance::Passes { .. } => crate::handlers::clamp_duration(10) as u64,
                };
```

(Task 4 replaces this stopgap.)

- [ ] **Step 6: Migrate the other suites off `duration`**

Mechanical: every request body `"duration": N` for a playlist item becomes `"advance": {"on": "time", "seconds": N}`, and every read `x["duration"]` of an item becomes `x["advance"]["seconds"]`. Files: `tests/cast/test_users.py` (lines ~127–286, including the merge assertions `reqs[0]["body"] == {"advance": {...}, "fit_mode": "cover"}`), `test_display.py` (`add_item`), `test_webhook.py` (`add_item`, and the template at ~394 becomes `"n": {{ data.advance.seconds }}`), `test_overlay.py` (~269, ~556, ~908), `test_media.py` (the item POSTs at ~372 and ~530). Upload `duration` form fields in `test_media.py` stay — that is the asset's measured length.

Run: `grep -n '"duration"' tests/cast/*.py` — the only remaining hits are the upload form field in `test_media.py` and `duration_secs` in `test_webhook.py`.

- [ ] **Step 7: Run everything touched**

Run: `cargo test && cargo build && python3 tests/cast/test_advance.py && python3 tests/cast/test_users.py`
Expected: all pass. Then (local instance on 3000 stopped first) `python3 tests/cast/test_display.py`, `python3 tests/cast/test_webhook.py`, `python3 tests/cast/test_overlay.py`, `python3 tests/cast/test_media.py` — not concurrently where CLAUDE.md forbids it.

- [ ] **Step 8: Commit**

```bash
git add -A src tests/cast
git commit -m "Store when an item advances instead of its duration"
```

---

### Task 3: The runtimes count passes

**Files:**
- Modify: `web/autoscroll.js`, `web/pdf_viewer.html`, `web/media_viewer.html`

**Interfaces:**
- Produces (in every page the display shows):
  - `globalThis.__advanceCounter(source) -> counter` with `source`, `target`, `passes`, `state() -> {passes, idle_ms, source}`, `reset(count)`, `alive(ts)`, `done() -> bool`, `atEnd(ts, minMs) -> bool` (true: stay where you are; false: go back to the start for the next pass)
  - `globalThis.__advance`: the counter that owns this page — `source` `scroll` by default, replaced by `pdf` (PDF viewer in step mode) or `video` (media viewer with a video)

- [ ] **Step 1: The counter factory in `web/autoscroll.js`**

At the very top of the IIFE, before `if (!globalThis.__asLog)`:

```js
  // Counting passes for an item that moves on after N of them. One factory for
  // all three runtimes (this one, the PDF viewer, the media viewer), so they
  // count the same way; browser.rs reads `globalThis.__advance.state()`.
  if (!globalThis.__advanceCounter) {
    globalThis.__advanceCounter = (source) => ({
      source,
      target: 0,
      passes: 0,
      progressTs: performance.now(),
      passStartTs: performance.now(),
      state() {
        return { passes: this.passes, idle_ms: Math.round(performance.now() - this.progressTs), source: this.source };
      },
      // Called by the controller when the item starts: counting begins now,
      // and with a target the content stops at the end of the last pass
      // instead of starting over -- the controller polls, and would otherwise
      // catch the top of the page flashing past.
      reset(count) {
        this.target = Number(count) > 0 ? Number(count) : 0;
        this.passes = 0;
        this.progressTs = this.passStartTs = performance.now();
      },
      done() {
        return this.target > 0 && this.passes >= this.target;
      },
      // The content is moving, or holding on purpose. A deliberate pause is
      // progress; only a stuck one is not.
      alive(ts) {
        this.progressTs = ts;
      },
      // At the end of the content. True: stay where you are (the pass is not
      // long enough yet, or the last one is complete). False: this pass is
      // counted, go back to the start for the next.
      atEnd(ts, minMs) {
        this.progressTs = ts;
        if (this.done()) return true;
        if (ts - this.passStartTs < minMs) return true;
        this.passes += 1;
        if (this.done()) return true;
        this.passStartTs = ts;
        return false;
      },
    });
  }
  const MIN_PASS_MS = 3000;
  // Installed only when the page has none: a viewer that counts for itself (a
  // video, a paged PDF) installs its own after this first run, and the
  // controller's re-evaluation after navigation must not take it back.
  if (!globalThis.__advance) globalThis.__advance = globalThis.__advanceCounter('scroll');
```

The scroll code below counts only while the page's counter is the scroll one. Add to `api`:

```js
    adv() {
      const a = globalThis.__advance;
      return a && a.source === 'scroll' ? a : null;
    },
```

Every use below is `const a = this.adv(); if (a) …`.

- [ ] **Step 2: Count in the scroll paths**

`scrollWindow` — the at-bottom branch becomes:

```js
      if (before.top >= max - 1 && max > 0) {
        if (this.returnDelayMs > 0 && ts < this.holdUntilTs) {
          return true;
        }
        const a = this.adv();
        if (a && a.atEnd(ts, MIN_PASS_MS)) return true;
        window.scrollTo(0, 0);
        this.holdUntilTs = ts + this.topDelayMs;
        return true;
      }
```

`scrollElement` — same change, with `el.scrollTop = 0;` in place of `window.scrollTo(0, 0);`.

`tick` — in the early hold return (`if (this.holdUntilTs > ts) {`) add `const held = this.adv(); if (held) held.alive(ts);` before scheduling the next frame. After the `moved` block and the repick, before scheduling the next frame:

```js
      const a = this.adv();
      if (a) {
        if (moved) {
          a.alive(ts);
        } else if (this.nothingToScroll()) {
          // A page that fits the screen is at its end at once: it counts a pass
          // per top-and-bottom delay, and never faster than MIN_PASS_MS.
          a.atEnd(ts, Math.max(MIN_PASS_MS, this.topDelayMs + this.returnDelayMs));
        }
      }
```

with a new method on `api`:

```js
    nothingToScroll() {
      const wm = this.windowMetrics();
      if (Math.max(0, wm.height - wm.viewport) > 1) return false;
      const el = this.scrollEl;
      if (!el || this.isRoot(el)) return true;
      const em = this.elementMetrics(el);
      return Math.max(0, em.height - em.viewport) <= 1;
    },
```

A page whose document is taller than the screen but does not move is *not* "nothing to scroll" — it reports no progress, and the controller's stall timeout moves it on.

`tickStep` — the early return `if (this.nextStepTs > ts) return;` becomes:

```js
      if (this.nextStepTs > ts) {
        const waiting = this.adv();
        if (waiting) waiting.alive(ts);
        return;
      }
```

In both `windowStep` and `elementStep`, the at-bottom branch becomes (window shown; element identical with `el.scrollTo`):

```js
        if (wm.top >= max - 1) {
          const a = this.adv();
          if (a && a.atEnd(ts, MIN_PASS_MS)) {
            this.nextStepTs = ts + 250;
            return true;
          }
          window.scrollTo({ top: 0, behavior: 'auto' });
          this.nextStepTs = ts + stepDelayMs;
          return true;
        }
```

and after a step (`window.scrollTo({ top: target, … })`) add `const a = this.adv(); if (a) a.alive(ts);`. In the `if (!moved)` block at the end of `tickStep`, add:

```js
        const a = this.adv();
        if (a && this.nothingToScroll()) a.atEnd(ts, Math.max(MIN_PASS_MS, stepDelayMs));
```

- [ ] **Step 3: The PDF viewer counts its own steps**

In `web/pdf_viewer.html`, the continuous mode already runs through `globalThis.__as`, so the scroll counter counts it. Step mode drives itself. Inside `if (mode === 'step') {`, before `const doStep`:

```js
        // This mode scrolls without the runtime, so it counts for itself -- the
        // same counter, installed over the scroll runtime's.
        const adv = globalThis.__advanceCounter ? globalThis.__advanceCounter('pdf') : null;
        if (adv) globalThis.__advance = adv;
        const MIN_PASS_MS = 3000;
```

`doStep` becomes:

```js
        const doStep = () => {
          const now = performance.now();
          if (maxScroll() <= 1) {
            // Nothing to scroll: one page that fits. A pass per step delay.
            if (adv) adv.atEnd(now, Math.max(MIN_PASS_MS, stepDelay));
            setTimeout(doStep, Math.max(1000, stepDelay));
            return;
          }

          if (atBottom()) {
            if (adv && adv.atEnd(now, MIN_PASS_MS)) {
              setTimeout(doStep, 250);
              return;
            }
            window.scrollTo({ top: 0, behavior: 'auto' });
            setTimeout(doStep, stepDelay);
            return;
          }

          if (adv) adv.alive(now);
          const target = window.scrollY + (Number.isFinite(stepPx) && stepPx > 0 ? stepPx : window.innerHeight);
          if (Number.isFinite(stepTime) && stepTime > 0) {
            window.scrollTo({ top: target, behavior: 'smooth' });
            setTimeout(() => setTimeout(doStep, stepDelay), stepTime);
          } else {
            window.scrollTo({ top: target, behavior: 'auto' });
            setTimeout(doStep, stepDelay);
          }
        };
```

The waits between steps are deliberate holds; `idle_ms` grows by at most `stepDelay + stepTime` between two `alive` calls, far below the default stall timeout.

- [ ] **Step 4: The media viewer counts a video's ends**

In `web/media_viewer.html`, add `<script src="/autoscroll.js"></script>` before the inline script (the runtime guards against a second install; this makes the counter factory available even before the controller's registration). Inside `if (kind === 'video') {` after `media.preload = 'auto';`:

```js
        // Counting ends for an item that moves on after N plays. With a target
        // (set by the controller's reset) the video stops looping and restarts
        // itself until the last end, where it stays on its final frame.
        const adv = globalThis.__advanceCounter ? globalThis.__advanceCounter('video') : null;
        if (adv) {
          const reset = adv.reset.bind(adv);
          adv.reset = (count) => {
            reset(count);
            media.loop = !(adv.target > 0);
          };
          media.addEventListener('timeupdate', () => adv.alive(performance.now()));
          media.addEventListener('ended', () => {
            if (!adv.atEnd(performance.now(), 0)) {
              media.currentTime = 0;
              play();
            }
          });
          globalThis.__advance = adv;
        }
```

`play` is declared below with `const`; move the `const play = () => { … };` definition above `if (kind === 'video') {` so the listener can reach it. Update the comment above `media.loop = true;` to: "`loop` unless the item counts plays (see the counter above)".

- [ ] **Step 5: Build and smoke-check**

Run: `touch src/web.rs && cargo build`
Expected: builds. (Behaviour is covered by Task 6.)

- [ ] **Step 6: Commit**

```bash
git add web/autoscroll.js web/pdf_viewer.html web/media_viewer.html
git commit -m "Count passes in the scroll runtime, the PDF viewer and the media viewer"
```

---

### Task 4: The loop moves on when the count is reached or the content stalls

**Files:**
- Modify: `src/browser.rs`

**Interfaces:**
- Consumes: `crate::advance::{Advance, RuntimeState, Verdict, verdict, POLL}`, `state.args.advance_stall_timeout`, the page's `globalThis.__advance`
- Produces: `async fn reset_advance(page: &Page, count: u16)`, `async fn read_advance(page: &Page) -> Result<Option<RuntimeState>, CdpError>`

- [ ] **Step 1: The two page helpers**

Beside `start_media` in `src/browser.rs`:

```rust
/// Start counting passes on `page`, with the target, right where the item's
/// clock would start. Probed, never assumed, like the other runtimes: a page
/// without the counter is the stall case the loop handles.
async fn reset_advance(page: &Page, count: u16) {
    let script = format!(
        "(() => {{ if (globalThis.__advance && globalThis.__advance.reset) globalThis.__advance.reset({count}); }})()"
    );
    if let Err(e) = page.evaluate(script).await {
        debug!("Could not reset the pass counter on this page: {}", e);
    }
}

/// What the page's counter says, or `None` when it has none. A navigation
/// under the evaluate is `None` as well; a lost connection is the error.
async fn read_advance(page: &Page) -> Result<Option<crate::advance::RuntimeState>, chromiumoxide::error::CdpError> {
    let raw = page
        .evaluate("JSON.stringify(globalThis.__advance && globalThis.__advance.state ? globalThis.__advance.state() : null)")
        .await;
    match raw {
        Ok(result) => Ok(result
            .into_value::<String>()
            .ok()
            .and_then(|text| serde_json::from_str::<Option<crate::advance::RuntimeState>>(&text).ok())
            .flatten()),
        Err(e) if is_connection_lost(&e) => Err(e),
        Err(_) => Ok(None),
    }
}
```

- [ ] **Step 2: Resolve the item's advance**

Replace the Task 2 stopgap (`let advance = …; let duration_secs = …;` and `let intended_duration = …`) with:

```rust
                let advance = item.advance.0.clamped();
                // A time runs out on its own; passes are counted by the page.
                let (time_limit, pass_target) = match advance {
                    crate::advance::Advance::Time { seconds } => (Some(Duration::from_secs(u64::from(seconds))), None),
                    crate::advance::Advance::Passes { count } => (None, Some(count)),
                };
```

The webhook emit uses `advance,`. The `info!` line becomes:

```rust
                info!(
                    "Showing item {} (keep_loaded: {}) at {} (advance: {:?})",
                    item.id, item.keep_loaded, redact_str(&target_url), advance
                );
```

- [ ] **Step 3: Start the count with the clock**

Directly before `start_media(&active_page).await;`:

```rust
                // Also for a keep_loaded tab brought to front: counting starts
                // when it is seen, not when it was loaded.
                if let Some(count) = pass_target {
                    reset_advance(&active_page, count).await;
                }
```

- [ ] **Step 4: The select loop**

Replace `let mut remaining = intended_duration;` through the end of the `while !remaining.is_zero() { … }` loop with the version below. Unchanged branches keep their bodies; the differences are: `remaining` is an `Option`, every recomputation is `remaining = time_limit.map(|t| t.saturating_sub(item_started_at.elapsed()));`, the boundary is computed only when something other than the poll woke the loop, and a new poll branch.

```rust
                let item_started_at = Instant::now();
                let mut remaining = time_limit;
                let stall = Duration::from_secs(state.args.advance_stall_timeout);
                let mut missing_since: Option<Instant> = None;
                // Recomputed after every wake except the pass poll, which comes
                // twice a second and changes nothing about the timetable.
                let mut boundary: Option<tokio::time::Instant> = None;
                let mut boundary_stale = true;

                let mut skip_requested = false;
                let mut reload_before_next = false;
                loop {
                    if matches!(remaining, Some(r) if r.is_zero()) {
                        break;
                    }
                    if boundary_stale {
                        // An edited timetable pokes `playlist_signal`, which lands
                        // here, so a timer computed before the edit never fires at
                        // a boundary that no longer exists.
                        boundary = match crate::schedule::load(&state.pool, &display_name).await {
                            Ok((_, windows)) => {
                                let now = crate::schedule::now();
                                crate::schedule::next_boundary(&windows, now)
                                    .and_then(|at| (at - now).to_std().ok())
                                    .map(|wait| tokio::time::Instant::now() + wait)
                            }
                            Err(e) => {
                                debug!("Failed to read the timetable for the boundary timer: {}", e);
                                None
                            }
                        };
                        boundary_stale = false;
                    }
                    tokio::select! {
                        _ = async {
                            match remaining {
                                Some(wait) => sleep(wait).await,
                                None => std::future::pending::<()>().await,
                            }
                        } => {
                            info!("Duration ended.");
                            break;
                        },
                        _ = async {
                            match pass_target {
                                Some(_) => sleep(crate::advance::POLL).await,
                                None => std::future::pending::<()>().await,
                            }
                        } => {
                            let count = pass_target.unwrap_or(1);
                            let report = match read_advance(&active_page).await {
                                Ok(report) => report,
                                Err(e) => {
                                    error!("Lost the page while counting passes: {}", e);
                                    reconnect_needed = true;
                                    break;
                                }
                            };
                            if report.is_some() {
                                missing_since = None;
                            } else if missing_since.is_none() {
                                missing_since = Some(Instant::now());
                            }
                            let missing_for = missing_since.map(|t| t.elapsed()).unwrap_or_default();
                            match crate::advance::verdict(report.as_ref(), count, missing_for, stall) {
                                crate::advance::Verdict::Wait => {}
                                crate::advance::Verdict::Done => {
                                    info!("Item {} ran its {} pass(es).", item.id, count);
                                    break;
                                }
                                crate::advance::Verdict::Stalled(why) => {
                                    warn!("Item {} stalled ({}), moving on.", item.id, why);
                                    break;
                                }
                            }
                        },
                        _ = display.skip_signal.notified() => {
                            info!("Skip signal received.");
                            skip_requested = true;
                            break;
                        },
                        _ = display.override_signal.notified() => {
                            info!("Override signal received, interrupting item.");
                            break;
                        },
                        _ = display.overlay_signal.notified() => {
                            boundary_stale = true;
                            // (body unchanged from before, except the last statement:)
                            // ... the existing re-read, re-seed and re-apply ...
                            remaining = time_limit.map(|t| t.saturating_sub(item_started_at.elapsed()));
                        },
                        _ = display.playlist_signal.notified() => {
                            boundary_stale = true;
                            let still_active =
                                is_playlist_item_active_now(&state, &display_name, item.id).await;
                            if still_active {
                                reload_before_next = true;
                                remaining = time_limit.map(|t| t.saturating_sub(item_started_at.elapsed()));
                            } else {
                                info!("Current item {} became inactive or was removed, skipping now.", item.id);
                                reload_before_next = true;
                                break;
                            }
                        }
                        _ = async {
                            match boundary {
                                Some(at) => tokio::time::sleep_until(at).await,
                                None => std::future::pending::<()>().await,
                            }
                        } => {
                            boundary_stale = true;
                            // ... the existing active_playlist comparison, unchanged ...
                            remaining = time_limit.map(|t| t.saturating_sub(item_started_at.elapsed()));
                        },
                    }
                }
```

Keep the overlay branch's and the boundary branch's existing bodies verbatim; only their final `remaining = …` statement changes as shown, and `boundary_stale = true;` is their first statement. `reconnect_needed = true; break;` inside the poll branch leaves the select loop; the existing `if reconnect_needed { break; }` checks after the item loop carry it out as they do for the other connection-lost paths — add `if reconnect_needed { break; }` right after this `loop` if the code below it would otherwise run `stop_scrolling` on a dead page (it logs and breaks on connection loss already, so this is belt and braces).

`start_media` stays after `reset_advance` — the media viewer's `reset` turns `loop` off before `start` plays.

- [ ] **Step 5: Build and run the unit tests**

Run: `cargo build && cargo test`
Expected: builds without warnings about unused `intended_duration`/`duration_secs`; all tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/browser.rs
git commit -m "Move on when the content has run its passes or stopped moving"
```

---

### Task 5: The operator pages

**Files:**
- Modify: `web/playlist.html`, `web/assets.html`, `web/proposals.js`

- [ ] **Step 1: An advance editor in `web/playlist.html`**

Beside `scrollEditor`, add:

```js
    // "Weiter nach": a time, or a number of passes of the content. Passes are
    // offered only where something ends -- a video, or a scroll mode -- and the
    // server refuses them anywhere else.
    function advanceEditor(initial) {
      const cfg = initial || { on: 'time', seconds: 10 };
      const on = el('select', {},
        el('option', { value: 'time', text: 'Zeit' }),
        el('option', { value: 'passes', text: 'Durchläufen' }));
      on.value = cfg.on === 'passes' ? 'passes' : 'time';
      const seconds = num(cfg.on === 'time' ? cfg.seconds : 10, '90px', 1);
      const count = num(cfg.on === 'passes' ? cfg.count : 1, '70px', 1);
      const unitS = el('span', { text: 's' });
      const unitX = el('span', { text: '×' });
      const hint = el('span', { class: 'muted', text: '' });
      const sync = () => {
        const passes = on.value === 'passes';
        seconds.hidden = unitS.hidden = passes;
        count.hidden = unitX.hidden = !passes;
      };
      on.addEventListener('change', sync);
      sync();
      return {
        root: el('span', {}, on, ' ', seconds, count, ' ', unitS, unitX, ' ', hint),
        read() {
          return on.value === 'passes'
            ? { on: 'passes', count: Math.max(1, Math.round(Number(count.value) || 1)) }
            : { on: 'time', seconds: Math.max(1, Math.round(Number(seconds.value) || 10)) };
        },
        // Durchläufe need something that ends; say why when there is none.
        setCountable(countable) {
          on.querySelector('option[value="passes"]').disabled = !countable;
          if (!countable && on.value === 'passes') { on.value = 'time'; sync(); }
          hint.textContent = countable ? '' : 'Durchläufe: nur mit Video oder Scroll-Modus';
        },
        setSeconds(value) { seconds.value = value; },
      };
    }
```

In the card builder: replace `const duration = num(item.duration ?? item.asset_duration ?? 10, '90px', 1);` with

```js
      const advance = advanceEditor(item.advance);
```

replace `field('Dauer (s)', duration),` with `field('Weiter nach', advance.root),`. After `const scroll = scrollEditor(item.scroll_config); card.append(scroll.root);` add:

```js
      const isVideo = (item.mimetype || '').toLowerCase().startsWith('video/');
      const recheck = () => advance.setCountable(isVideo || scroll.read().type !== 'None');
      scroll.root.addEventListener('change', recheck);
      recheck();
```

In the save handler: delete the `durationValue` check and replace `duration: durationValue,` with `advance: advance.read(),`.

The add form: replace the `Dauer (s)` label with

```html
        <label class="f"><span>Weiter nach</span><span id="addAdvance"></span></label>
```

and in the setup code after `const addScroll = scrollEditor(null); …`:

```js
    const addAdvance = advanceEditor(null);
    document.getElementById('addAdvance').append(addAdvance.root);
    const addCountable = () => {
      const asset = assets.find((a) => String(a.id) === document.getElementById('addAsset').value);
      const video = !!asset && (asset.mimetype || '').toLowerCase().startsWith('video/');
      addAdvance.setCountable(video || addScroll.read().type !== 'None');
    };
    addScroll.root.addEventListener('change', addCountable);
    addCountable();
```

The asset `change` listener becomes (prefill the time from a measured length, then re-check):

```js
    document.getElementById('addAsset').addEventListener('change', (e) => {
      const asset = assets.find((a) => String(a.id) === e.target.value);
      if (asset && asset.duration) addAdvance.setSeconds(asset.duration);
      addCountable();
    });
```

In the add payload replace `duration: Number(document.getElementById('addDuration').value || 10),` with `advance: addAdvance.read(),`.

Run: `grep -n "duration" web/playlist.html` — only the asset-length prefill remains.

- [ ] **Step 2: The asset length is shown, not edited — `web/assets.html`**

Replace the explanatory paragraph with:

```html
  <p class="muted">
    Die <strong>Länge</strong> eines Videos wird beim Hochladen gemessen. Sie ist
    nur Vorschlag: wählt man das Video in der Playlist mit „Weiter nach: Zeit“, ist
    sie vorbelegt. Wie lange ein Eintrag steht, entscheidet der Eintrag selbst.
  </p>
```

Header cell: `<th>Länge</th>` (drop the hint). In the row builder replace the `durInput` block with:

```js
        const isVideo = (asset.mimetype || '').startsWith('video/');
        cell(tr, isVideo && asset.duration ? `${asset.duration} s` : '–');
```

Delete `saveBtn` (its creation, listener, and its place in `actions.append`, which becomes `actions.append(delBtn, ...extra, feedback);`) and the whole `updateDuration` function. `remeasure` stays.

- [ ] **Step 3: Proposals read the new field — `web/proposals.js`**

In `render`, `LABELS`: replace `duration: 'Dauer',` with `advance: 'Weiter',`. In `shown(k, v)`, before the `typeof v === 'boolean'` line:

```js
      if (k === 'advance' && v && typeof v === 'object') {
        return v.on === 'passes' ? `${v.count} ${v.count === 1 ? 'Durchlauf' : 'Durchläufe'}` : `${v.seconds} s`;
      }
```

In the new-item line replace `if (body.duration) text(\`, ${body.duration} s\`);` with `if (body.advance) text(\`, ${shown('advance', body.advance)}\`);`.

- [ ] **Step 4: Build and check by hand in the browser**

Run: `cargo build` then start a scratch instance (not the user's DB) and open `/playlist.html`: the card shows "Weiter nach", "Durchläufen" is disabled for a URL item with scroll None and enabled after choosing Continuous; saving sends `advance`. Headless check: `test_media.py` case [111c] (prefill from the asset) must still pass after updating its assertion to read `#addAdvance input` instead of `#addDuration` — adjust that case in the same commit.

- [ ] **Step 5: Commit**

```bash
git add web/playlist.html web/assets.html web/proposals.js tests/cast/test_media.py
git commit -m "Choose when an item moves on in the playlist page"
```

---

### Task 6: The display really moves on — browser tests

**Files:**
- Modify: `tests/cast/test_advance.py` (browser half)

- [ ] **Step 1: Write the browser half**

Append to `tests/cast/test_advance.py`, before `if __name__ == "__main__":`:

```python
def spawn(cmd, **kwargs):
    p = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, **kwargs)
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


def current(port=HTTP):
    return http("GET", "/api/control/current", port=port)[1].get("item_id")


def serve_page(name, html, port=HTTP):
    """An HTML page as an asset, played through its /uploads/ URL."""
    upload(name, html.encode(), "text/html", port=port)
    row = next(r for r in http("GET", "/api/assets", port=port)[1] if r["filename"] == name)
    return f"http://127.0.0.1:{port}/uploads/{row['local_path']}"


TALL = "<!doctype html><body style='margin:0'><div style='height:3000px;background:linear-gradient(#f00,#00f)'></div>"
FITS = "<!doctype html><body><p>fits</p>"


async def time_on(item_id, port=HTTP, timeout=90):
    """Seconds item `item_id` stays current once it becomes current."""
    became = wait_for(lambda: current(port) == item_id, timeout)
    if not became:
        return None
    start = time.time()
    left = wait_for(lambda: current(port) != item_id, timeout)
    return time.time() - start if left else None


async def browser_flow():
    print("\n[142] a scrolling page moves on after its passes")
    if not os.path.exists(CHROME):
        print("  SKIP  no Chrome at " + CHROME)
        return
    shutil.rmtree(f"{SP}/advance-display", ignore_errors=True)
    spawn([CHROME, "--headless=new", f"--remote-debugging-port={CDP}",
           f"--user-data-dir={SP}/advance-display", "--no-first-run", "--no-sandbox",
           "--disable-gpu", "--window-size=1280,720",
           "--autoplay-policy=no-user-gesture-required", "about:blank"])
    check("display chrome up", wait_for(lambda: cdp.targets(CDP)) is not None)
    for leftover in ("a.db", "a.db-wal", "a.db-shm"):
        try:
            os.remove(os.path.join(SP, leftover))
        except FileNotFoundError:
            pass
    spawn([BIN, "--port", str(HTTP), "--cast-tls-port", str(TLS),
           "--database-path", f"{SP}/a.db", "--assets-dir", f"{SP}/assets",
           "--cast-cert-path", f"{SP}/cert.pem", "--no-launch-browser",
           "--managed-cert", "off", "--cdp-url", f"http://127.0.0.1:{CDP}",
           "--advance-stall-timeout", "8"])
    check("controller up", wait_for(lambda: http("GET", "/api/cast/info", port=HTTP)[0] == 200) is not None)

    pl = a_playlist(port=HTTP)
    filler = http("POST", "/api/playlist", {"url": serve_page("filler.html", FITS), "playlist_id": pl,
                                            "advance": {"on": "time", "seconds": 3}}, port=HTTP)[1]["id"]
    # 3000 px at 400 px/s is about 5.7 s down, plus half a second at the bottom.
    tall = http("POST", "/api/playlist", {"url": serve_page("tall.html", TALL), "playlist_id": pl,
                                          "scroll_config": SCROLL,
                                          "advance": {"on": "passes", "count": 2}}, port=HTTP)[1]["id"]
    shown = await time_on(tall)
    check("it moves on by itself", shown is not None, shown)
    check("after the second pass, not the first (each is ~6 s)", shown and 10 < shown < 30, shown)

    print("\n[143] it stays at the bottom after the last pass")
    ws_url, _ = cdp.page_ws(CDP)
    async with cdp.Session(ws_url) as page:
        wait_for(lambda: current() == tall, 60)
        tops = []
        deadline = time.time() + 40
        while time.time() < deadline and current() == tall:
            try:
                tops.append(await page.eval("Math.round(window.scrollY)"))
            except Exception:
                pass
            await asyncio.sleep(0.1)
        # After the final bottom the page must not jump back to 0 before leaving.
        last_bottom = max((i for i, t in enumerate(tops) if t > 2000), default=None)
        check("the last samples before leaving are at the bottom, not back at the top",
              last_bottom is not None and all(t > 2000 for t in tops[last_bottom:]), tops[-15:])

    print("\n[144] a page that fits the screen does not flash through its passes")
    http("PUT", f"/api/playlist/{tall}", {"enabled": False}, port=HTTP)
    fits = http("POST", "/api/playlist", {"url": serve_page("fits.html", FITS), "playlist_id": pl,
                                          "scroll_config": {"type": "Continuous", "options":
                                                            {"speed": 400.0, "top_delay": 0, "return_delay": 0}},
                                          "advance": {"on": "passes", "count": 2}}, port=HTTP)[1]["id"]
    shown = await time_on(fits)
    check("two passes take at least two minimum passes (2 x 3 s)", shown and shown >= 5.5, shown)
    http("PUT", f"/api/playlist/{fits}", {"enabled": False}, port=HTTP)

    print("\n[145] a video moves on after its plays")
    ws_url, _ = cdp.page_ws(CDP)
    async with cdp.Session(ws_url) as page:
        await page.call("Page.navigate", {"url": f"http://127.0.0.1:{HTTP}/upload.html"})
        await asyncio.sleep(2)
        # The recording script from test_media.py, case [111]: a ~3 s WebM.
        from test_media import RECORD
        await page.eval(RECORD, timeout=30)
    clip = wait_for(lambda: next((r for r in http("GET", "/api/assets", port=HTTP)[1]
                                  if r["filename"] == "recorded.webm"), None), 30)
    check("a short video was recorded", clip is not None, clip)
    video = http("POST", "/api/playlist", {"asset_id": clip["id"], "playlist_id": pl,
                                           "advance": {"on": "passes", "count": 2}}, port=HTTP)[1]["id"]
    ws_url, _ = cdp.page_ws(CDP)
    async with cdp.Session(ws_url) as page:
        wait_for(lambda: current() == video, 60)
        looping = await page.eval("(() => { const v = document.getElementById('media'); return v ? v.loop : null; })()")
        check("counting plays turns loop off", looping is False, looping)
    shown_video = None
    # time_on needs the item to become current again; measure its next showing.
    shown_video = await time_on(video)
    check("two ~3 s plays take ~6 s, well short of the 8 s stall and not 3 s",
          shown_video and 5 < shown_video < 12, shown_video)
    http("PUT", f"/api/playlist/{video}", {"enabled": False}, port=HTTP)

    print("\n[146] a paged PDF moves on after its last page")
    from test_media import pdf_bytes
    pdf = upload("slides.pdf", pdf_bytes(pages=3), "application/pdf", port=HTTP)
    paged = http("POST", "/api/playlist", {"asset_id": pdf, "playlist_id": pl, "fit_mode": "contain",
                                           "scroll_config": {"type": "Step", "options":
                                                             {"step_time": None, "step_px": None, "step_delay": 1500}},
                                           "advance": {"on": "passes", "count": 1}}, port=HTTP)[1]["id"]
    shown = await time_on(paged)
    check("three pages at 1.5 s each, then on", shown and 3 < shown < 20, shown)
    http("PUT", f"/api/playlist/{paged}", {"enabled": False}, port=HTTP)

    print("\n[147] a page whose counter is gone moves on after the stall timeout")
    stuck = http("POST", "/api/playlist", {"url": serve_page("stuck.html", TALL), "playlist_id": pl,
                                           "scroll_config": SCROLL,
                                           "advance": {"on": "passes", "count": 50}}, port=HTTP)[1]["id"]
    ws_url, _ = cdp.page_ws(CDP)
    async with cdp.Session(ws_url) as page:
        wait_for(lambda: current() == stuck, 60)
        started = time.time()
        # Remove it repeatedly: the controller re-evaluates the runtime after
        # navigation, but not while the item stands.
        while current() == stuck and time.time() - started < 40:
            try:
                await page.eval("delete globalThis.__advance; if (globalThis.__as) globalThis.__as.disable(); true")
            except Exception:
                pass
            await asyncio.sleep(0.5)
        waited = time.time() - started
    check("it moved on after about the stall timeout (8 s here), not the 50 passes",
          7 <= waited < 20, waited)
```

and replace the `__main__` block with:

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
        shutil.rmtree(f"{SP}/advance-display", ignore_errors=True)
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
```

Notes for the implementer: `upload.html` is where `RECORD` expects `#fileInput`/`#uploadBtn` — check the page `test_media.py` case [111] navigates to and use the same one. `/uploads/` serves HTML assets as pages; if an HTML asset is refused by the upload page's type check, serve the page from a `python3 -m http.server` on `127.0.0.1` in `SP` instead (write the files there). `GET /api/control/current` returns `{"item_id": …}` — confirm the key against `handlers.rs` and adjust `current()`.

- [ ] **Step 2: Run it**

Run: `cargo build && python3 tests/cast/test_advance.py`
Expected: ALL PASSED. Tune only timings that are genuinely environment-bound (headless speed), never the thresholds that encode the behaviour (second pass vs first; bottom vs top; stall vs count).

- [ ] **Step 3: Commit**

```bash
git add tests/cast/test_advance.py
git commit -m "Test that the display moves on after passes, at the bottom, and on a stall"
```

---

### Task 7: Documentation

**Files:**
- Modify: `CLAUDE.md`, `README.md`, `docs/features.md`, `docs/architecture.md` (if it describes the per-item select), `docs/roadmap.md`, `docs/superpowers/specs/2026-09-24-advance-on-content-end-design.md`

- [ ] **Step 1: CLAUDE.md**

In "The control loop", replace the line `item.duration.or(item.asset_duration).unwrap_or(10)` … with:

```markdown
**An item moves on by its `advance`** (`src/advance.rs`): `Time` runs out on its
own; `Passes` is counted by the page. Three rules:

- **What a pass is follows from the content and is not stored** — a video's
  end, the bottom of anything scrolled, a paged PDF's last page — so no stored
  kind can contradict the content. `advance::check` refuses `Passes` where
  nothing ends, on create *and* when an update changes the scroll mode or the
  asset under it.
- **The runtime stops at the end of the last pass** (`reset(count)` gives it the
  target). The loop polls every 500 ms; a runtime that started over would flash
  the top of the page first.
- **Every runtime publishes `globalThis.__advance`, built by
  `__advanceCounter` in `autoscroll.js`**; a viewer that counts for itself (a
  video, a paged PDF) installs its own over the scroll one, and the scroll
  runtime never takes it back. A deliberate hold is progress; only a stuck one
  is not — that is what `--advance-stall-timeout` (default 120 s) measures, and
  it moves a stuck page on with a `warn!`. It is a flag, not a setting: fault
  handling, not content.
```

Also in "Database", mention: `playlist_items.duration` was replaced by `advance` (JSON, `COALESCE`d on read), backfilled in one transaction with the loop's old fallback.

- [ ] **Step 2: README**

In the playlist API section, document `advance` (`{"on":"time","seconds":N}` | `{"on":"passes","count":N}`, `400` where nothing ends, `duration` is a `422`), and in the webhook section that `playback.item_changed` carries `advance` instead of `duration`.

- [ ] **Step 3: features.md, roadmap, spec**

`docs/features.md`: a short "When an item moves on" section in the operator's words (Zeit / Durchläufe, what a pass is for each content, the stall safety net). `docs/roadmap.md`: delete the "An item advances when its content ends" entry (it ships). Spec: change `Status:` to `implemented`, and fix the bounds line to `1..=604800` (seven days, the existing duration clamp).

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md README.md docs
git commit -m "Document advancing on content end"
```

---

## Final verification

- [ ] `cargo test` — all pass.
- [ ] Local instance on 3000 stopped; then one at a time: `test_advance.py`, `test_users.py`, `test_media.py`, `test_overlay.py`, `test_display.py`, `test_webhook.py`, `test_browser.py`, `test_basicauth.py`, `test_auth.py` — all pass (known pre-existing: `test_public.py`/`test_managed.py` mdns timing).
- [ ] Restart the user's local instance on 3000 with the release build so they can try it.
