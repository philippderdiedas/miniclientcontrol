use axum::{
    extract::{Multipart, State, Path},
    response::{IntoResponse, Json},
    http::StatusCode,
};
use tracing::error;
use crate::models::{AppState, Asset, OverrideItem, PlaylistItemWithAsset, ScrollMode};
use serde::{Deserialize, Serialize};

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
}

#[derive(Deserialize)]
pub struct UpdatePlaylistRequest {
    pub play_order: Option<i64>,
    pub duration: Option<i64>,
    pub enabled: Option<bool>,
    pub is_enabled: Option<bool>,
    pub keep_loaded: Option<bool>,
    pub start_date: Option<Option<String>>,
    pub end_date: Option<Option<String>>,
    pub scroll_config: Option<ScrollMode>,
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

// --- Handlers ---

pub async fn upload_asset(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let mut uploaded_files = Vec::new();

    while let Some(field) = multipart.next_field().await.unwrap_or(None) {
        let filename = field.file_name().map(|f| f.to_string()).unwrap_or_else(|| "unnamed".to_string());
        let content_type = field.content_type().map(|c| c.to_string()).unwrap_or_else(|| "application/octet-stream".to_string());
        
        let safe_filename = format!("{}_{}", uuid::Uuid::new_v4(), filename.replace("..", "").replace("/", "_"));
        let filepath = state.args.assets_dir.join(&safe_filename);
        
        // Stream to file
        let file_result = tokio::fs::File::create(&filepath).await;
        if let Ok(mut file) = file_result {
            let data_res = field.bytes().await; 
            
            if let Ok(data) = data_res {
                use tokio::io::AsyncWriteExt;
                if let Err(e) = file.write_all(&data).await {
                    error!("Write failed: {}", e);
                    continue; 
                }
            }
        } else {
            error!("Failed to create file: {:?}", filepath);
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

        match result {             Ok(_) => uploaded_files.push(safe_filename),             Err(e) => error!("DB Insert error: {}", e),        }    }    (StatusCode::OK, Json(UploadResponse { uploaded: uploaded_files }))}
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
        let _ = sqlx::query("UPDATE assets SET duration = ? WHERE id = ?")
            .bind(duration)
            .bind(id)
            .execute(&state.pool)
            .await;
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
        
        let _ = sqlx::query("DELETE FROM assets WHERE id = ?")
            .bind(id)
            .execute(&state.pool)
            .await;
        state.playlist_signal.notify_waiters();
    }
    StatusCode::OK
}

pub async fn get_playlist(State(state): State<AppState>) -> impl IntoResponse {
    // We need to join with assets
    let items = sqlx::query_as::<_, PlaylistItemWithAsset>(
        r#"
        SELECT 
            p.id, p.asset_id, p.url, p.play_order, p.duration, p.is_enabled as enabled, p.is_enabled,
            p.start_date, p.end_date,
            p.keep_loaded,
            p.scroll_config,
            a.local_path, a.mimetype, a.duration as asset_duration
        FROM playlist_items p
        LEFT JOIN assets a ON p.asset_id = a.id
        ORDER BY p.play_order ASC
        "#
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    (StatusCode::OK, Json(items))
}

pub async fn add_to_playlist(
    State(state): State<AppState>,
    Json(payload): Json<AddToPlaylistRequest>,
) -> impl IntoResponse {
    // Get max play_order
    let row: (i64,) = sqlx::query_as("SELECT COALESCE(MAX(play_order), 0) FROM playlist_items")
        .fetch_one(&state.pool)
        .await
        .unwrap_or((0,));
    let next_order = row.0 + 1;

    let scroll_config = payload.scroll_config.unwrap_or(ScrollMode::None);
    let keep_loaded = payload.keep_loaded.unwrap_or(false);
    let enabled = payload.enabled.unwrap_or(true);

    let _ = sqlx::query(
        "INSERT INTO playlist_items (asset_id, url, play_order, duration, is_enabled, keep_loaded, start_date, end_date, scroll_config) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(payload.asset_id)
    .bind(payload.url)
    .bind(next_order)
    .bind(payload.duration)
    .bind(enabled)
    .bind(keep_loaded)
    .bind(payload.start_date)
    .bind(payload.end_date)
    .bind(sqlx::types::Json(scroll_config))
    .execute(&state.pool)
    .await;

    state.playlist_signal.notify_waiters();

    StatusCode::CREATED
}

pub async fn update_playlist_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdatePlaylistRequest>,
) -> impl IntoResponse {
    if let Some(val) = payload.play_order {
        let _ = sqlx::query("UPDATE playlist_items SET play_order = ? WHERE id = ?").bind(val).bind(id).execute(&state.pool).await;
    }
    if let Some(val) = payload.duration {
        let _ = sqlx::query("UPDATE playlist_items SET duration = ? WHERE id = ?").bind(val).bind(id).execute(&state.pool).await;
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

    state.playlist_signal.notify_waiters();

    StatusCode::OK
}

pub async fn delete_playlist_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let _ = sqlx::query("DELETE FROM playlist_items WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await;
    state.playlist_signal.notify_waiters();
    StatusCode::OK
}


pub async fn set_current(
    State(state): State<AppState>, 
    Json(payload): Json<SetCurrentItemRequest>
) -> impl IntoResponse {
    {
        let mut lock = state.current_item_id.lock().await;
        *lock = payload.item_id;
    }
    // Also notify skip to interrupt current wait
    state.skip_signal.notify_waiters();
    StatusCode::OK
}

pub async fn get_current(State(state): State<AppState>) -> impl IntoResponse {
    let id = {
        let lock = state.current_item_id.lock().await;
        *lock
    };
    (StatusCode::OK, Json(CurrentItemResponse { item_id: id }))
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

    state.override_signal.notify_waiters();
    state.skip_signal.notify_waiters();

    (StatusCode::OK, Json(OverrideResponse { active: true })).into_response()
}

pub async fn clear_override(
    State(state): State<AppState>,
) -> impl IntoResponse {
    {
        let mut lock = state.override_item.lock().await;
        *lock = None;
    }

    state.override_signal.notify_waiters();
    state.skip_signal.notify_waiters();

    (StatusCode::OK, Json(OverrideResponse { active: false }))
}