use sqlx::{Pool, Sqlite, Row};

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
            duration      INTEGER,
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

    Ok(())
}
