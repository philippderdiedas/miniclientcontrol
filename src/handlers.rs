use axum::{
    extract::{Multipart, State, Path},
    response::{IntoResponse, Json, Response},
    http::StatusCode,
};
use tracing::error;
use crate::models::{AppState, Asset, Display, FitMode, OverrideItem, PlaylistItemWithAsset, ScrollMode};
use serde::{Deserialize, Deserializer, Serialize};

/// Distinguishes "field absent" from "field explicitly null".
///
/// A plain `Option<Option<T>>` cannot do this: serde collapses a JSON `null` into the
/// *outer* `None`, so `Some(None)` is unreachable and a nullable field can never be
/// cleared once set.
pub(crate) fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
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

/// A length measured by the upload page, as whole seconds: rounded down, at
/// least 1. Down, because a fraction of the last second cut off is invisible
/// and a flash of the video starting over is not. `None` for anything that is
/// not a finite positive number, which leaves the default in place.
pub(crate) fn seconds_from_measured(raw: &str) -> Option<i64> {
    let value: f64 = raw.trim().parse().ok()?;
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    Some((value.floor() as i64).max(1))
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
    /// How an image or video sits on the screen, by name. A string rather than
    /// `FitMode` so an unknown name falls back instead of failing the whole
    /// request at deserialisation.
    pub fit_mode: Option<String>,
    /// What fills the bars around a contained asset. Must be a hex colour.
    pub fit_background: Option<String>,
    /// Which playlist the item joins. Required: an item with no playlist is one
    /// no screen will ever play, and nothing would say so.
    pub playlist_id: i64,
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
    /// How an image or video sits on the screen, by name. A string rather than
    /// `FitMode` so an unknown name falls back instead of failing the whole
    /// request at deserialisation.
    pub fit_mode: Option<String>,
    /// What fills the bars around a contained asset. Must be a hex colour.
    pub fit_background: Option<String>,
    /// Replacement URL. Only accepted for items that already are URL-backed.
    pub url: Option<String>,
    /// Replacement asset. Only accepted for items that already are asset-backed.
    pub asset_id: Option<i64>,
    /// Move the item into another playlist. Sent on its own -- see the rule at
    /// the top of `update_playlist_item`.
    ///
    /// `double_option` here is not an offer to clear the field but the only way
    /// to *refuse* clearing it: with a plain `Option<i64>` a JSON `null`
    /// collapses into the outer `None`, which is indistinguishable from "field
    /// absent", so a client asking for no playlist would be answered `200`
    /// having changed nothing. An item with no playlist is the broken state this
    /// field exists to repair -- `POST /api/playlist` requires one for exactly
    /// that reason -- so `null` is an error, not a way back into it.
    #[serde(default, deserialize_with = "double_option")]
    pub playlist_id: Option<Option<i64>>,
}

impl UpdatePlaylistRequest {
    /// Whether this request asks for anything besides the move.
    ///
    /// Destructured field by field rather than tested with a handful of
    /// `is_some()` calls: a field added to the struct later and forgotten here
    /// would silently become a combination the "a move stands alone" rule is
    /// meant to refuse, and this way it fails to compile instead.
    fn edits_besides_the_playlist(&self) -> bool {
        let Self {
            play_order,
            duration,
            enabled,
            is_enabled,
            keep_loaded,
            start_date,
            end_date,
            scroll_config,
            overlay,
            fit_mode,
            fit_background,
            url,
            asset_id,
            playlist_id: _,
        } = self;

        play_order.is_some()
            || duration.is_some()
            || enabled.is_some()
            || is_enabled.is_some()
            || keep_loaded.is_some()
            || start_date.is_some()
            || end_date.is_some()
            || scroll_config.is_some()
            || overlay.is_some()
            || fit_mode.is_some()
            || fit_background.is_some()
            || url.is_some()
            || asset_id.is_some()
    }
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
    pub fit_mode: Option<String>,
    pub fit_background: Option<String>,
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
    identity: Option<axum::Extension<crate::accounts::Identity>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    // An editor's upload is stored at once -- a file cannot wait as JSON -- but
    // pending in their draft, invisible to everyone else until it is applied.
    let proposer = identity
        .filter(|who| who.role == crate::accounts::Role::Editor)
        .and_then(|who| who.user_id);
    use tokio::io::AsyncWriteExt;

    let mut uploaded_files = Vec::new();
    // A `duration` field applies to the file parts after it, until the next one:
    // multipart parts are ordered and read one after another, so this needs no
    // buffering. The upload page sends one before every file, measured or empty.
    let mut measured: Option<i64> = None;

    while let Some(mut field) = multipart.next_field().await.unwrap_or(None) {
        // Plain (non-file) form fields carry no filename; they are not assets.
        // The one plain field that means something is the measured length.
        let Some(raw_filename) = field.file_name().map(|f| f.to_string()) else {
            if field.name() == Some("duration") {
                measured = field.text().await.ok().as_deref().and_then(seconds_from_measured);
            }
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
        let default_duration = measured.map(clamp_duration).unwrap_or(10);
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
            Ok(row) => {
                if let Some(author) = proposer {
                    use sqlx::Row;
                    let asset_id: i64 = row.get(0);
                    if let Err(e) = crate::proposals::record_upload(&state.pool, author, asset_id, &filename).await {
                        error!("Failed to record an upload as a proposal: {}", e);
                    }
                }
                uploaded_files.push(safe_filename)
            }
            Err(e) => {
                error!("DB Insert error: {}", e);
                let _ = tokio::fs::remove_file(&filepath).await;
            }
        }
    }

    (StatusCode::OK, Json(UploadResponse { uploaded: uploaded_files }))
}

pub async fn list_assets(
    State(state): State<AppState>,
    identity: Option<axum::Extension<crate::accounts::Identity>>,
) -> impl IntoResponse {
    // A proposed upload is its author's until the bundle is applied.
    let viewer = identity.and_then(|who| who.user_id);
    let assets = sqlx::query_as::<_, Asset>(
        "SELECT * FROM assets
         WHERE pending_changeset IS NULL
            OR pending_changeset IN (SELECT id FROM changesets WHERE author_id = ?)
         ORDER BY created_at DESC",
    )
        .bind(viewer)
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
        state.notify_playlist_changed();
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

        // Explicit, so a database whose rows predate `foreign_keys` being enabled is
        // cleaned up too. Without it those rows survive as playlist items with no
        // asset, which render as "no content".
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
        state.notify_playlist_changed();
    }
    StatusCode::OK
}

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
            COALESCE(p.fit_mode, 'contain') as fit_mode,
            COALESCE(p.fit_background, '#000000') as fit_background,
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

pub async fn add_to_playlist(
    State(state): State<AppState>,
    Json(payload): Json<AddToPlaylistRequest>,
) -> axum::response::Response {
    // An item with neither source silently renders as "no content" forever.
    if payload.asset_id.is_none() && payload.url.as_deref().unwrap_or("").trim().is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }

    // Checked before anything is read or written, so a refusal leaves no trace.
    let fit_background = match checked_fit_background(payload.fit_background) {
        Ok(value) => value.unwrap_or_else(|| crate::models::DEFAULT_FIT_BACKGROUND.to_string()),
        Err(response) => return response,
    };
    // Named or not, an item always stores a concrete fit. When it names none,
    // the default follows what it plays: a PDF keeps the full-width layout it
    // has always had.
    let fit_mode = match payload.fit_mode.as_deref() {
        Some(raw) => FitMode::from_value(raw),
        None => {
            let mimetype = match payload.asset_id {
                Some(asset_id) => sqlx::query_scalar::<_, String>(
                    "SELECT mimetype FROM assets WHERE id = ?",
                )
                .bind(asset_id)
                .fetch_optional(&state.pool)
                .await
                .unwrap_or_else(|e| {
                    error!("Failed to read the mimetype of asset {}: {}", asset_id, e);
                    None
                }),
                None => None,
            };
            FitMode::default_for(mimetype.as_deref())
        }
    };

    // Scoped to the target playlist: an unscoped max would give a new
    // playlist's first item a high order borrowed from an unrelated screen's
    // history, which reads as a bug the moment anything trusts the absolute
    // value (today only relative order within a playlist is read anywhere).
    let row: (i64,) = sqlx::query_as(
        "SELECT COALESCE(MAX(play_order), 0) FROM playlist_items WHERE playlist_id = ?",
    )
    .bind(payload.playlist_id)
    .fetch_one(&state.pool)
    .await
    .unwrap_or((0,));
    let next_order = row.0 + 1;

    let scroll_config = payload.scroll_config.unwrap_or(ScrollMode::None);
    let keep_loaded = payload.keep_loaded.unwrap_or(false);
    let enabled = payload.enabled.unwrap_or(true);
    // `null` unless it would draw something *or* recolour the box it lands in,
    // so the read paths never have to tell "switched off" from "empty".
    let overlay = payload
        .overlay
        .map(|overlay| overlay.sanitized())
        .filter(|overlay| overlay.matters())
        .map(sqlx::types::Json);

    let inserted = sqlx::query(
        "INSERT INTO playlist_items (asset_id, url, play_order, duration, is_enabled, keep_loaded, start_date, end_date, scroll_config, overlay_config, playlist_id, fit_mode, fit_background) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
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
    .bind(payload.playlist_id)
    .bind(fit_mode.as_str())
    .bind(fit_background)
    .execute(&state.pool)
    .await;
    let id = match inserted {
        Ok(result) => result.last_insert_rowid(),
        Err(e) => {
            error!("Failed to add playlist item: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    state.notify_playlist_changed();

    // The id, so a caller can refer to what it created -- which is what an
    // approved proposal does when a later request in its bundle names the item.
    (StatusCode::CREATED, Json(serde_json::json!({ "id": id }))).into_response()
}

pub async fn update_playlist_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdatePlaylistRequest>,
) -> impl IntoResponse {
    // A move stands alone: a request carrying `playlist_id` may carry nothing
    // else.
    //
    // Moving is not a kind change -- the playlist an item belongs to is a
    // different axis from what it plays, so the like-for-like rule below has
    // nothing to say about it -- but everything below is a field-by-field write
    // whose error is swallowed and logged, while the move is a transaction over
    // two playlists' `play_order`. Combined, a move that rolled back would sit
    // behind edits that had already landed, under one status code that cannot
    // say which half happened. The two also contradict each other outright: the
    // move decides the item's new `play_order`, so an explicit one in the same
    // request is two answers to one question. Refusing the combination costs a
    // client a second request -- which is what the up/down buttons already do --
    // and is the only version of this handler with no half-applied write in it.
    if let Some(target) = payload.playlist_id {
        if payload.edits_besides_the_playlist() {
            return bad_request(
                "Playlist-Wechsel bitte allein senden, ohne weitere Änderungen am Element.",
            );
        }
        let Some(target) = target else {
            return bad_request(
                "Ein Element ohne Playlist spielt kein Bildschirm. Bitte eine Playlist wählen.",
            );
        };
        return match move_item_to_playlist(&state.pool, id, target).await {
            Ok(MoveOutcome::Moved) => {
                // Two screens change what they play, which is why this is the
                // unscoped notify like every other playlist-affecting write.
                // Deliberately *not* `notify_overlay_changed`: an item's overlay
                // travels with the item and neither it nor the global one was
                // touched here, so poking that signal would re-apply a badge
                // nothing changed about -- and on a display standing in an
                // override (a cast, a pinned page) that is work for no reason.
                state.notify_playlist_changed();
                StatusCode::OK.into_response()
            }
            // Nothing changed, so nothing to announce. Not an error either: a
            // client resending the playlist an item already sits in has asked
            // for the state it is already in.
            Ok(MoveOutcome::AlreadyThere) => StatusCode::OK.into_response(),
            // Same refusal wording as `PUT /api/displays/{name}`, which checks
            // the same id against the same table for the same reason.
            Ok(MoveOutcome::UnknownPlaylist) => bad_request("Unbekannte Playlist."),
            Ok(MoveOutcome::UnknownItem) => (
                StatusCode::NOT_FOUND,
                Json(ApiError {
                    error: format!("Element {} gibt es nicht.", id),
                }),
            )
                .into_response(),
            Err(e) => {
                error!("Failed to move playlist item {} to playlist {}: {}", id, target, e);
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        };
    }

    // Refused before the source edit below writes anything, so a 400 here is not
    // sitting on top of a half-applied request.
    let fit_background = match checked_fit_background(payload.fit_background.clone()) {
        Ok(value) => value,
        Err(response) => return response,
    };

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
        // Stored as `null` when it neither draws nor recolours, so the read paths
        // do not have to tell "switched off" from "empty".
        let stored = overlay.matters().then(|| sqlx::types::Json(overlay));
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

    state.notify_playlist_changed();
    // The item on screen may be this one, and its badge should not wait for the
    // next navigation. The loop re-reads the item's overlay when it re-applies.
    if overlay_changed {
        state.notify_overlay_changed();
    }

    StatusCode::OK.into_response()
}

/// The ids `move_playlist_item` reorders among, in play order, scoped to one
/// playlist. Pulled out of the handler so the scoping can be tested at pool
/// level, without an `AppState` -- the previous version built this list from
/// every row in the table regardless of playlist, which let an "up"/"down"
/// click on one screen swap orders with an unrelated screen's playlist.
/// `IS` rather than `=`: `playlist_id` is nullable (a row an older binary
/// wrote, or one the one-time backfill never reached), and SQLite's
/// `NULL = NULL` is unknown, not true, in a `WHERE` clause -- which would
/// silently exclude same-playlist NULL rows from each other.
///
/// Generic over the executor rather than taking `&SqlitePool`, so the move
/// below can read the same list inside its transaction instead of keeping a
/// second copy of this query -- and with it a second copy of the `IS` rule.
pub(crate) async fn ordered_ids_in_playlist<'e, E>(
    executor: E,
    playlist_id: Option<i64>,
) -> Result<Vec<i64>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query_scalar::<_, i64>(
        "SELECT id FROM playlist_items WHERE playlist_id IS ? ORDER BY play_order ASC, id ASC",
    )
    .bind(playlist_id)
    .fetch_all(executor)
    .await
}

/// Renumber one playlist `1..n` in the order it currently reads in.
///
/// Renumbering rather than patching the one row that moved, for the reason
/// `move_playlist_item` gives: `play_order` is typed by hand in the UI, so
/// duplicates and gaps are the column's normal state, and a move that only
/// touched its own row would carry them along.
async fn renumber_playlist(
    conn: &mut sqlx::SqliteConnection,
    playlist_id: Option<i64>,
) -> Result<(), sqlx::Error> {
    let ids = ordered_ids_in_playlist(&mut *conn, playlist_id).await?;
    for (offset, item_id) in ids.iter().enumerate() {
        sqlx::query("UPDATE playlist_items SET play_order = ? WHERE id = ?")
            .bind(offset as i64 + 1)
            .bind(item_id)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// What `move_item_to_playlist` did, so the handler can phrase the refusal and
/// the tests can assert the outcome without an `AppState`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MoveOutcome {
    Moved,
    AlreadyThere,
    UnknownItem,
    UnknownPlaylist,
}

/// Move one item into another playlist, appended to the end of it, and close
/// the gap it leaves behind.
///
/// `play_order` belongs to a playlist, not to the table: an item carrying its
/// old number into a new list collides with whatever already holds that number
/// and leaves a hole where it came from. So the item is appended (`MAX + 1`
/// scoped to the target, exactly as `add_to_playlist` numbers a new one) and
/// *both* lists are then renumbered `1..n`, which is what "leaves both
/// playlists coherent" means here -- the target with the newcomer last, the
/// source with its gap closed.
///
/// One transaction, because these are several statements describing one
/// decision. Half of it -- the item in the target list, both lists still
/// numbered as if it had not moved -- is a playlist that plays in an order
/// nobody chose, on a device whose ordinary way to stop is losing power.
///
/// The playlist's existence is checked in the statement that writes it, in the
/// style of `playlists::remove`, and not before it: `playlist_items.playlist_id`
/// was added by `ALTER TABLE` and therefore carries **no** foreign key, so
/// nothing underneath this function would refuse a dangling id. The item would
/// simply stop appearing on every screen with nothing saying why -- the same
/// blank-screen failure the `asset_id` check upstream exists to prevent.
pub(crate) async fn move_item_to_playlist(
    pool: &sqlx::SqlitePool,
    id: i64,
    target: i64,
) -> Result<MoveOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // The outer Option is "is there such an item", the inner one is "does it
    // have a playlist" -- an orphan (inner `None`) is precisely the row this
    // endpoint exists to adopt, so it is not an error here.
    let source: Option<Option<i64>> =
        sqlx::query_scalar("SELECT playlist_id FROM playlist_items WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(source) = source else {
        return Ok(MoveOutcome::UnknownItem);
    };
    // Checked rather than let through: the append below would otherwise send an
    // item to the end of the list it is already in, which is a reorder nobody
    // asked for.
    if source == Some(target) {
        return Ok(MoveOutcome::AlreadyThere);
    }

    let done = sqlx::query(
        "UPDATE playlist_items
            SET playlist_id = ?1,
                play_order = (SELECT COALESCE(MAX(play_order), 0) + 1
                                FROM playlist_items WHERE playlist_id = ?1)
          WHERE id = ?2
            AND EXISTS (SELECT 1 FROM playlists WHERE id = ?1)",
    )
    .bind(target)
    .bind(id)
    .execute(&mut *tx)
    .await?;

    if done.rows_affected() == 0 {
        // The item was read a statement ago inside this transaction, so the only
        // thing the guard can have refused is the playlist.
        return Ok(MoveOutcome::UnknownPlaylist);
    }

    renumber_playlist(&mut tx, source).await?;
    renumber_playlist(&mut tx, Some(target)).await?;
    tx.commit().await?;
    Ok(MoveOutcome::Moved)
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

    // The renumbering below must stay inside the moved item's own playlist:
    // pulling the whole table into one 1..n sequence would interleave two
    // screens' orders, which shows up as items playing in the wrong sequence on
    // a screen nobody was touching.
    let playlist_id: Option<i64> =
        sqlx::query_scalar("SELECT playlist_id FROM playlist_items WHERE id = ?")
            .bind(id)
            .fetch_optional(&state.pool)
            .await
            .unwrap_or(None)
            .flatten();

    let mut ids = match ordered_ids_in_playlist(&state.pool, playlist_id).await {
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

    state.notify_playlist_changed();
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
    state.notify_playlist_changed();
    StatusCode::OK
}


// The five playback handlers below come in pairs: a `_for` entry point that
// takes the display's name out of the path, a legacy unscoped one that resolves
// through `display::resolve(.., None)`, and one shared `_of` body. Duplicating
// the body instead would let the two drift, and the unscoped path is exactly the
// one nobody tests by hand.

pub async fn set_current_for(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(payload): Json<SetCurrentItemRequest>,
) -> Response {
    match crate::display::resolve(&state, Some(&name)) {
        Ok(display) => set_current_of(&display, payload).await,
        Err(response) => response,
    }
}

pub async fn set_current(
    State(state): State<AppState>,
    Json(payload): Json<SetCurrentItemRequest>,
) -> Response {
    match crate::display::resolve(&state, None) {
        Ok(display) => set_current_of(&display, payload).await,
        Err(response) => response,
    }
}

async fn set_current_of(display: &Display, payload: SetCurrentItemRequest) -> Response {
    // Record the request in `pending_jump`, not `current_item_id`: the browser loop
    // owns `current_item_id` and rewrites it at the start of every item, so writing
    // there races with playback and loses the click.
    {
        let mut lock = display.pending_jump.lock().await;
        *lock = payload.item_id;
    }
    // Interrupt the current wait. notify_one stores a permit if the loop is busy
    // navigating, so the request survives until the loop next awaits.
    display.skip_signal.notify_one();
    StatusCode::OK.into_response()
}

pub async fn get_current_for(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    match crate::display::resolve(&state, Some(&name)) {
        Ok(display) => get_current_of(&display).await,
        Err(response) => response,
    }
}

pub async fn get_current(State(state): State<AppState>) -> Response {
    match crate::display::resolve(&state, None) {
        Ok(display) => get_current_of(&display).await,
        Err(response) => response,
    }
}

async fn get_current_of(display: &Display) -> Response {
    let id = {
        let lock = display.current_item_id.lock().await;
        *lock
    };
    (StatusCode::OK, Json(CurrentItemResponse { item_id: id })).into_response()
}

pub async fn get_override_for(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    match crate::display::resolve(&state, Some(&name)) {
        Ok(display) => get_override_of(&display).await,
        Err(response) => response,
    }
}

pub async fn get_override(State(state): State<AppState>) -> Response {
    match crate::display::resolve(&state, None) {
        Ok(display) => get_override_of(&display).await,
        Err(response) => response,
    }
}

async fn get_override_of(display: &Display) -> Response {
    let current = {
        let lock = display.override_item.lock().await;
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

    (StatusCode::OK, Json(body)).into_response()
}

pub async fn set_override_for(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(payload): Json<SetOverrideRequest>,
) -> Response {
    match crate::display::resolve(&state, Some(&name)) {
        Ok(display) => set_override_of(&state, &display, payload).await,
        Err(response) => response,
    }
}

pub async fn set_override(
    State(state): State<AppState>,
    Json(payload): Json<SetOverrideRequest>,
) -> Response {
    match crate::display::resolve(&state, None) {
        Ok(display) => set_override_of(&state, &display, payload).await,
        Err(response) => response,
    }
}

async fn set_override_of(
    state: &AppState,
    display: &Display,
    payload: SetOverrideRequest,
) -> Response {
    if payload.asset_id.is_none() && payload.url.is_none() {
        return (StatusCode::BAD_REQUEST, Json(OverrideResponse { active: false })).into_response();
    }

    let fit_background = match checked_fit_background(payload.fit_background) {
        Ok(value) => value.unwrap_or_else(|| crate::models::DEFAULT_FIT_BACKGROUND.to_string()),
        Err(response) => return response,
    };

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

    // Captured before `payload.url` is moved into the item below.
    let announced_url = payload.url.clone().unwrap_or_default();

    // Decided before `mimetype` moves into the item: an asset override that
    // names no fit gets the one its kind defaults to, as a playlist item does.
    let fit_mode = payload
        .fit_mode
        .as_deref()
        .map(FitMode::from_value)
        .unwrap_or_else(|| FitMode::default_for(mimetype.as_deref()));

    let override_item = OverrideItem {
        asset_id: payload.asset_id,
        url: payload.url,
        local_path,
        mimetype,
        scroll_config: payload.scroll_config.unwrap_or(ScrollMode::None),
        fit_mode,
        fit_background,
    };

    {
        let mut lock = display.override_item.lock().await;
        *lock = Some(override_item);
    }

    // Only override_signal: the browser loop watches it both while playing the
    // playlist and while an override is up. Poking skip_signal as well would leave
    // an unconsumed permit that cuts the next playlist item short.
    display.override_signal.notify_one();

    // An asset override has no URL to announce, and inventing one would be a
    // link to something the receiver cannot fetch. The `asset_id` in the
    // operator's own request is the identifier; the empty string says "not a URL".
    state.webhooks.fire(&display.name, crate::webhook::Event::OverrideSet {
        url: crate::browser::redact_str(&announced_url),
        source: "operator",
    });

    (StatusCode::OK, Json(OverrideResponse { active: true })).into_response()
}

pub async fn clear_override_for(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    match crate::display::resolve(&state, Some(&name)) {
        Ok(display) => clear_override_of(&state, &display).await,
        Err(response) => response,
    }
}

pub async fn clear_override(State(state): State<AppState>) -> Response {
    match crate::display::resolve(&state, None) {
        Ok(display) => clear_override_of(&state, &display).await,
        Err(response) => response,
    }
}

async fn clear_override_of(state: &AppState, display: &Display) -> Response {
    {
        let mut lock = display.override_item.lock().await;
        *lock = None;
    }

    display.override_signal.notify_one();

    state
        .webhooks
        .fire(&display.name, crate::webhook::Event::OverrideCleared { source: "operator" });

    (StatusCode::OK, Json(OverrideResponse { active: false })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> sqlx::SqlitePool {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:?cache=shared")
            .await
            .unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn ordered_ids_stay_inside_one_playlist() {
        let pool = pool().await;
        sqlx::query("INSERT INTO playlists (id, name) VALUES (1, 'A'), (2, 'B')")
            .execute(&pool)
            .await
            .unwrap();
        // Interleaved ids on purpose: 1 and 3 belong to playlist 1, 2 and 4 to
        // playlist 2, so an unscoped query (the bug) would return all four.
        for (id, playlist_id) in [(1, 1), (2, 2), (3, 1), (4, 2)] {
            sqlx::query(
                "INSERT INTO playlist_items (id, url, play_order, playlist_id) VALUES (?, 'https://a.test', ?, ?)",
            )
            .bind(id)
            .bind(id)
            .bind(playlist_id)
            .execute(&pool)
            .await
            .unwrap();
        }

        let ids = ordered_ids_in_playlist(&pool, Some(1)).await.unwrap();
        assert_eq!(
            ids,
            vec![1, 3],
            "a move in playlist 1 must never reach items belonging to playlist 2"
        );
    }

    #[tokio::test]
    async fn null_playlist_id_groups_with_other_null_rows() {
        let pool = pool().await;
        sqlx::query("INSERT INTO playlists (id, name) VALUES (1, 'A')")
            .execute(&pool)
            .await
            .unwrap();
        // Two rows an older binary wrote before the backfill reached them (NULL
        // playlist_id), with one playlist-1 row sandwiched between them. `=`
        // would treat `NULL = NULL` as unknown and return neither NULL row;
        // `IS` must return exactly the two NULL rows, not zero and not all three.
        sqlx::query(
            "INSERT INTO playlist_items (id, url, play_order, playlist_id) VALUES (1, 'https://a.test', 1, NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO playlist_items (id, url, play_order, playlist_id) VALUES (2, 'https://a.test', 2, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO playlist_items (id, url, play_order, playlist_id) VALUES (3, 'https://a.test', 3, NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let ids = ordered_ids_in_playlist(&pool, None).await.unwrap();
        assert_eq!(
            ids,
            vec![1, 3],
            "IS groups NULL rows with each other, not with every row nor with none"
        );
    }

    /// Two playlists, the second one deliberately numbered with a gap and a
    /// duplicate -- which is the ordinary state of a column operators type by
    /// hand, and the state a move has to survive.
    async fn two_playlists() -> sqlx::SqlitePool {
        let pool = pool().await;
        sqlx::query("INSERT INTO playlists (id, name) VALUES (1, 'Foyer'), (2, 'Werkstatt')")
            .execute(&pool)
            .await
            .unwrap();
        for (id, play_order, playlist_id) in [
            (1, 1, Some(1)),
            (2, 2, Some(1)),
            (3, 3, Some(1)),
            (4, 7, Some(2)),
            (5, 7, Some(2)),
        ] {
            sqlx::query(
                "INSERT INTO playlist_items (id, url, play_order, playlist_id) VALUES (?, 'https://a.test', ?, ?)",
            )
            .bind(id)
            .bind(play_order)
            .bind(playlist_id)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool
    }

    async fn playlist_of(pool: &sqlx::SqlitePool, id: i64) -> Option<i64> {
        sqlx::query_scalar::<_, Option<i64>>("SELECT playlist_id FROM playlist_items WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn orders_in(pool: &sqlx::SqlitePool, playlist_id: Option<i64>) -> Vec<(i64, i64)> {
        sqlx::query_as::<_, (i64, i64)>(
            "SELECT id, play_order FROM playlist_items WHERE playlist_id IS ? \
             ORDER BY play_order ASC, id ASC",
        )
        .bind(playlist_id)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_moved_item_is_appended_and_both_lists_are_renumbered() {
        let pool = two_playlists().await;

        assert_eq!(
            move_item_to_playlist(&pool, 2, 2).await.unwrap(),
            MoveOutcome::Moved
        );

        assert_eq!(playlist_of(&pool, 2).await, Some(2));
        // Appended, not inserted where its old number would have put it: item 2
        // carried `play_order = 2`, which in the target list would have placed
        // it ahead of both items already there.
        assert_eq!(
            orders_in(&pool, Some(2)).await,
            vec![(4, 1), (5, 2), (2, 3)],
            "the newcomer sits last, and the target's duplicate 7s are renumbered 1..n"
        );
        // The gap the item left behind is closed rather than left as 1,3.
        assert_eq!(
            orders_in(&pool, Some(1)).await,
            vec![(1, 1), (3, 2)],
            "the source list is renumbered 1..n too"
        );
    }

    #[tokio::test]
    async fn a_playlist_that_does_not_exist_is_refused_and_nothing_moves() {
        let pool = two_playlists().await;

        assert_eq!(
            move_item_to_playlist(&pool, 2, 99).await.unwrap(),
            MoveOutcome::UnknownPlaylist
        );

        // Nothing written at all: a dangling `playlist_id` would take the item
        // off every screen with nothing saying why, and `playlist_items` has no
        // foreign key on this column to catch it afterwards.
        assert_eq!(playlist_of(&pool, 2).await, Some(1));
        assert_eq!(
            orders_in(&pool, Some(1)).await,
            vec![(1, 1), (2, 2), (3, 3)],
            "a refused move renumbers nothing either"
        );
    }

    #[tokio::test]
    async fn an_item_with_no_playlist_can_be_adopted() {
        let pool = two_playlists().await;
        // The row an interrupted backfill or an older binary leaves behind:
        // invisible in the editor and, before this endpoint, unadoptable.
        sqlx::query(
            "INSERT INTO playlist_items (id, url, play_order, playlist_id) VALUES (9, 'https://a.test', 4, NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(
            move_item_to_playlist(&pool, 9, 1).await.unwrap(),
            MoveOutcome::Moved
        );

        assert_eq!(playlist_of(&pool, 9).await, Some(1));
        assert_eq!(
            orders_in(&pool, Some(1)).await,
            vec![(1, 1), (2, 2), (3, 3), (9, 4)]
        );
        assert!(
            orders_in(&pool, None).await.is_empty(),
            "the orphan group is empty afterwards"
        );
    }

    #[tokio::test]
    async fn moving_an_item_where_it_already_is_reorders_nothing() {
        let pool = two_playlists().await;

        assert_eq!(
            move_item_to_playlist(&pool, 1, 1).await.unwrap(),
            MoveOutcome::AlreadyThere
        );

        // Not appended to the end of its own list, which is what an unguarded
        // `MAX + 1` would have done to an item nobody asked to reorder.
        assert_eq!(
            orders_in(&pool, Some(1)).await,
            vec![(1, 1), (2, 2), (3, 3)]
        );
    }

    #[tokio::test]
    async fn an_item_that_does_not_exist_is_reported_as_missing() {
        let pool = two_playlists().await;
        assert_eq!(
            move_item_to_playlist(&pool, 404, 1).await.unwrap(),
            MoveOutcome::UnknownItem
        );
    }

    #[test]
    fn a_measured_length_is_rounded_down_to_whole_seconds() {
        assert_eq!(seconds_from_measured("37.8"), Some(37));
        assert_eq!(seconds_from_measured(" 5 "), Some(5));
        // A clip shorter than a second still plays for one.
        assert_eq!(seconds_from_measured("0.4"), Some(1));
        for junk in ["", "abc", "NaN", "inf", "-3", "0"] {
            assert_eq!(seconds_from_measured(junk), None, "{junk:?}");
        }
    }

    #[test]
    fn a_move_travelling_with_any_other_edit_is_recognised() {
        let mut request = UpdatePlaylistRequest {
            play_order: None,
            duration: None,
            enabled: None,
            is_enabled: None,
            keep_loaded: None,
            start_date: None,
            end_date: None,
            scroll_config: None,
            overlay: None,
            fit_mode: None,
            fit_background: None,
            url: None,
            asset_id: None,
            playlist_id: Some(Some(2)),
        };
        assert!(!request.edits_besides_the_playlist(), "a move on its own");

        // The playlist page saves a whole card at once, so the field most likely
        // to arrive alongside a move is the source -- and a source edit reads
        // the row as it is, which the move is about to rewrite.
        request.url = Some("https://b.test".into());
        assert!(request.edits_besides_the_playlist());
        request.url = None;
        // The one that contradicts the move outright: it decides the order.
        request.play_order = Some(1);
        assert!(request.edits_besides_the_playlist());
        request.play_order = None;
        // Saving a card sends the fit along with everything else.
        request.fit_mode = Some("cover".into());
        assert!(request.edits_besides_the_playlist());
    }
}
