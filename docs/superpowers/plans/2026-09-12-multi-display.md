# Multiple Displays Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One controller drives several screens — each with its own playlist, its own control loop and its own browser — from one port, one asset library and one admin page.

**Architecture:** Playlists become first-class rows that items belong to and displays are assigned. A display is a name the deployment declares with `--display`; the controller derives its CDP port, `app_id` and profile path from that name, supervises one Chromium per display and runs one control loop per display. Everything that is not playback state stays global. Window placement remains the window manager's job.

**Tech Stack:** Rust (edition 2021), axum 0.8, sqlx/SQLite, chromiumoxide over CDP, vanilla HTML/JS, stdlib-only Python tests.

**Spec:** `docs/superpowers/specs/2026-09-12-multi-display-design.md` — read it before Task 1. Every "why" below is short-form; the spec carries the reasoning and the measurements behind the two decisions that were reversed during design.

## Global Constraints

- **Rust edition 2021.** `cargo build` is the gate; there is no linting config. It is currently **warning-clean and must stay so**.
- **Schema lives only in `src/db.rs::run_migrations`**, idempotent: `CREATE TABLE IF NOT EXISTS` plus a `pragma_table_info` probe before each `ALTER TABLE ADD COLUMN`. `main.rs` must not create tables.
- **`COALESCE` every JSON column on every read path.** A real SQL `NULL` fails to decode and takes the whole query with it, and the call sites swallow that into an empty playlist — one bad row blanks a screen.
- **`PRAGMA foreign_keys` is on per connection** via `SqliteConnectOptions`; it is off by default in SQLite, which once made `ON DELETE CASCADE` a no-op.
- **The control loop owns what is on screen.** The API writes state and pokes a signal; it never navigates.
- **Always `notify_one()`, never `notify_waiters()`** on the per-display signals. The loop is only parked for part of its cycle, and `notify_waiters()` silently drops a "Play now".
- **Never consume `pending_jump` before the target has been found in a freshly fetched playlist** — peek, re-read, resolve, then clear.
- **Swallow handler DB errors** (`let _ = …` / `unwrap_or_default`) so the display never dies on a bad request — but `error!`-log first.
- **Handlers that can reject answer `{"error": "..."}`** in **German**, matching `src/settings.rs`.
- **`web/` is compiled into the binary** by `include_dir!` (`src/web.rs`). Rebuild after changing anything under `web/`, or the change is invisible.
- **UI is dependency-free vanilla HTML/JS.** Build rows with `textContent`/`createElement` through the `el()` helper, **never** `innerHTML` interpolation. A page that polls must not re-render what it polls into.
- **Stop any locally running instance before the Python suite.** `test_port.py` needs `3443` free; `test_browser.py`, `test_overlay.py` and `test_webhook.py` launch their own Chrome on `9222`/`9232`/`9242`.
- **Backward compatibility is a hard requirement**: with no `--display`, everything behaves exactly as it does today. These run unattended in a venue.
- **Never add a `Co-Authored-By: Claude` or `Claude-Session:` trailer to a commit.** Match the repo's style: a short imperative capitalised sentence, no type prefix.

---

## Phase 1 — Playlists become objects

Tasks 1–3. At the end of Phase 1 a single-screen device plays exactly what it played before, from a playlist it did not previously know it had. No display work yet, on purpose: the schema migration is the most dangerous part of the whole change and it is worth reviewing on its own, against a copy of a real device's database.

### Task 1: The schema and the backfill

**Files:**
- Modify: `src/db.rs` (add before the final `Ok(())` in `run_migrations`)
- Modify: `src/models.rs` (add `playlist_id` to `PlaylistItemWithAsset`)

**Interfaces:**
- Produces: tables `playlists` and `displays`; `playlist_items.playlist_id`; `PlaylistItemWithAsset.playlist_id: Option<i64>`.

- [ ] **Step 1: Write the failing tests**

Add a new `#[cfg(test)] mod tests` at the end of `src/db.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> Pool<Sqlite> {
        // `cache=shared` so every connection in the pool sees the same in-memory
        // database; a bare `sqlite::memory:` gives each connection its own.
        let pool = sqlx::SqlitePool::connect("sqlite::memory:?cache=shared")
            .await
            .unwrap();
        run_migrations(&pool).await.unwrap();
        pool
    }

    async fn count(pool: &Pool<Sqlite>, sql: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(sql).fetch_one(pool).await.unwrap()
    }

    #[tokio::test]
    async fn a_fresh_database_has_the_new_tables_and_no_rows_to_move() {
        let pool = pool().await;
        assert_eq!(count(&pool, "SELECT count(*) FROM playlists").await, 0);
        assert_eq!(count(&pool, "SELECT count(*) FROM displays").await, 0);
    }

    #[tokio::test]
    async fn existing_items_are_moved_into_a_standard_playlist() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:?cache=shared").await.unwrap();
        // The shape an older device has: items, and no notion of a playlist.
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
        for order in 1..=3 {
            sqlx::query("INSERT INTO playlist_items (url, play_order) VALUES (?, ?)")
                .bind(format!("https://example.test/{order}"))
                .bind(order)
                .execute(&pool)
                .await
                .unwrap();
        }

        run_migrations(&pool).await.unwrap();

        assert_eq!(count(&pool, "SELECT count(*) FROM playlists").await, 1);
        let name: String = sqlx::query_scalar("SELECT name FROM playlists")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(name, "Standard");
        assert_eq!(
            count(&pool, "SELECT count(*) FROM playlist_items WHERE playlist_id IS NULL").await,
            0,
            "every existing item must have been moved into the playlist"
        );
    }

    #[tokio::test]
    async fn the_backfill_is_idempotent() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:?cache=shared").await.unwrap();
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
        run_migrations(&pool).await.unwrap();

        assert_eq!(
            count(&pool, "SELECT count(*) FROM playlists").await,
            1,
            "a second run must not create a second Standard playlist"
        );
    }

    #[tokio::test]
    async fn a_database_that_already_has_playlists_is_left_alone() {
        let pool = pool().await;
        sqlx::query("INSERT INTO playlists (name) VALUES ('Foyer')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO playlist_items (url, play_order) VALUES ('https://a.test', 1)")
            .execute(&pool)
            .await
            .unwrap();

        run_migrations(&pool).await.unwrap();

        // The item has no playlist, but the backfill must not fire: the operator
        // already has playlists and moving strays into a new "Standard" would be
        // the migration inventing a decision.
        assert_eq!(count(&pool, "SELECT count(*) FROM playlists").await, 1);
        let name: String = sqlx::query_scalar("SELECT name FROM playlists")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(name, "Foyer");
    }

    #[tokio::test]
    async fn deleting_a_playlist_unassigns_it_from_a_display() {
        let pool = pool().await;
        sqlx::query("INSERT INTO playlists (id, name) VALUES (1, 'Foyer')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO displays (name, playlist_id) VALUES ('foyer', 1)")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("DELETE FROM playlists WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();

        let assigned: Option<i64> =
            sqlx::query_scalar("SELECT playlist_id FROM displays WHERE name = 'foyer'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(assigned.is_none(), "ON DELETE SET NULL did not fire -- is PRAGMA foreign_keys on?");
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test db::tests`

Expected: FAIL — `no such table: playlists`.

Note: this is a binary-only crate, so `cargo test --lib` reports "no library targets". Use `cargo test db::tests` (and `cargo test` for everything) throughout this plan.

- [ ] **Step 3: Add the tables and the column**

In `src/db.rs::run_migrations`, immediately before the final `Ok(())`:

```rust
    // 9. Playlists as objects. Items belonged to "the" playlist; now they belong
    // to one of several, and a display is assigned one.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS playlists (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            name       TEXT NOT NULL,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );"
    )
    .execute(pool)
    .await?;

    // 10. The screens this deployment declared. `name` is the identity, because
    // an operator's playlist assignment has to survive a restart and an index
    // would not. ON DELETE SET NULL is what keeps a playlist alive when the
    // screen it was assigned to is taken away -- that is the whole point of
    // playlists being objects.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS displays (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            name        TEXT NOT NULL UNIQUE,
            label       TEXT,
            playlist_id INTEGER,
            FOREIGN KEY(playlist_id) REFERENCES playlists(id) ON DELETE SET NULL
        );"
    )
    .execute(pool)
    .await?;

    let has_playlist_id: bool = sqlx::query(
        "SELECT count(*) FROM pragma_table_info('playlist_items') WHERE name='playlist_id'",
    )
    .fetch_one(pool)
    .await
    .map(|row| row.get::<i32, _>(0) > 0)
    .unwrap_or(false);

    if !has_playlist_id {
        let _ = sqlx::query("ALTER TABLE playlist_items ADD COLUMN playlist_id INTEGER")
            .execute(pool)
            .await;
    }

    // 11. One-time backfill. Only when the operator has no playlists at all:
    // once they do, a stray item with no playlist is their business, and
    // sweeping it into a new "Standard" would be the migration inventing a
    // decision nobody asked for.
    let playlists: i64 = sqlx::query_scalar("SELECT count(*) FROM playlists")
        .fetch_one(pool)
        .await
        .unwrap_or(0);
    let orphans: i64 = sqlx::query_scalar("SELECT count(*) FROM playlist_items")
        .fetch_one(pool)
        .await
        .unwrap_or(0);

    if playlists == 0 && orphans > 0 {
        let row = sqlx::query("INSERT INTO playlists (name) VALUES ('Standard') RETURNING id")
            .fetch_one(pool)
            .await?;
        let id: i64 = row.get(0);
        sqlx::query("UPDATE playlist_items SET playlist_id = ? WHERE playlist_id IS NULL")
            .bind(id)
            .execute(pool)
            .await?;
        tracing::info!("Moved {} existing items into the 'Standard' playlist", orphans);
    }
```

- [ ] **Step 4: Add the struct field**

In `src/models.rs`, in `PlaylistItemWithAsset`, beside the other columns:

```rust
    /// Which playlist this item belongs to. `Option` because the column is added
    /// by migration and an item written by an older binary has none until the
    /// backfill runs.
    #[sqlx(default)]
    pub playlist_id: Option<i64>,
```

- [ ] **Step 5: Run the tests**

Run: `cargo test db::tests`

Expected: PASS, 5 tests.

- [ ] **Step 6: Verify against a real database**

This is the step the phase exists for. Copy a real device database and migrate it:

```bash
cp miniclient.db /tmp/migrate-check.db
cargo run --quiet -- --database-path /tmp/migrate-check.db --no-launch-browser --disable-cast --managed-cert off --port 3999 &
sleep 5
sqlite3 /tmp/migrate-check.db "SELECT count(*) FROM playlists; SELECT count(*) FROM playlist_items WHERE playlist_id IS NULL; SELECT name FROM playlists;"
kill %1
```

Expected: `1`, `0`, `Standard`. If any item still has a `NULL` playlist, stop and report — that is a real device losing its playlist.

- [ ] **Step 7: Commit**

```bash
git add src/db.rs src/models.rs
git commit -m "Give playlist items a playlist to belong to"
```

---

### Task 2: The playlist API

**Files:**
- Create: `src/playlists.rs`
- Modify: `src/main.rs` (add `mod playlists;` and `.merge(playlists::routes())`)
- Modify: `src/handlers.rs` (`get_playlist` filters, `add_to_playlist` requires a playlist)

**Interfaces:**
- Consumes: the `playlists` table from Task 1.
- Produces: `pub fn playlists::routes() -> Router<AppState>` serving `/api/playlists` and `/api/playlists/{id}`; `get_playlist` accepting `?playlist_id=`; `AddToPlaylistRequest.playlist_id: i64`.

- [ ] **Step 1: Write the failing tests**

Create `src/playlists.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> sqlx::SqlitePool {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:?cache=shared").await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn a_playlist_holding_items_cannot_be_deleted() {
        let pool = pool().await;
        sqlx::query("INSERT INTO playlists (id, name) VALUES (1, 'Foyer')")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO playlist_items (url, play_order, playlist_id) VALUES ('https://a.test', 1, 1)")
            .execute(&pool).await.unwrap();

        let held = items_in(&pool, 1).await;
        assert_eq!(held, 1, "the guard counts what the refusal will report");
    }

    #[tokio::test]
    async fn an_empty_playlist_reports_nothing_held() {
        let pool = pool().await;
        sqlx::query("INSERT INTO playlists (id, name) VALUES (2, 'Leer')")
            .execute(&pool).await.unwrap();
        assert_eq!(items_in(&pool, 2).await, 0);
    }

    #[tokio::test]
    async fn a_name_must_not_be_blank() {
        assert!(validate_name("Foyer").is_ok());
        assert!(validate_name("   ").is_err(), "whitespace is not a name");
        assert!(validate_name("").is_err());
        // A name is the only handle the admin page and a log line have.
        assert_eq!(validate_name("  Foyer  ").unwrap(), "Foyer", "stored trimmed");
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test playlists::`

Expected: FAIL — `cannot find function items_in in this scope`.

- [ ] **Step 3: Write the module**

Put this above the test module in `src/playlists.rs`:

```rust
//! Playlists as objects: a thing an operator names and assigns to a screen.
//!
//! Items used to belong to "the" playlist. They now belong to one of several,
//! which is what lets two screens share one and what lets a playlist outlive the
//! screen it was assigned to.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, put},
    Json, Router,
};
use serde_json::json;
use tracing::error;

use crate::models::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/playlists", get(list).post(create))
        .route("/api/playlists/{id}", put(rename).delete(remove))
}

#[derive(serde::Deserialize)]
struct NameRequest {
    name: String,
}

fn bad(message: String) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

/// A name is the only handle the admin page and a log line have for a playlist,
/// so a blank one would be untraceable. Stored trimmed for the same reason.
pub fn validate_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Der Name darf nicht leer sein.".to_string());
    }
    Ok(trimmed.to_string())
}

/// How many items a playlist holds. The delete guard reports this number, so the
/// operator is told what is in the way rather than just being refused.
pub async fn items_in(pool: &sqlx::SqlitePool, id: i64) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM playlist_items WHERE playlist_id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

async fn list(State(state): State<AppState>) -> Response {
    let rows = sqlx::query_as::<_, (i64, String, i64)>(
        "SELECT p.id, p.name, (SELECT count(*) FROM playlist_items i WHERE i.playlist_id = p.id)
         FROM playlists p ORDER BY p.name ASC",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_else(|e| {
        error!("Failed to list playlists: {}", e);
        Vec::new()
    });

    Json(
        rows.into_iter()
            .map(|(id, name, items)| json!({ "id": id, "name": name, "items": items }))
            .collect::<Vec<_>>(),
    )
    .into_response()
}

async fn create(State(state): State<AppState>, Json(payload): Json<NameRequest>) -> Response {
    let name = match validate_name(&payload.name) {
        Ok(name) => name,
        Err(message) => return bad(message),
    };
    match sqlx::query_scalar::<_, i64>("INSERT INTO playlists (name) VALUES (?) RETURNING id")
        .bind(&name)
        .fetch_one(&state.pool)
        .await
    {
        Ok(id) => Json(json!({ "id": id })).into_response(),
        Err(e) => {
            error!("Failed to create playlist: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Konnte nicht gespeichert werden." })),
            )
                .into_response()
        }
    }
}

async fn rename(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<NameRequest>,
) -> Response {
    let name = match validate_name(&payload.name) {
        Ok(name) => name,
        Err(message) => return bad(message),
    };
    match sqlx::query("UPDATE playlists SET name = ? WHERE id = ?")
        .bind(&name)
        .bind(id)
        .execute(&state.pool)
        .await
    {
        Ok(done) if done.rows_affected() == 0 => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Nicht gefunden." })),
        )
            .into_response(),
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => {
            error!("Failed to rename playlist {}: {}", id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Konnte nicht gespeichert werden." })),
            )
                .into_response()
        }
    }
}

/// Refused while it still holds items. Deleting the playlist would leave them
/// with a dangling `playlist_id` and no screen would ever play them again --
/// worse than an error, because nothing would say so.
async fn remove(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let held = items_in(&state.pool, id).await;
    if held > 0 {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!("Enthält noch {held} Elemente. Erst leeren oder verschieben.")
            })),
        )
            .into_response();
    }
    if let Err(e) = sqlx::query("DELETE FROM playlists WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await
    {
        error!("Failed to delete playlist {}: {}", id, e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "Konnte nicht gelöscht werden." })),
        )
            .into_response();
    }
    Json(json!({ "ok": true })).into_response()
}
```

In `src/main.rs`, add `mod playlists;` beside the other module declarations and `.merge(playlists::routes())` beside the other merges.

- [ ] **Step 4: Scope the item endpoints**

In `src/handlers.rs`, change `get_playlist` to take an optional filter. Replace its signature and query:

```rust
#[derive(serde::Deserialize)]
pub struct PlaylistQuery {
    pub playlist_id: Option<i64>,
}

pub async fn get_playlist(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<PlaylistQuery>,
) -> impl IntoResponse {
    // scroll_config is COALESCEd: a NULL there fails to decode into Json<ScrollMode>,
    // which fails the whole query and would silently return an empty playlist.
    // The playlist filter is applied with a NULL-tolerant predicate so that
    // `?playlist_id=` absent still means "everything", which is what the legacy
    // single-display callers expect.
    let items = sqlx::query_as::<_, PlaylistItemWithAsset>(
        r#"
        SELECT
            p.id, p.asset_id, p.url, p.play_order, p.duration, p.is_enabled as enabled, p.is_enabled,
            p.start_date, p.end_date, p.playlist_id,
            COALESCE(p.keep_loaded, 0) as keep_loaded,
            COALESCE(p.scroll_config, '{"type":"None","options":null}') as scroll_config,
            COALESCE(p.overlay_config, 'null') as overlay_config,
            a.local_path, a.mimetype, a.duration as asset_duration, a.filename
        FROM playlist_items p
        LEFT JOIN assets a ON p.asset_id = a.id
        WHERE (?1 IS NULL OR p.playlist_id = ?1)
        ORDER BY p.play_order ASC
        "#
    )
    .bind(query.playlist_id)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_else(|e| {
        error!("Failed to load playlist: {}", e);
        Vec::new()
    });

    (StatusCode::OK, Json(items))
}
```

Add `playlist_id` to `AddToPlaylistRequest`:

```rust
    /// Which playlist the item joins. Required: an item with no playlist is one
    /// no screen will ever play, and nothing would say so.
    pub playlist_id: i64,
```

and bind it in the `INSERT` inside `add_to_playlist` — find the existing insert and add the column and its bind.

In `move_playlist_item`, the renumbering must stay inside the item's own playlist. Find the `UPDATE … SET play_order` renumbering and add `WHERE playlist_id = ?` scoped to the moved item's playlist, reading that id first:

```rust
    let playlist_id: Option<i64> =
        sqlx::query_scalar("SELECT playlist_id FROM playlist_items WHERE id = ?")
            .bind(id)
            .fetch_optional(&state.pool)
            .await
            .unwrap_or(None)
            .flatten();
```

Renumbering across playlists would interleave two screens' orders, which shows up as items playing in the wrong sequence on a screen nobody was touching.

- [ ] **Step 5: Run the tests and build**

Run: `cargo test && cargo build`

Expected: all tests pass, build warning-clean.

- [ ] **Step 6: Check it by hand**

```bash
cargo run --quiet -- --database-path /tmp/api-check.db --no-launch-browser --disable-cast --managed-cert off --port 3999 &
sleep 5
curl -s -X POST localhost:3999/api/playlists -H 'content-type: application/json' -d '{"name":"Foyer"}'
curl -s localhost:3999/api/playlists
curl -s -X POST localhost:3999/api/playlists -H 'content-type: application/json' -d '{"name":"   "}'
curl -s -X POST localhost:3999/api/playlist -H 'content-type: application/json' -d '{"url":"https://a.test","playlist_id":1}'
curl -s -X DELETE localhost:3999/api/playlists/1
kill %1
```

Expected: `{"id":1}`; a one-element list with `"items":0`; `{"error":"Der Name darf nicht leer sein."}`; an item id; then a `409` naming one held item.

- [ ] **Step 7: Commit**

```bash
git add src/playlists.rs src/main.rs src/handlers.rs
git commit -m "Serve playlists as their own resource"
```

---

### Task 3: The playlist selector

**Files:**
- Modify: `web/playlist.html`

**Interfaces:**
- Consumes: `GET/POST /api/playlists`, `PUT/DELETE /api/playlists/{id}`, `GET /api/playlist?playlist_id=`, `POST /api/playlist` with `playlist_id`.

- [ ] **Step 1: Add the selector**

At the top of `web/playlist.html`, above the status bar, add a row holding a `<select id="playlistPick">`, a **Neu** button, a **Umbenennen** button and a **Löschen** button, plus a `<span class="feedback" id="playlistFeedback">`.

In the script:

```javascript
    // The selected playlist scopes everything below. Kept in the URL so a
    // reload, or a link an operator sends a colleague, lands on the same one.
    let currentPlaylist = null;

    async function loadPlaylists() {
      const lists = await fetch('/api/playlists').then((r) => r.json()).catch(() => []);
      const pick = document.getElementById('playlistPick');
      pick.replaceChildren();
      for (const list of lists) {
        pick.append(el('option', {
          value: String(list.id),
          text: `${list.name} (${list.items})`,
        }));
      }
      const wanted = new URLSearchParams(location.search).get('playlist');
      currentPlaylist = (wanted && lists.some((l) => String(l.id) === wanted))
        ? wanted
        : (lists[0] ? String(lists[0].id) : null);
      if (currentPlaylist) pick.value = currentPlaylist;
      return lists;
    }
```

Wire `pick.onchange` to set `currentPlaylist`, push it into the URL with `history.replaceState`, and reload the list. Every existing `fetch('/api/playlist')` becomes `fetch('/api/playlist?playlist_id=' + currentPlaylist)`, and the add-item body gains `playlist_id: Number(currentPlaylist)`.

**Deleting a playlist** shows the server's own `409` message inline through the existing `flash()` helper — do not pre-empt it in the page, because the server counts the items and the page would have to count them again.

**With no playlists at all**, hide the item editor and show one line: `Noch keine Playlist. Lege eine an, um Elemente hinzuzufügen.` A page that offers an item form with nowhere to put the item is a page that produces a 400 for a reason the operator cannot see.

- [ ] **Step 2: Rebuild — the page is compiled in**

Run: `cargo build`

`web/` is embedded by `include_dir!`. Without this the page is the old one and it looks exactly like a change that did not work.

- [ ] **Step 3: Check it by hand**

Start the server, open `http://localhost:3999/playlist.html`, and confirm: the selector lists playlists with their item counts; switching updates the items below; the URL carries `?playlist=`; adding an item lands in the selected playlist; deleting a non-empty playlist shows the German refusal inline; and typing in an item card survives three poll cycles.

- [ ] **Step 4: Commit**

```bash
git add web/playlist.html
git commit -m "Pick which playlist the editor is editing"
```

---

## Phase 2 — The controller drives N displays

Tasks 4–12. Task 7 is the risky one: it splits the control loop, which owns what is on every screen.

### Task 4: Declaring displays

**Files:**
- Create: `src/display.rs`
- Modify: `src/models.rs` (the `--display` arg)
- Modify: `src/main.rs` (`mod display;`)

**Interfaces:**
- Produces: `pub struct DisplayConfig { pub name: String, pub cdp_url: String, pub window_class: String, pub user_data_dir: PathBuf }`; `pub fn configure(args: &Args) -> Result<Vec<DisplayConfig>, String>`.

- [ ] **Step 1: Write the failing tests**

Create `src/display.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn args_with(displays: Vec<String>) -> crate::models::Args {
        let mut args = crate::models::Args::parse_from(["miniclientcontrol"]);
        args.display = displays;
        args
    }

    #[test]
    fn no_flag_means_one_display_that_behaves_exactly_as_today() {
        let configured = configure(&args_with(vec![])).unwrap();
        assert_eq!(configured.len(), 1);
        assert_eq!(configured[0].name, "default");
        // The existing defaults, untouched: these devices run unattended and an
        // upgrade must not move their CDP port or their window class.
        assert_eq!(configured[0].cdp_url, "http://127.0.0.1:9222");
        assert_eq!(configured[0].window_class, "miniclientcontrol-9222");
    }

    #[test]
    fn declared_names_get_derived_ports_and_classes() {
        let configured =
            configure(&args_with(vec!["foyer".into(), "werkstatt".into()])).unwrap();
        assert_eq!(configured.len(), 2);
        assert_eq!(configured[0].cdp_url, "http://127.0.0.1:9222");
        assert_eq!(configured[1].cdp_url, "http://127.0.0.1:9223");
        // Named, not numbered: a human writes the window-manager config and
        // should read "werkstatt" there, not "9223".
        assert_eq!(configured[0].window_class, "miniclientcontrol-foyer");
        assert_eq!(configured[1].window_class, "miniclientcontrol-werkstatt");
        assert!(configured[1].user_data_dir.to_string_lossy().contains("werkstatt"));
    }

    #[test]
    fn an_explicit_port_wins() {
        let configured =
            configure(&args_with(vec!["foyer:9300".into(), "werkstatt".into()])).unwrap();
        assert_eq!(configured[0].cdp_url, "http://127.0.0.1:9300");
        // The implicit one still counts from the base by index, so declaring an
        // explicit port for one display does not silently move another.
        assert_eq!(configured[1].cdp_url, "http://127.0.0.1:9223");
    }

    #[test]
    fn duplicates_and_nonsense_are_refused_at_startup() {
        assert!(configure(&args_with(vec!["foyer".into(), "foyer".into()])).is_err());
        assert!(configure(&args_with(vec!["".into()])).is_err());
        assert!(configure(&args_with(vec!["foyer:nichtszahl".into()])).is_err());
        // A name reaches a window-manager config and a filesystem path, so keep
        // it to something both can hold without quoting.
        assert!(configure(&args_with(vec!["foyer schirm".into()])).is_err());
        assert!(configure(&args_with(vec!["../etc".into()])).is_err());
    }

    #[test]
    fn two_displays_cannot_share_a_port() {
        assert!(configure(&args_with(vec!["a:9300".into(), "b:9300".into()])).is_err());
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test display::`

Expected: FAIL — `cannot find function configure in this scope`.

- [ ] **Step 3: Add the argument**

In `src/models.rs`, in `Args`:

```rust
    /// A screen this deployment drives, as `name` or `name:cdp-port`.
    ///
    /// Repeat it once per screen. The name is the identity an operator's
    /// playlist assignment is stored against, and it is what the window-manager
    /// config matches on — `miniclientcontrol-<name>` becomes the Wayland
    /// `app_id`. Passing none keeps the single-display behaviour exactly as it
    /// was, which is why this is a `Vec` with no clap default rather than an
    /// `Option`.
    #[arg(long = "display", env = "DISPLAYS", value_delimiter = ',')]
    pub display: Vec<String>,
```

- [ ] **Step 4: Write the module**

Put this above the test module in `src/display.rs`:

```rust
//! Which screens this deployment drives.
//!
//! Declared, not discovered. Discovery was prototyped against sway and works,
//! but it puts compositor-specific knowledge inside the controller — sway calls
//! an output `HDMI-A-1` where i3 says `HDMI-1` — and it takes window placement
//! away from the window manager, which is where this project already puts it.
//! See the design spec for the measurements.

use std::path::PathBuf;

use crate::models::Args;

/// The first CDP port, and the one a single-display deployment has always used.
const BASE_CDP_PORT: u16 = 9222;

#[derive(Clone, Debug)]
pub struct DisplayConfig {
    pub name: String,
    pub cdp_url: String,
    /// Becomes the Wayland `app_id`, which is how the window manager tells two
    /// of our windows apart and puts each on the right output.
    pub window_class: String,
    pub user_data_dir: PathBuf,
}

/// A name ends up in a window-manager config and in a filesystem path, so it is
/// restricted to what both hold without quoting or escaping.
///
/// Deliberately not called `validate_name`: `playlists::validate_name` already
/// exists and returns the trimmed name rather than `()`, and two same-named
/// helpers with different return types is how a call site ends up quietly
/// wrong.
fn validate_display_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Ein Display-Name darf nicht leer sein.".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "Display-Name '{name}': nur Buchstaben, Ziffern, - und _ sind erlaubt."
        ));
    }
    Ok(())
}

/// Resolve the declared displays, or the single implicit one.
///
/// Fails the process rather than degrading: a typo that silently dropped a
/// screen would show up as a black panel in a venue, with nothing saying why.
pub fn configure(args: &Args) -> Result<Vec<DisplayConfig>, String> {
    if args.display.is_empty() {
        // Exactly today's behaviour, down to the class derived from the port.
        let port = crate::chromium::debugging_port(&args.cdp_url).unwrap_or(BASE_CDP_PORT);
        return Ok(vec![DisplayConfig {
            name: "default".to_string(),
            cdp_url: args.cdp_url.clone(),
            window_class: crate::chromium::window_class(args, port),
            user_data_dir: crate::chromium::user_data_dir(args, port),
        }]);
    }

    let mut out: Vec<DisplayConfig> = Vec::new();
    for (index, raw) in args.display.iter().enumerate() {
        let (name, explicit) = match raw.split_once(':') {
            Some((name, port)) => {
                let parsed: u16 = port
                    .parse()
                    .map_err(|_| format!("Display '{name}': '{port}' ist kein Port."))?;
                (name, Some(parsed))
            }
            None => (raw.as_str(), None),
        };
        validate_display_name(name)?;
        if out.iter().any(|d| d.name == name) {
            return Err(format!("Display '{name}' ist doppelt deklariert."));
        }
        // Implicit ports count from the base by declaration index, so giving one
        // display an explicit port does not shift another one's.
        let port = explicit.unwrap_or(BASE_CDP_PORT + index as u16);
        if out.iter().any(|d| d.cdp_url.ends_with(&format!(":{port}"))) {
            return Err(format!("CDP-Port {port} ist doppelt vergeben."));
        }
        out.push(DisplayConfig {
            name: name.to_string(),
            cdp_url: format!("http://127.0.0.1:{port}"),
            window_class: format!("miniclientcontrol-{name}"),
            user_data_dir: PathBuf::from(format!("/tmp/miniclientcontrol-chromium-{name}")),
        });
    }
    Ok(out)
}
```

`debugging_port` is currently private in `src/chromium.rs` — make it `pub(crate)`. Report that in your notes; it is the only visibility this task widens.

Add `mod display;` to `src/main.rs`.

- [ ] **Step 5: Run the tests**

Run: `cargo test display::`

Expected: PASS, 5 tests.

- [ ] **Step 6: Commit**

```bash
git add src/display.rs src/models.rs src/main.rs src/chromium.rs
git commit -m "Declare the screens a deployment drives"
```

---

### Task 5: Per-display state

**Files:**
- Modify: `src/models.rs` (the `Display` struct, `AppState`)
- Modify: `src/main.rs` (build the map)
- Modify: `src/handlers.rs`, `src/cast.rs`, `src/settings.rs` (follow the moved fields)

**Interfaces:**
- Consumes: `DisplayConfig` from Task 4.
- Produces: `pub struct Display` with `name`, `cdp_url`, `current_item_id`, `pending_jump`, `override_item`, `browser_pid`, `skip_signal`, `playlist_signal`, `override_signal`, `overlay_signal`; `AppState::displays: Arc<Vec<Arc<Display>>>`; `AppState::display(&self, name: &str) -> Option<Arc<Display>>`; `AppState::primary(&self) -> Arc<Display>`.

- [ ] **Step 1: Write the failing test**

Add to `src/display.rs`'s test module:

```rust
    #[test]
    fn the_primary_display_is_the_first_declared() {
        // `--cast-display` and the legacy unscoped API paths both resolve
        // through this, so which one is primary is not an implementation detail.
        let configured = configure(&args_with(vec!["foyer".into(), "werkstatt".into()])).unwrap();
        assert_eq!(configured[0].name, "foyer");
    }
```

And in `src/models.rs`'s test module (create one if absent):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_display_starts_with_nothing_playing() {
        let display = Display::new("foyer", "http://127.0.0.1:9222");
        assert_eq!(display.name, "foyer");
        assert!(display.current_item_id.try_lock().unwrap().is_none());
        assert!(display.override_item.try_lock().unwrap().is_none());
    }
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test models:: display::`

Expected: FAIL — `cannot find type Display in this scope`.

- [ ] **Step 3: Move the fields**

In `src/models.rs`, add above `AppState`:

```rust
/// One screen's playback state.
///
/// These were fields on `AppState` when there was one screen. They are the only
/// things that had to become per-display: everything else the controller owns —
/// assets, settings, the overlay, credentials, audio, webhook targets — is
/// shared, because duplicating *configuration* was never the problem.
pub struct Display {
    pub name: String,
    pub cdp_url: String,
    /// What this display's loop is currently showing. Owned by the loop; the API only reads it.
    pub current_item_id: Mutex<Option<i64>>,
    /// "Play now" for this display. See the peek-don't-take rule in CLAUDE.md.
    pub pending_jump: Mutex<Option<i64>>,
    pub override_item: Mutex<Option<OverrideItem>>,
    pub browser_pid: Mutex<Option<u32>>,
    pub skip_signal: Notify,
    pub playlist_signal: Notify,
    pub override_signal: Notify,
    pub overlay_signal: Notify,
}

impl Display {
    pub fn new(name: &str, cdp_url: &str) -> Self {
        Self {
            name: name.to_string(),
            cdp_url: cdp_url.to_string(),
            current_item_id: Mutex::new(None),
            pending_jump: Mutex::new(None),
            override_item: Mutex::new(None),
            browser_pid: Mutex::new(None),
            skip_signal: Notify::new(),
            playlist_signal: Notify::new(),
            override_signal: Notify::new(),
            overlay_signal: Notify::new(),
        }
    }
}
```

Remove `current_item_id`, `pending_jump`, `override_item`, `browser_pid`, `skip_signal`, `playlist_signal`, `override_signal` and `overlay_signal` from `AppState`, and add:

```rust
    /// The screens this deployment drives, in declaration order. Built once at
    /// startup and never changed, so nothing guards the list itself — only the
    /// fields inside each `Display`.
    pub displays: Arc<Vec<Arc<Display>>>,
```

Add the two lookups:

```rust
impl AppState {
    pub fn display(&self, name: &str) -> Option<Arc<Display>> {
        self.displays.iter().find(|d| d.name == name).cloned()
    }

    /// The first declared display. What an unscoped legacy API path resolves to
    /// when only one display exists, and what casting pins until per-display
    /// casting is built.
    pub fn primary(&self) -> Arc<Display> {
        self.displays[0].clone()
    }
}
```

`displays` is never empty — `display::configure` returns at least the implicit `default` — so the index cannot panic. Say so in a comment beside `primary`.

- [ ] **Step 4: Follow the fields to their call sites**

`cargo build` now lists every site. There are 18 of them, and each one resolves the same way: the handlers in `src/handlers.rs` take the display from the request (Task 8) or `state.primary()` until then; `src/cast.rs` uses `state.primary()` (Task 11 makes that configurable); `src/settings.rs`'s overlay poke goes to every display:

```rust
    for display in state.displays.iter() {
        display.overlay_signal.notify_one();
    }
```

**An overlay edit must reach every screen**, because the overlay configuration is global — a notice put up for the building is not put up for one panel.

- [ ] **Step 5: Build the map in main**

In `src/main.rs`, before `AppState` is constructed:

```rust
    let configured = match display::configure(&args) {
        Ok(configured) => configured,
        Err(message) => {
            error!("{}", message);
            std::process::exit(1);
        }
    };
    info!(
        "Driving {} display(s): {}",
        configured.len(),
        configured.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ")
    );
    let displays: Vec<Arc<models::Display>> = configured
        .iter()
        .map(|c| Arc::new(models::Display::new(&c.name, &c.cdp_url)))
        .collect();
```

and in the `AppState { … }` literal: `displays: Arc::new(displays),`.

A bad `--display` **exits** rather than degrading: a typo that silently dropped a screen would show up as a black panel with nothing saying why.

- [ ] **Step 6: Run everything**

Run: `cargo test && cargo build`

Expected: all pass, warning-clean.

- [ ] **Step 7: Commit**

```bash
git add src/models.rs src/main.rs src/handlers.rs src/cast.rs src/settings.rs src/display.rs
git commit -m "Give each display its own playback state"
```

---

### Task 6: One browser per display

**Files:**
- Modify: `src/chromium.rs` (`supervise` takes a display)
- Modify: `src/main.rs` (spawn one supervisor per display)

**Interfaces:**
- Consumes: `DisplayConfig`, `Display`.
- Produces: `pub async fn chromium::supervise(args: Arc<Args>, display: DisplayConfig, pid_slot: PidSlot)`.

- [ ] **Step 1: Change the signature**

`supervise` currently derives the port, the profile and the class from `args` alone. Give it the display and take all three from there:

```rust
/// Keep a browser available on this display's CDP port for its control loop.
pub async fn supervise(
    args: std::sync::Arc<Args>,
    display: crate::display::DisplayConfig,
    pid_slot: PidSlot,
) {
```

and in `spawn`, replace the derivations:

```rust
fn spawn(args: &Args, display: &crate::display::DisplayConfig) -> Result<Child> {
    let executable = detect_executable(args.chromium.as_deref())?;
    let port = debugging_port(&display.cdp_url)?;
    let profile = display.user_data_dir.clone();
    // …existing flag assembly, with:
    //   --remote-debugging-port={port}
    //   --user-data-dir={profile}
    //   --class={display.window_class}
```

**`--chromium-class` and `--chromium-user-data-dir` keep pinning their values**, exactly as today — but with several displays declared they would pin *all* of them to the same value, which is the one way to make two browsers collide. Refuse that combination in `display::configure`:

```rust
    if args.display.len() > 1 && (args.chromium_class.is_some() || args.chromium_user_data_dir.is_some()) {
        return Err(
            "--chromium-class und --chromium-user-data-dir gelten für alle Displays und \
             würden sie kollidieren lassen. Mit mehreren --display weglassen."
                .to_string(),
        );
    }
```

Add a test for it in `src/display.rs`:

```rust
    #[test]
    fn a_pinned_class_collides_with_several_displays() {
        let mut args = args_with(vec!["a".into(), "b".into()]);
        args.chromium_class = Some("fest".into());
        assert!(configure(&args).is_err());
        // One display is fine: there is nothing to collide with.
        let mut single = args_with(vec![]);
        single.chromium_class = Some("fest".into());
        assert!(configure(&single).is_ok());
    }
```

- [ ] **Step 2: Spawn one per display**

In `src/main.rs`, replace the single supervisor block:

```rust
    // Keep a browser alive on each display's CDP port. Skipped when something
    // else manages them (an existing sway `exec` line), which the supervisor
    // detects by finding the port already answering.
    if !args.no_launch_browser {
        for (config, display) in configured.iter().zip(state.displays.iter()) {
            let browser_args = state.args.clone();
            let config = config.clone();
            let pid_slot = display.browser_pid.clone();
            tokio::spawn(async move { chromium::supervise(browser_args, config, pid_slot).await });
        }
    }
```

`browser_pid` is now a field on `Display`, so `pid_slot` is `Arc<Mutex<Option<u32>>>` taken from there — adjust `Display::browser_pid` to `PidSlot` (`Arc<Mutex<Option<u32>>>`) rather than a bare `Mutex` so this clone works.

- [ ] **Step 3: Build and check by hand**

Run: `cargo build`

Then, with a compositor available:

```bash
cargo run --quiet -- --display foyer --display werkstatt --database-path /tmp/two.db \
  --disable-cast --managed-cert off --port 3999 &
sleep 12
curl -s http://127.0.0.1:9222/json/version | head -c 40; echo
curl -s http://127.0.0.1:9223/json/version | head -c 40; echo
kill %1
```

Expected: both ports answer. Under sway, `swaymsg -t get_tree` should show two windows with `app_id` `miniclientcontrol-foyer` and `miniclientcontrol-werkstatt` — that is the property the whole placement story rests on, so confirm it rather than assuming.

- [ ] **Step 4: Commit**

```bash
git add src/chromium.rs src/main.rs src/display.rs
git commit -m "Supervise one browser per display"
```

---

### Task 7: One control loop per display

The risky task. The loop owns what is on screen, and every invariant in [CLAUDE.md](../../../CLAUDE.md#the-control-loop-srcbrowserrs) applies unchanged — per loop.

**Files:**
- Modify: `src/browser.rs`
- Modify: `src/main.rs` (spawn one loop per display)

**Interfaces:**
- Consumes: `Display`, `AppState::displays`.
- Produces: `pub async fn browser_loop(state: AppState, display: Arc<Display>)`.

- [ ] **Step 1: Thread the display through**

```rust
pub async fn browser_loop(state: AppState, display: std::sync::Arc<crate::models::Display>) {
    info!("Starting browser loop for display '{}'", display.name);
```

Then replace, throughout the file:

| was | becomes |
|---|---|
| `state.args.cdp_url` | `display.cdp_url` |
| `state.current_item_id` | `display.current_item_id` |
| `state.pending_jump` | `display.pending_jump` |
| `state.override_item` | `display.override_item` |
| `state.skip_signal` | `display.skip_signal` |
| `state.playlist_signal` | `display.playlist_signal` |
| `state.override_signal` | `display.override_signal` |
| `state.overlay_signal` | `display.overlay_signal` |

The edge-tracking locals `announced_empty` and `connected_before` stay function-locals, which is what makes them per-display for free.

Every webhook `fire` in this file gains the display name (Task 10 adds the envelope key; until then pass `display.name.clone()` into the event and let it be unused).

- [ ] **Step 2: Filter the playlist by the assigned playlist**

The loop's playlist query currently reads every item. Give it this display's assignment, read fresh each pass — an operator can reassign while it runs, and the snapshot is already documented as stale by design:

```rust
            // Read per pass, not cached: an operator reassigning a playlist
            // expects the next item to come from the new one, and this query is
            // trivial against a table with single-digit rows.
            let assigned: Option<i64> = sqlx::query_scalar(
                "SELECT playlist_id FROM displays WHERE name = ?",
            )
            .bind(&display.name)
            .fetch_optional(&state.pool)
            .await
            .unwrap_or(None)
            .flatten();
```

and add to the existing `SELECT`'s `WHERE`:

```sql
                                    AND p.playlist_id = ?
```

binding `assigned`. **A display with no playlist assigned shows the idle screen**: when `assigned` is `None`, skip the query entirely and take the existing empty-playlist branch, so an unassigned screen looks deliberate rather than broken.

- [ ] **Step 3: Spawn one loop per display**

In `src/main.rs`, replace the single spawn:

```rust
    // 4. One control loop per display. Each owns its own screen; nothing is
    // shared between them but the database and the settings.
    for display in state.displays.iter() {
        let loop_state = state.clone();
        let loop_display = display.clone();
        tokio::spawn(async move {
            browser_loop(loop_state, loop_display).await;
        });
    }
```

- [ ] **Step 4: Build and run the existing suites**

Run: `cargo build && cargo test`

Then, with no local instance running:

```bash
python3 tests/cast/test_browser.py
python3 tests/cast/test_overlay.py
python3 tests/cast/test_webhook.py
```

These three drive a real browser through the loop and are the regression net for this task. Expected: `ALL PASSED` for each. If `test_overlay.py` fails on the overlay reaching the item on screen, check that `settings.rs` pokes **every** display's `overlay_signal` (Task 5, step 4).

- [ ] **Step 5: Commit**

```bash
git add src/browser.rs src/main.rs
git commit -m "Run one control loop per display"
```

---

### Task 8: Display-scoped API

**Files:**
- Modify: `src/display.rs` (add `routes()` and the handlers)
- Modify: `src/handlers.rs` (`get_current`, `set_current`, `get_override`, `set_override`, `clear_override` resolve a display)
- Modify: `src/main.rs` (merge the routes)

**Interfaces:**
- Produces: `GET /api/displays`, `PUT /api/displays/{name}`, `GET|POST /api/displays/{name}/control/current`, `GET|POST|DELETE /api/displays/{name}/override`; `pub fn resolve(state: &AppState, name: Option<&str>) -> Result<Arc<Display>, Response>`.

- [ ] **Step 1: Write the failing test**

Add to `src/display.rs`'s test module:

```rust
    #[test]
    fn an_unscoped_path_resolves_only_while_one_display_exists() {
        // With one display there is no ambiguity to report.
        assert!(unscoped_is_ambiguous(1) == false);
        // With two, picking one would be a coin flip an operator's script
        // cannot see, so the request is refused and told the names instead.
        assert!(unscoped_is_ambiguous(2));
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test display::`

Expected: FAIL — `cannot find function unscoped_is_ambiguous`.

- [ ] **Step 3: Write the resolver and the routes**

Add to `src/display.rs`:

```rust
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::json;
use std::sync::Arc;

use crate::models::{AppState, Display};

/// Whether an unscoped legacy path has to refuse.
pub fn unscoped_is_ambiguous(display_count: usize) -> bool {
    display_count > 1
}

/// Resolve the display a request is about.
///
/// `None` is the legacy unscoped path. It resolves while one display exists and
/// refuses once several do — picking one would be a coin flip an operator's
/// existing script cannot see, and a screen changing on its own is exactly the
/// failure this project treats as worst.
pub fn resolve(state: &AppState, name: Option<&str>) -> Result<Arc<Display>, Response> {
    match name {
        Some(name) => state.display(name).ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": format!("Unbekanntes Display '{name}'."),
                    "displays": state.displays.iter().map(|d| d.name.clone()).collect::<Vec<_>>(),
                })),
            )
                .into_response()
        }),
        None if unscoped_is_ambiguous(state.displays.len()) => Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "Mehrere Displays: bitte /api/displays/<name>/… verwenden.",
                "displays": state.displays.iter().map(|d| d.name.clone()).collect::<Vec<_>>(),
            })),
        )
            .into_response()),
        None => Ok(state.primary()),
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/displays", get(list))
        .route("/api/displays/{name}", axum::routing::put(update))
        .route(
            "/api/displays/{name}/control/current",
            get(crate::handlers::get_current_for).post(crate::handlers::set_current_for),
        )
        .route(
            "/api/displays/{name}/override",
            get(crate::handlers::get_override_for)
                .post(crate::handlers::set_override_for)
                .delete(crate::handlers::clear_override_for),
        )
}

async fn list(State(state): State<AppState>) -> Response {
    let rows = sqlx::query_as::<_, (String, Option<String>, Option<i64>)>(
        "SELECT name, label, playlist_id FROM displays",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_else(|e| {
        tracing::error!("Failed to list displays: {}", e);
        Vec::new()
    });

    // Driven off the declared displays, not off the table: a row for a screen
    // this deployment no longer declares must still be visible (so its playlist
    // can be reassigned) but must not claim to be attached.
    let out: Vec<_> = state
        .displays
        .iter()
        .map(|d| {
            let stored = rows.iter().find(|(name, _, _)| name == &d.name);
            json!({
                "name": d.name,
                "label": stored.and_then(|(_, label, _)| label.clone()).unwrap_or_else(|| d.name.clone()),
                "playlist_id": stored.and_then(|(_, _, id)| *id),
                "declared": true,
            })
        })
        .chain(rows.iter().filter(|(name, _, _)| state.display(name).is_none()).map(
            |(name, label, playlist_id)| {
                json!({
                    "name": name,
                    "label": label.clone().unwrap_or_else(|| name.clone()),
                    "playlist_id": playlist_id,
                    "declared": false,
                })
            },
        ))
        .collect();
    Json(out).into_response()
}

#[derive(serde::Deserialize)]
struct UpdateDisplay {
    label: Option<String>,
    #[serde(default, deserialize_with = "crate::handlers::double_option")]
    playlist_id: Option<Option<i64>>,
}

async fn update(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(payload): Json<UpdateDisplay>,
) -> Response {
    // Upsert: the row may not exist yet for a display declared after the
    // database was created.
    if let Err(e) = sqlx::query("INSERT INTO displays (name) VALUES (?) ON CONFLICT(name) DO NOTHING")
        .bind(&name)
        .execute(&state.pool)
        .await
    {
        tracing::error!("Failed to ensure display row for {}: {}", name, e);
    }
    if let Some(label) = &payload.label {
        let _ = sqlx::query("UPDATE displays SET label = ? WHERE name = ?")
            .bind(label.trim())
            .bind(&name)
            .execute(&state.pool)
            .await;
    }
    if let Some(playlist_id) = payload.playlist_id {
        let _ = sqlx::query("UPDATE displays SET playlist_id = ? WHERE name = ?")
            .bind(playlist_id)
            .bind(&name)
            .execute(&state.pool)
            .await;
        // The loop re-reads its assignment every pass, but poking it means the
        // change lands on the next item rather than at the end of this one.
        if let Some(display) = state.display(&name) {
            display.playlist_signal.notify_one();
        }
    }
    Json(json!({ "ok": true })).into_response()
}
```

`playlist_id` uses `double_option` so a JSON `null` clears the assignment rather than collapsing to "not given" — the same trap `start_date` already documents in `src/handlers.rs`. Make `double_option` `pub(crate)` if it is not already.

- [ ] **Step 4: Split the playback handlers**

In `src/handlers.rs`, each of the five handlers becomes a thin pair: a `_for` variant taking a `Path<String>`, and the legacy one resolving through `display::resolve(state, None)`. For `get_current`:

```rust
pub async fn get_current_for(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    match crate::display::resolve(&state, Some(&name)) {
        Ok(display) => current_of(&display).await,
        Err(response) => response,
    }
}

pub async fn get_current(State(state): State<AppState>) -> Response {
    match crate::display::resolve(&state, None) {
        Ok(display) => current_of(&display).await,
        Err(response) => response,
    }
}

async fn current_of(display: &crate::models::Display) -> Response {
    let current = *display.current_item_id.lock().await;
    Json(CurrentItemResponse { item_id: current }).into_response()
}
```

Do the same for `set_current`, `get_override`, `set_override` and `clear_override`: one `_of` function holding the behaviour, two entry points differing only in how they resolve the display. Duplicating the body instead would let the two drift, and the legacy path is exactly the one nobody tests by hand.

Add `.merge(display::routes())` in `src/main.rs`.

- [ ] **Step 5: Build, test, check by hand**

Run: `cargo test && cargo build`

```bash
cargo run --quiet -- --display foyer --display werkstatt --database-path /tmp/two.db \
  --no-launch-browser --disable-cast --managed-cert off --port 3999 &
sleep 5
curl -s localhost:3999/api/displays
curl -s localhost:3999/api/control/current
curl -s localhost:3999/api/displays/foyer/control/current
curl -s localhost:3999/api/displays/kueche/control/current
kill %1
```

Expected: two displays listed; the unscoped path answers `409` naming both; the scoped one answers `{"item_id":null}`; an unknown name answers `404` listing the real ones.

- [ ] **Step 6: Commit**

```bash
git add src/display.rs src/handlers.rs src/main.rs
git commit -m "Scope playback and override to a display"
```

---

### Task 9: The displays page

**Files:**
- Create: `web/displays.html`
- Modify: `web/admin.html` (link it)

**Interfaces:**
- Consumes: `GET /api/displays`, `PUT /api/displays/{name}`, `GET /api/playlists`.

- [ ] **Step 1: Write the page**

Create `web/displays.html`. The `<style>` block, the `el()` helper and the
`flash()`/`readError()` pair are lifted from `web/webhooks.html` so the three
operator pages share one set of conventions rather than three.

```html
<!doctype html>
<html lang="de">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <title>Displays</title>
  <style>
    :root {
      --line: #d8d8d8; --muted: #666; --accent: #0b6b3a;
      --warn: #8a5a00; --warn-bg: #fff5e0; --err: #a11;
    }
    * { box-sizing: border-box; }
    body {
      font-family: system-ui, sans-serif; max-width: 1000px;
      margin: 0 auto 4rem; padding: 0 1rem; line-height: 1.45;
    }
    h1 { margin-bottom: .2rem; }
    nav { margin: 0 0 1rem; color: var(--muted); }
    .muted { color: var(--muted); }
    .err { color: var(--err); }
    .card {
      border: 1px solid var(--line); border-radius: 6px;
      padding: .8rem; margin: .8rem 0;
    }
    .card.dirty { border-color: var(--warn); background: var(--warn-bg); }
    .card.gone { border-style: dashed; }
    .grid { display: flex; flex-wrap: wrap; gap: .6rem 1rem; align-items: flex-end; }
    .f { display: flex; flex-direction: column; gap: .15rem; }
    .f span { font-size: .8rem; color: var(--muted); }
    .name { font-family: ui-monospace, monospace; font-size: .85rem; color: var(--muted); }
    .badge {
      display: inline-block; font-size: .75rem; color: var(--warn);
      border: 1px solid var(--warn); border-radius: 10px; padding: .05rem .5rem;
    }
    .note { font-size: .85rem; color: var(--muted); margin: .4rem 0 0; }
    .actions { display: flex; gap: .5rem; align-items: center; margin-top: .6rem; }
    .feedback { font-size: .85rem; }
    button.primary {
      background: var(--accent); color: #fff; border: 0;
      padding: .35rem .8rem; border-radius: 4px; cursor: pointer;
    }
    button { padding: .35rem .7rem; cursor: pointer; }
  </style>
</head>
<body>
  <h1>Displays</h1>
  <nav><a href="/admin.html">Verwaltung</a> · <a href="/playlist.html">Playlist</a> · <a href="/assets.html">Assets</a> · <a href="/webhooks.html">Webhooks</a></nav>
  <p class="muted">
    Welche Playlist auf welchem Schirm läuft. Welche Schirme es gibt, entscheidet
    das Deployment über <code>--display</code>; die Fensterplatzierung macht der
    Fenstermanager.
  </p>

  <p class="err" id="loadError" hidden>Displays konnten nicht geladen werden – Server nicht erreichbar.</p>
  <div id="list"></div>
  <p class="muted" id="emptyHint" hidden>Keine Displays.</p>

  <script>
    // Every node is built with createElement/textContent. A label and a playlist
    // name are both operator-supplied, so string-interpolated innerHTML would be
    // an injection sink.
    const el = (tag, props = {}, ...children) => {
      const node = document.createElement(tag);
      for (const [k, v] of Object.entries(props)) {
        if (k === 'class') node.className = v;
        else if (k === 'text') node.textContent = v;
        else if (k.startsWith('on')) node.addEventListener(k.slice(2), v);
        else if (v !== null && v !== undefined && v !== false) node[k] = v;
      }
      for (const c of children) if (c) node.append(c);
      return node;
    };

    const field = (labelText, input) =>
      el('label', { class: 'f' }, el('span', { text: labelText }), input);

    // Lifted from playlist.html so all the operator pages report the same way:
    // a success message clears itself, an error one sits until the next attempt.
    function flash(node, message, isError) {
      node.textContent = message;
      node.className = isError ? 'feedback err' : 'feedback';
      if (!isError) setTimeout(() => { if (node.textContent === message) node.textContent = ''; }, 2500);
    }

    async function readError(res, fallback) {
      try {
        const body = await res.json();
        if (body && body.error) return body.error;
      } catch (_) { /* not a JSON error body */ }
      return fallback;
    }

    let PLAYLISTS = [];
    // Cards the operator has touched since their last save. Skipped by the poll,
    // because a card holds the label input and the playlist dropdown.
    const dirty = new Set();
    // name -> the card's DOM node, so the poll can update one line inside it.
    const cards = new Map();

    function buildCard(display) {
      const card = el('div', { class: display.declared ? 'card' : 'card gone' });
      const markDirty = () => { dirty.add(display.name); card.classList.add('dirty'); };

      const label = el('input', {
        value: display.label || display.name, style: 'width:200px', oninput: markDirty,
      });

      // The declared name is the identity the assignment is stored against.
      // Shown, never editable: renaming it here would orphan the assignment
      // without saying so.
      const name = el('span', { class: 'name', text: display.name });

      const playlist = el('select', { onchange: markDirty },
        el('option', { value: '', text: '(keine)' }),
        ...PLAYLISTS.map((list) => el('option', {
          value: String(list.id),
          text: `${list.name} (${list.items})`,
          selected: display.playlist_id === list.id,
        })));

      const feedback = el('span', { class: 'feedback' });

      const save = async () => {
        flash(feedback, 'Speichern…', false);
        let response;
        try {
          response = await fetch(`/api/displays/${encodeURIComponent(display.name)}`, {
            method: 'PUT',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({
              label: label.value,
              // null, not omitted: the server distinguishes "clear it" from
              // "not given", so an empty selection has to travel as null.
              playlist_id: playlist.value === '' ? null : Number(playlist.value),
            }),
          });
        } catch (_) {
          flash(feedback, 'Server nicht erreichbar.', true);
          return;
        }
        if (!response.ok) {
          flash(feedback, await readError(response, 'Fehler beim Speichern.'), true);
          return;
        }
        dirty.delete(display.name);
        card.classList.remove('dirty');
        flash(feedback, 'Gespeichert.', false);
      };

      card.append(
        el('div', { class: 'grid' },
          field('Name', label),
          field('Playlist', playlist),
          el('div', { class: 'f' }, el('span', { text: 'Bezeichner' }), name),
          display.declared ? null : el('span', { class: 'badge', text: 'nicht deklariert' })),
        display.declared ? null : el('p', {
          class: 'note',
          text: 'Dieses Display ist im Deployment nicht mehr deklariert. Seine '
              + 'Playlist bleibt erhalten und kann einem anderen Display '
              + 'zugewiesen werden.',
        }),
        el('div', { class: 'actions' },
          el('button', { type: 'button', class: 'primary', text: 'Speichern', onclick: save }),
          feedback));

      cards.set(display.name, card);
      return card;
    }

    async function loadAll() {
      const list = document.getElementById('list');
      let displays;
      try {
        const [pl, dp] = await Promise.all([
          fetch('/api/playlists'),
          fetch('/api/displays'),
        ]);
        if (!pl.ok || !dp.ok) throw new Error('nicht erreichbar');
        PLAYLISTS = await pl.json();
        displays = await dp.json();
      } catch (_) {
        // A failed fetch must not read as "nothing configured": the hint below
        // is an affirmative statement and would be a lie here.
        document.getElementById('loadError').hidden = false;
        document.getElementById('emptyHint').hidden = true;
        return;
      }
      document.getElementById('loadError').hidden = true;
      list.replaceChildren();
      cards.clear();
      for (const display of displays) list.append(buildCard(display));
      document.getElementById('emptyHint').hidden = displays.length > 0;
    }

    // Nothing on this page changes on its own except whether a display is still
    // declared, so the poll reloads only while no card is being edited. A card
    // is the edit form; re-rendering one eats what is being typed.
    async function poll() {
      if (dirty.size > 0) return;
      await loadAll();
    }

    (async () => {
      await loadAll();
      setInterval(poll, 2000);
    })();
  </script>
</body>
</html>
```

Two choices in there are deliberate and worth keeping:

- **`playlist_id` travels as `null`, not omitted**, because the server uses
  `double_option` to tell "clear the assignment" from "not given". Omitting it
  would make `(keine)` silently do nothing.
- **The poll reloads the whole list, but only while nothing is dirty.** This page
  has no per-card status line to update in place the way `webhooks.html` does, so
  the cheap correct thing is to skip the reload entirely while an edit is open.


- [ ] **Step 2: Link it**

In `web/admin.html`, beside the Playlist/Assets/Webhooks links: `<a class="button" href="/displays.html">Displays</a>`.

- [ ] **Step 3: Rebuild and check by hand**

Run: `cargo build` — the page is embedded by `include_dir!`.

Start with `--display foyer --display werkstatt`, open `/displays.html`, and confirm: both appear; assigning a playlist persists across a reload; `(keine)` clears it; a label survives three poll cycles while being typed; and starting with only `--display foyer` afterwards shows `werkstatt` as `nicht deklariert` with its playlist still assigned.

- [ ] **Step 4: Commit**

```bash
git add web/displays.html web/admin.html
git commit -m "Assign a playlist to a display"
```

---

### Task 10: Webhooks name the display

**Files:**
- Modify: `src/webhook/mod.rs` (`envelope`, `Dispatcher::fire`)
- Modify: `src/webhook/api.rs` (`events_payload`)
- Modify: `src/browser.rs`, `src/cast.rs`, `src/handlers.rs` (pass the display name)

**Interfaces:**
- Produces: `envelope(event, device, display, test)`; `Dispatcher::fire(&self, display: &str, event: Event)`.

- [ ] **Step 1: Write the failing test**

Add to `src/webhook/mod.rs`'s test module:

```rust
    #[test]
    fn the_envelope_names_the_display() {
        let value = envelope(&Event::PlaylistEmpty, "kiosk-pi-1", "werkstatt", false);
        assert_eq!(value["device"], "kiosk-pi-1");
        assert_eq!(value["display"], "werkstatt");
        // Additive: a target configured before several displays existed keeps
        // receiving everything it received before, in the same shape.
        assert_eq!(value["event"], "playback.playlist_empty");
        assert!(value["timestamp"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn the_catalogue_offers_display_as_a_placeholder() {
        let payload = crate::webhook::api::events_payload();
        let envelope: Vec<&str> = payload["envelope"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            envelope.contains(&"display"),
            "the catalogue is the only source the admin page has: {envelope:?}"
        );
    }
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test webhook::`

Expected: FAIL — `envelope` takes 3 arguments, not 4.

- [ ] **Step 3: Add the key**

In `envelope`, add `display: &str` after `device` and put it in the object beside `device`. In `Dispatcher`, `fire` takes the display name and passes it through; `deliver_one` takes one too (the test-send endpoint passes `state.primary().name`).

In `src/webhook/api.rs::events_payload`, add `"display"` to the `envelope` array — the catalogue is the only source the admin page has, so a placeholder the server sends and the catalogue omits is exactly the drift it exists to prevent.

Update every `fire` call site to pass its display's name: `browser.rs` has `display.name`, `handlers.rs` resolves one, `cast.rs` uses the cast display (Task 11).

- [ ] **Step 4: Run everything**

Run: `cargo test && cargo build && python3 tests/cast/test_webhook.py`

Expected: all pass. `test_webhook.py`'s existing assertions are unaffected because the key is additive; add one case asserting a delivery carries `display` naming the right screen.

- [ ] **Step 5: Commit**

```bash
git add src/webhook src/browser.rs src/cast.rs src/handlers.rs tests/cast/test_webhook.py
git commit -m "Say which display a webhook event is about"
```

---

### Task 11: Casting picks a display

**Files:**
- Modify: `src/models.rs` (the `--cast-display` arg)
- Modify: `src/cast.rs` (`activate_display`/`deactivate_display` take the display)
- Modify: `src/display.rs` (validate the flag)

**Interfaces:**
- Produces: `AppState::cast_display(&self) -> Arc<Display>`.

- [ ] **Step 1: Add the flag**

In `src/models.rs`:

```rust
    /// Which declared display a cast pins. Defaults to the first declared.
    ///
    /// One session for the whole controller, on one screen: casting to a chosen
    /// screen is its own piece of work, because `cast.rs` carries the session
    /// state machine and rebuilding it alongside the control loop would blur the
    /// task boundaries that make review effective.
    #[arg(long, env = "CAST_DISPLAY")]
    pub cast_display: Option<String>,
```

Validate it in `display::configure` — an unknown name must fail at startup, not when the first guest scans a QR code:

```rust
    if let Some(wanted) = &args.cast_display {
        if !out.iter().any(|d| &d.name == wanted) {
            return Err(format!(
                "--cast-display '{wanted}' ist kein deklariertes Display."
            ));
        }
    }
```

with a test:

```rust
    #[test]
    fn an_unknown_cast_display_fails_at_startup() {
        let mut args = args_with(vec!["foyer".into()]);
        args.cast_display = Some("kueche".into());
        assert!(configure(&args).is_err());
        args.cast_display = Some("foyer".into());
        assert!(configure(&args).is_ok());
    }
```

- [ ] **Step 2: Add the lookup**

In `src/models.rs`:

```rust
    /// The display a cast pins. `--cast-display` when given, else the first
    /// declared; the flag is validated at startup so this cannot miss.
    pub fn cast_display(&self) -> Arc<Display> {
        self.args
            .cast_display
            .as_deref()
            .and_then(|name| self.display(name))
            .unwrap_or_else(|| self.primary())
    }
```

- [ ] **Step 3: Thread it through cast.rs**

`activate_display` and `deactivate_display` currently reach for the global override. Give them `state.cast_display()` and use that display's `override_item` and `override_signal`. This is a parameter change, not a redesign: a cast is still an override, it just now says whose.

Every rule in [CLAUDE.md](../../../CLAUDE.md#casting-srccast-srctlsrs) still holds — the still-ours check on teardown, `cast_announced` gating `cast.ended`, and the grace periods.

- [ ] **Step 4: Run the cast suites**

Run: `cargo test && cargo build`

Then, with no local instance running:

```bash
python3 tests/cast/test_cast.py
python3 tests/cast/test_pairing.py
python3 tests/cast/test_guestpage.py
python3 tests/cast/test_reserve.py
python3 tests/cast/test_conflict.py
```

Expected: `ALL PASSED` for each. These are the regression net for `cast.rs` and this task has no business changing their outcome.

- [ ] **Step 5: Commit**

```bash
git add src/models.rs src/cast.rs src/display.rs
git commit -m "Pin a cast to a chosen display"
```

---

### Task 12: The integration suite

**Files:**
- Create: `tests/cast/test_display.py`

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Write the suite**

Follow `tests/cast/test_webhook.py` closely — it is the newest suite and solved the same problems. In particular reuse its shape for:

- a `Display(Server)` subclass declaring `--display` and pointing at a real Chrome, and an `Alone(Server)` on a dead CDP port for the cases that need no control loop
- `LONG = 600` on any item whose count is asserted, so the playlist cannot loop and inflate it
- a positive barrier before every negative assertion

**It needs a real Chrome per declared display**, for the same reason `test_webhook.py` does: `playback.*` comes from the control loop and the loop does not run without a CDP connection. Start two, on 9242 and 9243.

Cases, continuing the numbering after `test_webhook.py`'s `[68]`:

- `[70]` two displays with different playlists each play their own; assert via `GET /api/displays/{name}/control/current` that the two report different item ids
- `[71]` two displays assigned the **same** playlist both play it — mirroring, which is the case the playlist-as-object model exists to make free
- `[72]` a display with no playlist assigned reports `item_id: null` and logs no error
- `[73]` reassigning a playlist while it plays takes effect, asserted by the item id changing to one from the new playlist
- `[74]` an override on one display leaves the other's `current_item_id` alone
- `[75]` the unscoped `/api/control/current` answers `409` naming both displays; with one declared it answers normally
- `[76]` a webhook delivery carries `display` naming the screen whose item changed
- `[77]` a playlist holding items cannot be deleted, and the refusal names the count

- [ ] **Step 2: Run it**

Run: `python3 tests/cast/test_display.py`

Iterate until every case passes. **If a case fails because the code is wrong rather than the test, stop and report** — that is a genuine finding.

- [ ] **Step 3: Prove the important cases can fail**

Mutation-check `[70]` and `[75]`: make both loops read the same playlist regardless of assignment, confirm `[70]` fails; make `resolve` return `primary()` instead of refusing, confirm `[75]` fails. Restore both and report the output. A test that cannot fail reads as coverage and is not.

- [ ] **Step 4: Run the neighbours**

```bash
python3 tests/cast/test_cast.py
python3 tests/cast/test_browser.py
python3 tests/cast/test_overlay.py
python3 tests/cast/test_webhook.py
python3 tests/cast/test_settings.py
```

Expected: unchanged results.

- [ ] **Step 5: Commit**

```bash
git add tests/cast/test_display.py
git commit -m "Test several displays end to end"
```

---

### Task 13: Documentation

**Files:**
- Modify: `README.md`, `docs/features.md`, `docs/architecture.md`, `docs/deployment.md`, `CLAUDE.md`, `tests/cast/README.md`
- Modify: `docs/superpowers/specs/2026-09-12-multi-display-design.md` (Status)

- [ ] **Step 1: README**

Rewrite *Two displays on one machine* around `--display`, keeping the window-manager config because that part does not change:

```
miniclientcontrol --display foyer --display werkstatt
```
```
assign [app_id="miniclientcontrol-foyer"]     output HDMI-A-1
assign [app_id="miniclientcontrol-werkstatt"] output DP-1
```

Add playlists and displays to the API overview.

- [ ] **Step 2: docs/features.md and docs/architecture.md**

Playlists and displays as concepts; the module map gains `display.rs` and `playlists.rs`, and the control loop is described as one per display.

- [ ] **Step 3: docs/deployment.md**

Declaring displays, and **the fault-isolation trade stated plainly**: one process means one crash takes every screen, where separate controllers lost only one. The answer if it bites is a supervisor that restarts the process, not a second process.

- [ ] **Step 4: CLAUDE.md**

A `## Displays (`src/display.rs`)` section holding the rules that cost something to learn:

- **Displays are declared, not discovered**, and why: discovery was prototyped against sway 1.12 and works, but it puts compositor knowledge in the controller (`HDMI-A-1` under sway, `HDMI-1` under i3) and takes placement from the window manager. `--chromium-class` becomes the Wayland `app_id`, which is what lets the window manager tell our windows apart with no help from us.
- **One Chromium per display, not one with several windows.** Measured: 515 MB PSS for one browser with two windows against 494 MB for two browsers. Chromium's cost is per renderer. One browser would also share one `app_id` across its windows, and `Browser.setWindowBounds` does not move a window under native Wayland — the size takes effect and the position does not.
- **`--display` is the identity**, stored against the assignment. A name reaches a window-manager config and a filesystem path, so it is restricted to `[A-Za-z0-9_-]`.
- **With no `--display`, behaviour is exactly as before** — one implicit `default` using `--cdp-url` and the port-derived class. These run unattended; an upgrade must not move a CDP port.
- **An unscoped legacy API path refuses with `409` once several displays exist** rather than picking one.
- **A playlist is never deleted with a display**, which the schema enforces with `ON DELETE SET NULL` rather than leaving it to a handler.
- **`settings.rs` pokes every display's `overlay_signal`**: the overlay configuration is global, so an edit has to reach every screen.
- **Each loop re-reads its assignment every pass**, so reassigning a playlist lands on the next item.

- [ ] **Step 5: tests/cast/README.md**

Add `test_display.py` to the suites that launch their own Chrome, with its ports (9242, 9243).

- [ ] **Step 6: Mark the spec implemented**

Change `**Status:** designed` to `**Status:** implemented`.

- [ ] **Step 7: Commit**

```bash
git add README.md docs CLAUDE.md tests/cast/README.md
git commit -m "Document driving several displays"
```
