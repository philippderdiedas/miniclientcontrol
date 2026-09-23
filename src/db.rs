use sqlx::{Pool, Sqlite, Row};

async fn playlist_items_has(pool: &Pool<Sqlite>, column: &str) -> bool {
    sqlx::query("SELECT count(*) FROM pragma_table_info('playlist_items') WHERE name = ?")
        .bind(column)
        .fetch_one(pool)
        .await
        .map(|row| row.get::<i32, _>(0) > 0)
        .unwrap_or(false)
}

pub async fn run_migrations(pool: &Pool<Sqlite>) -> anyhow::Result<()> {
    // 1. Ensure assets table exists
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS assets (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            filename    TEXT NOT NULL,
            local_path  TEXT NOT NULL UNIQUE,
            mimetype    TEXT NOT NULL,
            duration    INTEGER DEFAULT 10,
            created_at  DATETIME DEFAULT CURRENT_TIMESTAMP
        );"
    )
    .execute(pool)
    .await?;

    // 2. Ensure playlist_items table exists
    // We include scroll_config in the initial creation.
    // Note: We use a string default for the JSON field.
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS playlist_items (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            asset_id      INTEGER,
            url           TEXT,
            play_order    INTEGER NOT NULL,
            advance       TEXT NOT NULL DEFAULT '{"on":"time","seconds":10}',
            is_enabled    BOOLEAN DEFAULT 1,
            start_date    TEXT,
            end_date      TEXT,
            keep_loaded   BOOLEAN DEFAULT 0,
            scroll_config TEXT DEFAULT '{"type":"None","options":null}',
            FOREIGN KEY(asset_id) REFERENCES assets(id) ON DELETE CASCADE
        );"#
    )
    .execute(pool)
    .await?;

    // `advance` replaced `duration`: a time *or* a number of passes. The old
    // value becomes a time, resolved exactly as the loop used to resolve it
    // (the item's own, else its asset's, else ten seconds) so every playlist
    // plays as before. One transaction, so a failure cannot leave the column
    // dropped with nothing backfilled -- and a failure is an error, because
    // every read selects `advance` and a half-migrated table blanks the screen.
    let has_advance = playlist_items_has(pool, "advance").await;
    if playlist_items_has(pool, "duration").await {
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

    // 3. Migration: Check for 'scroll_config' column
    // This allows upgrading databases created before this field existed.
    let has_scroll_config: bool = sqlx::query("SELECT count(*) FROM pragma_table_info('playlist_items') WHERE name='scroll_config'")
        .fetch_one(pool)
        .await
        .map(|row| row.get::<i32, _>(0) > 0)
        .unwrap_or(false);

    if !has_scroll_config {
        let _ = sqlx::query("ALTER TABLE playlist_items ADD COLUMN scroll_config TEXT DEFAULT '{\"type\":\"None\",\"options\":null}'")
            .execute(pool)
            .await;
    }

    let has_overlay: bool = sqlx::query("SELECT count(*) FROM pragma_table_info('playlist_items') WHERE name='overlay_config'")
        .fetch_one(pool)
        .await
        .map(|row| row.get::<i32, _>(0) > 0)
        .unwrap_or(false);

    if !has_overlay {
        // Default 'null' rather than NULL: the read paths decode this column as
        // JSON, and a real NULL there fails the whole query -- which both call
        // sites swallow into an empty playlist, blanking the screen over one row.
        let _ = sqlx::query("ALTER TABLE playlist_items ADD COLUMN overlay_config TEXT DEFAULT 'null'")
            .execute(pool)
            .await;
    }

    let has_keep_loaded: bool = sqlx::query("SELECT count(*) FROM pragma_table_info('playlist_items') WHERE name='keep_loaded'")
        .fetch_one(pool)
        .await
        .map(|row| row.get::<i32, _>(0) > 0)
        .unwrap_or(false);

    if !has_keep_loaded {
        let _ = sqlx::query("ALTER TABLE playlist_items ADD COLUMN keep_loaded BOOLEAN DEFAULT 0")
            .execute(pool)
            .await;
    }

    let has_start_date: bool = sqlx::query("SELECT count(*) FROM pragma_table_info('playlist_items') WHERE name='start_date'")
        .fetch_one(pool)
        .await
        .map(|row| row.get::<i32, _>(0) > 0)
        .unwrap_or(false);

    if !has_start_date {
        let _ = sqlx::query("ALTER TABLE playlist_items ADD COLUMN start_date TEXT")
            .execute(pool)
            .await;
    }

    let has_end_date: bool = sqlx::query("SELECT count(*) FROM pragma_table_info('playlist_items') WHERE name='end_date'")
        .fetch_one(pool)
        .await
        .map(|row| row.get::<i32, _>(0) > 0)
        .unwrap_or(false);

    if !has_end_date {
        let _ = sqlx::query("ALTER TABLE playlist_items ADD COLUMN end_date TEXT")
            .execute(pool)
            .await;
    }

    // How an image or video asset sits on the screen, and what fills the bars
    // around it. Both carry a default so the rows an older binary wrote get one
    // too -- and every read COALESCEs anyway, because a NULL in a String field
    // fails the whole playlist query and blanks the screen.
    // An editor's upload, stored at once but hidden until its bundle is applied.
    let has_pending: bool = sqlx::query("SELECT count(*) FROM pragma_table_info('assets') WHERE name='pending_changeset'")
        .fetch_one(pool)
        .await
        .map(|row| row.get::<i32, _>(0) > 0)
        .unwrap_or(false);
    if !has_pending {
        let _ = sqlx::query("ALTER TABLE assets ADD COLUMN pending_changeset INTEGER")
            .execute(pool)
            .await;
    }

    let has_fit_mode: bool = sqlx::query("SELECT count(*) FROM pragma_table_info('playlist_items') WHERE name='fit_mode'")
        .fetch_one(pool)
        .await
        .map(|row| row.get::<i32, _>(0) > 0)
        .unwrap_or(false);

    if !has_fit_mode {
        let added = sqlx::query("ALTER TABLE playlist_items ADD COLUMN fit_mode TEXT DEFAULT 'contain'")
            .execute(pool)
            .await;
        // The column default is right for an image and wrong for a PDF, which has
        // always been drawn at full width -- `contain` there would repaginate
        // every PDF a venue already has. Only here, when the column is new: after
        // this it is the operator's value, and `FitMode::default_for` decides for
        // items created from now on.
        if added.is_ok() {
            let _ = sqlx::query(
                "UPDATE playlist_items SET fit_mode = 'width'
                 WHERE asset_id IN (SELECT id FROM assets WHERE lower(mimetype) = 'application/pdf')",
            )
            .execute(pool)
            .await;
        }
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

    // 7. Runtime settings the operator can change without a restart.
    // Key/value rather than columns: these are a handful of unrelated scalars,
    // and adding one should not need another ALTER TABLE probe.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );"
    )
    .execute(pool)
    .await?;

    // Accounts and their sessions. A session stores only the SHA-256 of its
    // cookie value, so a copy of the database signs nobody in.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS users (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            name          TEXT NOT NULL UNIQUE,
            password_hash TEXT NOT NULL,
            role          TEXT NOT NULL CHECK (role IN ('admin', 'manager', 'editor')),
            disabled      BOOLEAN NOT NULL DEFAULT 0,
            created_at    DATETIME DEFAULT CURRENT_TIMESTAMP
        );"
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS sessions (
            token_hash TEXT PRIMARY KEY,
            user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            expires_at DATETIME NOT NULL
        );"
    )
    .execute(pool)
    .await?;

    // An editor's proposals: a bundle, and the requests it holds, each with a
    // snapshot of the object as it read when proposed.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS changesets (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            author_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            state        TEXT NOT NULL CHECK (state IN ('draft','submitted','applying','applied','rejected','stale','failed')),
            note         TEXT,
            created_at   DATETIME DEFAULT CURRENT_TIMESTAMP,
            submitted_at DATETIME,
            decided_by   INTEGER REFERENCES users(id) ON DELETE SET NULL,
            decided_at   DATETIME
        );"
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS change_requests (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            changeset_id INTEGER NOT NULL REFERENCES changesets(id) ON DELETE CASCADE,
            position     INTEGER NOT NULL,
            method       TEXT NOT NULL,
            path         TEXT NOT NULL,
            body         TEXT,
            placeholder  TEXT,
            before       TEXT,
            applied      BOOLEAN NOT NULL DEFAULT 0,
            result       TEXT
        );"
    )
    .execute(pool)
    .await?;

    // After the table it alters, not before: on a fresh database the probe
    // would find no table, the ALTER would fail silently, and every UPDATE
    // naming the column after it with it.
    // Whether the author has looked at a decision on their bundle since it was
    // made -- what the editor's "new decisions" count is.
    let has_seen: bool = sqlx::query("SELECT count(*) FROM pragma_table_info('changesets') WHERE name='author_seen'")
        .fetch_one(pool)
        .await
        .map(|row| row.get::<i32, _>(0) > 0)
        .unwrap_or(false);
    if !has_seen {
        let _ = sqlx::query("ALTER TABLE changesets ADD COLUMN author_seen BOOLEAN NOT NULL DEFAULT 1")
            .execute(pool)
            .await;
    }
    // A decided bundle its author has cleared from their own list; kept for the
    // reviewers' history.
    let has_hidden: bool = sqlx::query("SELECT count(*) FROM pragma_table_info('changesets') WHERE name='author_hidden'")
        .fetch_one(pool)
        .await
        .map(|row| row.get::<i32, _>(0) > 0)
        .unwrap_or(false);
    if !has_hidden {
        let _ = sqlx::query("ALTER TABLE changesets ADD COLUMN author_hidden BOOLEAN NOT NULL DEFAULT 0")
            .execute(pool)
            .await;
    }

    // 8. Webhook targets. A table rather than a settings key: these are rows
    // with independent lifetimes, and the settings KV would have to rewrite the
    // whole blob on every edit.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS webhooks (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            name         TEXT NOT NULL,
            url          TEXT NOT NULL,
            method       TEXT NOT NULL DEFAULT 'POST',
            is_enabled   BOOLEAN DEFAULT 1,
            events       TEXT DEFAULT '[]',
            headers      TEXT DEFAULT '{}',
            body         TEXT,
            insecure_tls BOOLEAN DEFAULT 0,
            created_at   DATETIME DEFAULT CURRENT_TIMESTAMP
        );"
    )
    .execute(pool)
    .await?;

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
    // `assignment_decided` separates "nobody has ever chosen a playlist for this
    // screen" from "somebody chose none". Both are `playlist_id IS NULL`, and
    // telling them apart is what lets startup carry a single-screen deployment's
    // playlist across the upgrade without ever undoing an operator's "(keine)".
    // It has to be stored rather than derived, because the only other witness --
    // "this process just inserted the row" -- dies with the process, and a power
    // loss between the insert and the assignment would strand the screen with no
    // playlist and no second chance.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS displays (
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            name                TEXT NOT NULL UNIQUE,
            label               TEXT,
            default_playlist_id INTEGER,
            assignment_decided  BOOLEAN DEFAULT 0,
            FOREIGN KEY(default_playlist_id) REFERENCES playlists(id) ON DELETE SET NULL
        );"
    )
    .execute(pool)
    .await?;

    let has_assignment_decided: bool = sqlx::query(
        "SELECT count(*) FROM pragma_table_info('displays') WHERE name='assignment_decided'",
    )
    .fetch_one(pool)
    .await
    .map(|row| row.get::<i32, _>(0) > 0)
    .unwrap_or(false);

    if !has_assignment_decided {
        // A row written before this column existed can only have come from the
        // registration that ran in the same startup, so defaulting it to 0 is
        // right: an assignment it already carries is recorded as decided by the
        // first `register` that sees it.
        let _ = sqlx::query("ALTER TABLE displays ADD COLUMN assignment_decided BOOLEAN DEFAULT 0")
            .execute(pool)
            .await;
    }

    // `playlist_id` became the *default* playlist when displays got a timetable,
    // and is named for what it is. Renamed in place, so its foreign key, its
    // `ON DELETE SET NULL` and every stored assignment carry over. `?` rather
    // than `let _`: a database left half-way would have every query in
    // `display.rs` naming a column that is not there.
    let has_old_assignment: bool = sqlx::query(
        "SELECT count(*) FROM pragma_table_info('displays') WHERE name='playlist_id'",
    )
    .fetch_one(pool)
    .await
    .map(|row| row.get::<i32, _>(0) > 0)
    .unwrap_or(false);

    if has_old_assignment {
        sqlx::query("ALTER TABLE displays RENAME COLUMN playlist_id TO default_playlist_id")
            .execute(pool)
            .await?;
    }

    // A display's timetable: ordered windows, the first match wins. Deleting a
    // playlist deletes its windows -- a window with no playlist would be a
    // setting that silently does nothing -- and deleting a display row takes its
    // timetable with it.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schedule_windows (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            display      TEXT NOT NULL REFERENCES displays(name) ON DELETE CASCADE,
            position     INTEGER NOT NULL,
            weekdays     INTEGER NOT NULL,
            start_minute INTEGER NOT NULL,
            end_minute   INTEGER NOT NULL,
            playlist_id  INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE
        );",
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
    //
    // That gate is also why the two statements below are one transaction. They
    // describe one decision, and a machine that loses power -- or meets a
    // SQLITE_BUSY -- between them must come back to either both or neither: a
    // committed playlist with the items still unmoved closes the gate for good,
    // and every item is then stranded where no loop selects it, no page lists it
    // and no API can adopt it.
    let playlists: i64 = sqlx::query_scalar("SELECT count(*) FROM playlists")
        .fetch_one(pool)
        .await
        .unwrap_or(0);
    let orphans: i64 = sqlx::query_scalar("SELECT count(*) FROM playlist_items")
        .fetch_one(pool)
        .await
        .unwrap_or(0);

    if playlists == 0 && orphans > 0 {
        let mut tx = pool.begin().await?;
        let row = sqlx::query("INSERT INTO playlists (name) VALUES ('Standard') RETURNING id")
            .fetch_one(&mut *tx)
            .await?;
        let id: i64 = row.get(0);
        sqlx::query("UPDATE playlist_items SET playlist_id = ? WHERE playlist_id IS NULL")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        tracing::info!("Moved {} existing items into the 'Standard' playlist", orphans);
    }

    Ok(())
}

pub async fn load_setting(pool: &Pool<Sqlite>, key: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await
        .unwrap_or(None)
}

pub async fn save_setting(pool: &Pool<Sqlite>, key: &str, value: &str) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(key)
        .bind(value)
        .execute(pool)
        .await?;
    Ok(())
}

/// One item's overlay, for the paths that have an id and no playlist row.
pub async fn load_item_overlay(
    pool: &sqlx::SqlitePool,
    item_id: i64,
) -> Option<crate::settings::ItemOverlay> {
    let raw: String =
        sqlx::query_scalar("SELECT COALESCE(overlay_config, 'null') FROM playlist_items WHERE id = ?")
            .bind(item_id)
            .fetch_optional(pool)
            .await
            .unwrap_or_else(|e| {
                tracing::error!("Failed to load overlay for item {}: {}", item_id, e);
                None
            })
            .flatten()?;
    serde_json::from_str::<Option<crate::settings::ItemOverlay>>(&raw)
        .ok()
        .flatten()
}

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

    /// The backfill is two statements, and the gate it runs behind closes the
    /// moment the first one lands. So a run that dies between them must leave
    /// nothing behind: a committed 'Standard' with the items still unmoved is
    /// permanent -- the next start sees a playlist, skips the backfill, and the
    /// items belong to nothing for good.
    ///
    /// The interruption is a trigger that aborts the UPDATE rather than a real
    /// power loss, because the failure mode is the same one SQLITE_BUSY has: the
    /// second statement does not run and the first must not survive it.
    #[tokio::test]
    async fn an_interrupted_backfill_leaves_the_items_adoptable() {
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
        sqlx::query(
            // `OF playlist_id`: the backfill under test, not the `advance`
            // migration, which updates the same table earlier on.
            "CREATE TRIGGER stop_the_backfill BEFORE UPDATE OF playlist_id ON playlist_items
             BEGIN SELECT RAISE(ABORT, 'interrupted'); END",
        )
        .execute(&pool)
        .await
        .unwrap();

        assert!(
            run_migrations(&pool).await.is_err(),
            "the second half of the backfill was supposed to fail"
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM playlists").await,
            0,
            "a 'Standard' that outlives the move it was created for closes the gate forever"
        );

        // The machine comes back up, with whatever stopped the write gone.
        sqlx::query("DROP TRIGGER stop_the_backfill").execute(&pool).await.unwrap();
        run_migrations(&pool).await.unwrap();

        assert_eq!(count(&pool, "SELECT count(*) FROM playlists").await, 1);
        assert_eq!(
            count(&pool, "SELECT count(*) FROM playlist_items WHERE playlist_id IS NULL").await,
            0,
            "the retry must still adopt the items the interrupted run left"
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

    #[tokio::test]
    async fn duration_becomes_a_time_to_advance() {
        // One connection: each connection to a bare `sqlite::memory:` is its own
        // database, and the migration's transaction must see the same one.
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1)
            .connect("sqlite::memory:").await.unwrap();
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
        assert!(!playlist_items_has(&pool, "duration").await, "the old column is gone");
        run_migrations(&pool).await.unwrap(); // idempotent
    }

    #[tokio::test]
    async fn an_older_pdf_item_keeps_its_full_width_layout() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:?cache=shared").await.unwrap();
        sqlx::query(
            "CREATE TABLE assets (
                id INTEGER PRIMARY KEY AUTOINCREMENT, filename TEXT NOT NULL,
                local_path TEXT NOT NULL UNIQUE, mimetype TEXT NOT NULL,
                duration INTEGER DEFAULT 10, created_at DATETIME DEFAULT CURRENT_TIMESTAMP)",
        )
        .execute(&pool)
        .await
        .unwrap();
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
        sqlx::query("INSERT INTO assets (id, filename, local_path, mimetype) VALUES
                     (1, 'a.pdf', 'a.pdf', 'application/pdf'), (2, 'b.png', 'b.png', 'image/png')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO playlist_items (asset_id, play_order) VALUES (1, 1), (2, 2)")
            .execute(&pool)
            .await
            .unwrap();

        run_migrations(&pool).await.unwrap();

        let fits: Vec<(i64, String)> =
            sqlx::query_as("SELECT asset_id, fit_mode FROM playlist_items ORDER BY asset_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(fits, vec![(1, "width".to_string()), (2, "contain".to_string())]);
    }

    #[tokio::test]
    async fn deleting_a_playlist_unassigns_it_and_deletes_its_windows() {
        let pool = pool().await;
        sqlx::query("INSERT INTO playlists (id, name) VALUES (1, 'Foyer'), (2, 'Nacht')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO displays (name, default_playlist_id) VALUES ('foyer', 1)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO schedule_windows (display, position, weekdays, start_minute, end_minute, playlist_id)
             VALUES ('foyer', 0, 127, 1320, 360, 1), ('foyer', 1, 127, 0, 1440, 2)",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("DELETE FROM playlists WHERE id = 1").execute(&pool).await.unwrap();

        let default: Option<i64> =
            sqlx::query_scalar("SELECT default_playlist_id FROM displays WHERE name = 'foyer'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(default.is_none(), "ON DELETE SET NULL did not fire -- is PRAGMA foreign_keys on?");
        let windows: Vec<i64> = sqlx::query_scalar("SELECT playlist_id FROM schedule_windows")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(windows, vec![2], "the window naming the deleted playlist must go with it");
    }

    #[tokio::test]
    async fn a_display_assigned_before_dayparting_keeps_its_playlist_as_the_default() {
        let pool = sqlx::SqlitePool::connect("sqlite:file:db_display_rename?mode=memory&cache=shared")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE playlists (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE displays (
                id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE, label TEXT,
                playlist_id INTEGER, assignment_decided BOOLEAN DEFAULT 0,
                FOREIGN KEY(playlist_id) REFERENCES playlists(id) ON DELETE SET NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO playlists (id, name) VALUES (5, 'Standard')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO displays (name, playlist_id, assignment_decided) VALUES ('left', 5, 1)")
            .execute(&pool)
            .await
            .unwrap();

        run_migrations(&pool).await.unwrap();
        run_migrations(&pool).await.unwrap();

        let default: Option<i64> =
            sqlx::query_scalar("SELECT default_playlist_id FROM displays WHERE name = 'left'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(default, Some(5));
        assert_eq!(count(&pool, "SELECT count(*) FROM schedule_windows").await, 0);
    }
}
