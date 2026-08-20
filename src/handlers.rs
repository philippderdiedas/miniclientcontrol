use axum::{
    extract::{Multipart, State, Path},
    response::{IntoResponse, Json},
    http::StatusCode,
};
use tracing::error;
use crate::models::{AppState, Asset, OverrideItem, PlaylistItemWithAsset, ScrollMode};
use serde::{Deserialize, Deserializer, Serialize};

/// Distinguishes "field absent" from "field explicitly null".
///
/// A plain `Option<Option<T>>` cannot do this: serde collapses a JSON `null` into the
/// *outer* `None`, so `Some(None)` is unreachable and a nullable field can never be
/// cleared once set.
fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

/// Playback durations are seconds and end up as `Duration::from_secs(x as u64)`.
/// A negative value would wrap to ~584 billion years and freeze the playlist on one
/// item; zero would spin the loop navigating as fast as CDP allows.
const MIN_DURATION_SECS: i64 = 1;
const MAX_DURATION_SECS: i64 = 7 * 24 * 60 * 60;

pub fn clamp_duration(secs: i64) -> i64 {
    secs.clamp(MIN_DURATION_SECS, MAX_DURATION_SECS)
}

// --- Models for Request Bodies ---
#[derive(Deserialize)]
pub struct AddToPlaylistRequest {
    pub asset_id: Option<i64>,
    pub url: Option<String>,
    pub duration: Option<i64>,
    pub enabled: Option<bool>,
    pub keep_loaded: Option<bool>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub scroll_config: Option<ScrollMode>,
    /// This item's own overlay, on top of the global one.
    pub overlay: Option<crate::settings::ItemOverlay>,
}

#[derive(Deserialize)]
pub struct UpdatePlaylistRequest {
    pub play_order: Option<i64>,
    pub duration: Option<i64>,
    pub enabled: Option<bool>,
    pub is_enabled: Option<bool>,
    pub keep_loaded: Option<bool>,
    #[serde(default, deserialize_with = "double_option")]
    pub start_date: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub end_date: Option<Option<String>>,
    pub scroll_config: Option<ScrollMode>,
    /// This item's own overlay, on top of the global one.
    pub overlay: Option<crate::settings::ItemOverlay>,
    /// Replacement URL. Only accepted for items that already are URL-backed.
    pub url: Option<String>,
    /// Replacement asset. Only accepted for items that already are asset-backed.
    pub asset_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct MovePlaylistItemRequest {
    /// `"up"` or `"down"`.
    pub direction: String,
}

#[derive(Serialize)]
pub struct ApiError {
    pub error: String,
}

fn bad_request(message: impl Into<String>) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError {
            error: message.into(),
        }),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct UpdateAssetRequest {
    pub duration: Option<i64>,
}

#[derive(Serialize)]
pub struct UploadResponse {
    pub uploaded: Vec<String>,
}

#[derive(Deserialize)]
pub struct SetCurrentItemRequest {
    pub item_id: Option<i64>,
}

#[derive(Serialize)]
pub struct CurrentItemResponse {
    pub item_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct SetOverrideRequest {
    pub asset_id: Option<i64>,
    pub url: Option<String>,
    pub scroll_config: Option<ScrollMode>,
}

#[derive(Serialize)]
pub struct OverrideResponse {
    pub active: bool,
}

/// Read model for the override, so the operator UI can show what is pinned instead of
/// only being able to set and clear it blind.
#[derive(Serialize)]
pub struct OverrideStateResponse {
    pub active: bool,
    pub asset_id: Option<i64>,
    pub url: Option<String>,
    pub scroll_config: Option<ScrollMode>,
}

// --- Handlers ---

/// Reduce a client-supplied filename to a harmless basename.
fn sanitize_filename(raw: &str) -> String {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(raw)
        .trim_matches('.')
        .trim();

    let cleaned: String = base
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect();

    if cleaned.is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

pub async fn upload_asset(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    use tokio::io::AsyncWriteExt;

    let mut uploaded_files = Vec::new();

    while let Some(mut field) = multipart.next_field().await.unwrap_or(None) {
        // Plain (non-file) form fields carry no filename; they are not assets.
        let Some(raw_filename) = field.file_name().map(|f| f.to_string()) else {
            continue;
        };
        let filename = sanitize_filename(&raw_filename);
        let content_type = field
            .content_type()
            .map(|c| c.to_string())
            .unwrap_or_else(|| {
                mime_guess::from_path(&filename)
                    .first_or_octet_stream()
                    .to_string()
            });

        let safe_filename = format!("{}_{}", uuid::Uuid::new_v4(), filename);
        let filepath = state.args.assets_dir.join(&safe_filename);

        let Ok(mut file) = tokio::fs::File::create(&filepath).await else {
            error!("Failed to create file: {:?}", filepath);
            continue;
        };

        // Stream chunk by chunk instead of buffering the whole upload (the body limit
        // is 500 MB) and drop the partial file if anything fails, so we never register
        // a truncated or empty asset in the DB.
        let mut write_failed = false;
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    if let Err(e) = file.write_all(&chunk).await {
                        error!("Write failed for {:?}: {}", filepath, e);
                        write_failed = true;
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    error!("Upload stream failed for {:?}: {}", filepath, e);
                    write_failed = true;
                    break;
                }
            }
        }

        if !write_failed {
            if let Err(e) = file.flush().await {
                error!("Flush failed for {:?}: {}", filepath, e);
                write_failed = true;
            }
        }
        drop(file);

        if write_failed {
            let _ = tokio::fs::remove_file(&filepath).await;
            continue;
        }

        // Save to DB
        let default_duration = 10;
        let created_at = chrono::Utc::now().to_rfc3339();

        let result = sqlx::query(
            "INSERT INTO assets (filename, local_path, mimetype, duration, created_at) VALUES (?, ?, ?, ?, ?) RETURNING id"
        )
        .bind(&filename)
        .bind(&safe_filename)
        .bind(&content_type)
        .bind(default_duration)
        .bind(created_at)
        .fetch_one(&state.pool)
        .await;

        match result {
            Ok(_) => uploaded_files.push(safe_filename),
            Err(e) => {
                error!("DB Insert error: {}", e);
                let _ = tokio::fs::remove_file(&filepath).await;
            }
        }
    }

    (StatusCode::OK, Json(UploadResponse { uploaded: uploaded_files }))
}

pub async fn list_assets(State(state): State<AppState>) -> impl IntoResponse {
    let assets = sqlx::query_as::<_, Asset>("SELECT * FROM assets ORDER BY created_at DESC")
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default();
    (StatusCode::OK, Json(assets))
}

pub async fn update_asset(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateAssetRequest>,
) -> impl IntoResponse {
    if let Some(duration) = payload.duration {
        if let Err(e) = sqlx::query("UPDATE assets SET duration = ? WHERE id = ?")
            .bind(clamp_duration(duration))
            .bind(id)
            .execute(&state.pool)
            .await
        {
            error!("Failed to update asset {}: {}", id, e);
        }
        // Playlist items without their own duration fall back to the asset duration.
        state.playlist_signal.notify_one();
    }
    StatusCode::OK
}

pub async fn delete_asset(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    // Get filepath first
    let asset = sqlx::query_as::<_, Asset>("SELECT * FROM assets WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await
        .unwrap_or(None);

    if let Some(asset) = asset {
        let filepath = state.args.assets_dir.join(&asset.local_path);
        let _ = tokio::fs::remove_file(filepath).await;

        // Explicit, so this also cleans up databases created before `foreign_keys`
        // was enabled (ON DELETE CASCADE was a no-op then and left orphaned rows
        // that rendered as "no content").
        if let Err(e) = sqlx::query("DELETE FROM playlist_items WHERE asset_id = ?")
            .bind(id)
            .execute(&state.pool)
            .await
        {
            error!("Failed to delete playlist items for asset {}: {}", id, e);
        }

        if let Err(e) = sqlx::query("DELETE FROM assets WHERE id = ?")
            .bind(id)
            .execute(&state.pool)
            .await
        {
            error!("Failed to delete asset {}: {}", id, e);
        }
        state.playlist_signal.notify_one();
    }
    StatusCode::OK
}

pub async fn get_playlist(State(state): State<AppState>) -> impl IntoResponse {
    // We need to join with assets.
    // scroll_config is COALESCEd: a NULL there fails to decode into Json<ScrollMode>,
    // which fails the whole query and would silently return an empty playlist.
    let items = sqlx::query_as::<_, PlaylistItemWithAsset>(
        r#"
        SELECT
            p.id, p.asset_id, p.url, p.play_order, p.duration, p.is_enabled as enabled, p.is_enabled,
            p.start_date, p.end_date,
            COALESCE(p.keep_loaded, 0) as keep_loaded,
            COALESCE(p.scroll_config, '{"type":"None","options":null}') as scroll_config,
            COALESCE(p.overlay_config, 'null') as overlay_config,
            a.local_path, a.mimetype, a.duration as asset_duration
        FROM playlist_items p
        LEFT JOIN assets a ON p.asset_id = a.id
        ORDER BY p.play_order ASC
        "#
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_else(|e| {
        error!("Failed to load playlist: {}", e);
        Vec::new()
    });

    (StatusCode::OK, Json(items))
}

pub async fn add_to_playlist(
    State(state): State<AppState>,
    Json(payload): Json<AddToPlaylistRequest>,
) -> impl IntoResponse {
    // An item with neither source silently renders as "no content" forever.
    if payload.asset_id.is_none() && payload.url.as_deref().unwrap_or("").trim().is_empty() {
        return StatusCode::BAD_REQUEST;
    }

    // Get max play_order
    let row: (i64,) = sqlx::query_as("SELECT COALESCE(MAX(play_order), 0) FROM playlist_items")
        .fetch_one(&state.pool)
        .await
        .unwrap_or((0,));
    let next_order = row.0 + 1;

    let scroll_config = payload.scroll_config.unwrap_or(ScrollMode::None);
    let keep_loaded = payload.keep_loaded.unwrap_or(false);
    let enabled = payload.enabled.unwrap_or(true);
    // `null` unless it would actually draw something, so the read paths never
    // have to tell "switched off" from "empty".
    let overlay = payload
        .overlay
        .map(|overlay| overlay.sanitized())
        .filter(|overlay| overlay.draws())
        .map(sqlx::types::Json);

    if let Err(e) = sqlx::query(
        "INSERT INTO playlist_items (asset_id, url, play_order, duration, is_enabled, keep_loaded, start_date, end_date, scroll_config, overlay_config) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(payload.asset_id)
    .bind(payload.url)
    .bind(next_order)
    .bind(payload.duration.map(clamp_duration))
    .bind(enabled)
    .bind(keep_loaded)
    .bind(payload.start_date)
    .bind(payload.end_date)
    .bind(sqlx::types::Json(scroll_config))
    .bind(overlay)
    .execute(&state.pool)
    .await
    {
        error!("Failed to add playlist item: {}", e);
        return StatusCode::INTERNAL_SERVER_ERROR;
    }

    state.playlist_signal.notify_one();

    StatusCode::CREATED
}

pub async fn update_playlist_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdatePlaylistRequest>,
) -> impl IntoResponse {
    // A source edit may only swap like for like: a URL item gets a different URL, an
    // asset item a different asset. Allowing a kind change would need the other column
    // cleared in the same write, and `playlist_target_url` silently prefers one column
    // over the other, so a half-changed row plays the wrong thing with no error.
    if payload.url.is_some() || payload.asset_id.is_some() {
        let existing = sqlx::query_as::<_, (Option<i64>, Option<String>)>(
            "SELECT asset_id, url FROM playlist_items WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&state.pool)
        .await;

        let (existing_asset_id, _existing_url) = match existing {
            Ok(Some(row)) => row,
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(ApiError {
                        error: format!("playlist item {} does not exist", id),
                    }),
                )
                    .into_response();
            }
            Err(e) => {
                error!("Failed to read playlist item {} before source edit: {}", id, e);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };

        let is_asset_item = existing_asset_id.is_some();

        if payload.url.is_some() && is_asset_item {
            return bad_request(
                "this item plays an asset; choose a different asset instead of a URL",
            );
        }
        if payload.asset_id.is_some() && !is_asset_item {
            return bad_request("this item plays a URL; edit the URL instead of choosing an asset");
        }

        if let Some(raw_url) = payload.url.as_deref() {
            let trimmed = raw_url.trim();
            if trimmed.is_empty() {
                return bad_request("URL must not be empty");
            }
            if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
                return bad_request("URL must start with http:// or https://");
            }
            if let Err(e) = sqlx::query("UPDATE playlist_items SET url = ? WHERE id = ?")
                .bind(trimmed)
                .bind(id)
                .execute(&state.pool)
                .await
            {
                error!("Failed to update url of playlist item {}: {}", id, e);
            }
        }

        if let Some(new_asset_id) = payload.asset_id {
            // Without this check a typo'd id would be written happily; the LEFT JOIN in
            // the loop's query then yields NULL local_path and the item shows nothing.
            let exists = sqlx::query_scalar::<_, i64>("SELECT id FROM assets WHERE id = ?")
                .bind(new_asset_id)
                .fetch_optional(&state.pool)
                .await;

            match exists {
                Ok(Some(_)) => {
                    if let Err(e) =
                        sqlx::query("UPDATE playlist_items SET asset_id = ? WHERE id = ?")
                            .bind(new_asset_id)
                            .bind(id)
                            .execute(&state.pool)
                            .await
                    {
                        error!("Failed to update asset of playlist item {}: {}", id, e);
                    }
                }
                Ok(None) => return bad_request(format!("asset {} does not exist", new_asset_id)),
                Err(e) => {
                    error!("Failed to check asset {}: {}", new_asset_id, e);
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            }
        }
    }

    if let Some(val) = payload.play_order {
        let _ = sqlx::query("UPDATE playlist_items SET play_order = ? WHERE id = ?").bind(val).bind(id).execute(&state.pool).await;
    }
    if let Some(val) = payload.duration {
        let _ = sqlx::query("UPDATE playlist_items SET duration = ? WHERE id = ?").bind(clamp_duration(val)).bind(id).execute(&state.pool).await;
    }
    if let Some(val) = payload.enabled {
        let _ = sqlx::query("UPDATE playlist_items SET is_enabled = ? WHERE id = ?").bind(val).bind(id).execute(&state.pool).await;
    }
    if let Some(val) = payload.is_enabled {
        let _ = sqlx::query("UPDATE playlist_items SET is_enabled = ? WHERE id = ?").bind(val).bind(id).execute(&state.pool).await;
    }
    if let Some(val) = payload.keep_loaded {
        let _ = sqlx::query("UPDATE playlist_items SET keep_loaded = ? WHERE id = ?").bind(val).bind(id).execute(&state.pool).await;
    }
    if let Some(val) = payload.start_date {
        let _ = sqlx::query("UPDATE playlist_items SET start_date = ? WHERE id = ?").bind(val).bind(id).execute(&state.pool).await;
    }
    if let Some(val) = payload.end_date {
        let _ = sqlx::query("UPDATE playlist_items SET end_date = ? WHERE id = ?").bind(val).bind(id).execute(&state.pool).await;
    }
    if let Some(val) = payload.scroll_config {
        let _ = sqlx::query("UPDATE playlist_items SET scroll_config = ? WHERE id = ?").bind(sqlx::types::Json(val)).bind(id).execute(&state.pool).await;
    }
    let mut overlay_changed = false;
    if let Some(overlay) = payload.overlay {
        let overlay = overlay.sanitized();
        if let Some(asset_id) = overlay.image_asset_id {
            // The same check as the global overlay, so a picture that works in one
            // place cannot be refused in the other.
            if let Err(message) = crate::settings::check_overlay_image(&state, asset_id).await {
                return (StatusCode::BAD_REQUEST, Json(ApiError { error: message })).into_response();
            }
        }
        // Stored as `null` when it draws nothing, so the read paths do not have to
        // tell "switched off" from "empty".
        let stored = overlay.draws().then(|| sqlx::types::Json(overlay));
        if let Err(e) = sqlx::query("UPDATE playlist_items SET overlay_config = ? WHERE id = ?")
            .bind(stored)
            .bind(id)
            .execute(&state.pool)
            .await
        {
            error!("Failed to update overlay of playlist item {}: {}", id, e);
        }
        overlay_changed = true;
    }

    state.playlist_signal.notify_one();
    // The item on screen may be this one, and its badge should not wait for the
    // next navigation. The loop re-reads the item's overlay when it re-applies.
    if overlay_changed {
        state.overlay_signal.notify_one();
    }

    StatusCode::OK.into_response()
}

/// Move an item one slot up or down and renumber the whole list.
///
/// Renumbering rather than swapping two values on purpose: `play_order` is typed by hand
/// in the UI, so duplicates and gaps accumulate, and a pairwise swap between two rows
/// that share an order does nothing visible.
pub async fn move_playlist_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<MovePlaylistItemRequest>,
) -> impl IntoResponse {
    let up = match payload.direction.as_str() {
        "up" => true,
        "down" => false,
        other => {
            return bad_request(format!(
                "direction must be \"up\" or \"down\", got {:?}",
                other
            ));
        }
    };

    let mut ids = match sqlx::query_scalar::<_, i64>(
        "SELECT id FROM playlist_items ORDER BY play_order ASC, id ASC",
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            error!("Failed to read playlist order: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let Some(pos) = ids.iter().position(|x| *x == id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: format!("playlist item {} does not exist", id),
            }),
        )
            .into_response();
    };

    let neighbour = if up {
        pos.checked_sub(1)
    } else if pos + 1 < ids.len() {
        Some(pos + 1)
    } else {
        None
    };

    // Already at the top or bottom: nothing to do, and not an error worth reporting.
    let Some(neighbour) = neighbour else {
        return StatusCode::OK.into_response();
    };

    ids.swap(pos, neighbour);

    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            error!("Failed to open transaction for reorder: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    for (offset, item_id) in ids.iter().enumerate() {
        if let Err(e) = sqlx::query("UPDATE playlist_items SET play_order = ? WHERE id = ?")
            .bind(offset as i64 + 1)
            .bind(item_id)
            .execute(&mut *tx)
            .await
        {
            error!("Failed to renumber playlist item {}: {}", item_id, e);
        }
    }

    if let Err(e) = tx.commit().await {
        error!("Failed to commit reorder: {}", e);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    state.playlist_signal.notify_one();
    StatusCode::OK.into_response()
}

pub async fn delete_playlist_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    if let Err(e) = sqlx::query("DELETE FROM playlist_items WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await
    {
        error!("Failed to delete playlist item {}: {}", id, e);
    }
    state.playlist_signal.notify_one();
    StatusCode::OK
}


pub async fn set_current(
    State(state): State<AppState>,
    Json(payload): Json<SetCurrentItemRequest>
) -> impl IntoResponse {
    // Record the request in `pending_jump`, not `current_item_id`: the browser loop
    // owns `current_item_id` and rewrites it at the start of every item, so writing
    // there races with playback and loses the click.
    {
        let mut lock = state.pending_jump.lock().await;
        *lock = payload.item_id;
    }
    // Interrupt the current wait. notify_one stores a permit if the loop is busy
    // navigating, so the request survives until the loop next awaits.
    state.skip_signal.notify_one();
    StatusCode::OK
}

pub async fn get_current(State(state): State<AppState>) -> impl IntoResponse {
    let id = {
        let lock = state.current_item_id.lock().await;
        *lock
    };
    (StatusCode::OK, Json(CurrentItemResponse { item_id: id }))
}

pub async fn get_override(State(state): State<AppState>) -> impl IntoResponse {
    let current = {
        let lock = state.override_item.lock().await;
        lock.clone()
    };

    let body = match current {
        Some(item) => OverrideStateResponse {
            active: true,
            asset_id: item.asset_id,
            url: item.url,
            scroll_config: Some(item.scroll_config),
        },
        None => OverrideStateResponse {
            active: false,
            asset_id: None,
            url: None,
            scroll_config: None,
        },
    };

    (StatusCode::OK, Json(body))
}

pub async fn set_override(
    State(state): State<AppState>,
    Json(payload): Json<SetOverrideRequest>,
) -> impl IntoResponse {
    if payload.asset_id.is_none() && payload.url.is_none() {
        return (StatusCode::BAD_REQUEST, Json(OverrideResponse { active: false })).into_response();
    }

    let mut local_path: Option<String> = None;
    let mut mimetype: Option<String> = None;

    if let Some(asset_id) = payload.asset_id {
        let asset = sqlx::query_as::<_, Asset>("SELECT * FROM assets WHERE id = ?")
            .bind(asset_id)
            .fetch_optional(&state.pool)
            .await
            .unwrap_or(None);

        let Some(asset) = asset else {
            return (StatusCode::BAD_REQUEST, Json(OverrideResponse { active: false })).into_response();
        };

        local_path = Some(asset.local_path);
        mimetype = Some(asset.mimetype);
    }

    let override_item = OverrideItem {
        asset_id: payload.asset_id,
        url: payload.url,
        local_path,
        mimetype,
        scroll_config: payload.scroll_config.unwrap_or(ScrollMode::None),
    };

    {
        let mut lock = state.override_item.lock().await;
        *lock = Some(override_item);
    }

    // Only override_signal: the browser loop watches it both while playing the
    // playlist and while an override is up. Also poking skip_signal used to leave an
    // unconsumed permit that silently cut the next playlist item short.
    state.override_signal.notify_one();

    (StatusCode::OK, Json(OverrideResponse { active: true })).into_response()
}

pub async fn clear_override(
    State(state): State<AppState>,
) -> impl IntoResponse {
    {
        let mut lock = state.override_item.lock().await;
        *lock = None;
    }

    state.override_signal.notify_one();

    (StatusCode::OK, Json(OverrideResponse { active: false }))
}