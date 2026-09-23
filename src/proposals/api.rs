//! The editor's draft, and (for managers) review, approval and rejection.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::Method;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::accounts::Identity;
use crate::models::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/changesets/draft", get(draft).delete(discard))
        .route("/api/changesets/draft/submit", post(submit))
        .route("/api/changesets/draft/requests/{id}", delete(drop_request))
        .route("/api/changesets", get(list))
        .route("/api/changesets/mine", get(mine))
        .route("/api/changesets/mine/seen", post(mine_seen))
        .route("/api/changesets/mine/hide-decided", post(hide_decided))
        .route("/api/changesets/{id}/withdraw", post(withdraw))
        .route("/api/changesets/{id}/hide", post(hide))
        .route("/api/changesets/{id}/approve", post(approve))
        .route("/api/changesets/{id}/reject", post(reject))
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn parsed(raw: Option<String>) -> Value {
    raw.and_then(|text| serde_json::from_str(&text).ok()).unwrap_or(Value::Null)
}

/// A bundle as the pages show it: its requests in order, each with the object
/// as it read when proposed.
pub(crate) async fn bundle(pool: &sqlx::SqlitePool, id: i64) -> Option<Value> {
    let head: (i64, String, Option<String>, Option<String>, Option<String>, Option<String>, String) = sqlx::query_as(
        "SELECT c.id, c.state, c.note, c.created_at, c.submitted_at, c.decided_at, COALESCE(u.name, '')
         FROM changesets c LEFT JOIN users u ON u.id = c.author_id WHERE c.id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .ok()??;
    let rows: Vec<(i64, i64, String, String, Option<String>, Option<String>, Option<String>, bool, Option<String>)> =
        sqlx::query_as(
            "SELECT id, position, method, path, body, placeholder, before, applied, result
             FROM change_requests WHERE changeset_id = ? ORDER BY position ASC",
        )
        .bind(id)
        .fetch_all(pool)
        .await
        .ok()?;
    let requests: Vec<Value> = rows.into_iter().map(|(id, position, method, path, body, placeholder, before, applied, result)| json!({
        "id": id, "position": position, "method": method, "path": path,
        "body": parsed(body), "placeholder": placeholder, "before": parsed(before),
        "applied": applied, "result": parsed(result),
    })).collect();
    let refs = refs(pool, &requests).await;
    Some(json!({
        "id": head.0, "state": head.1, "note": head.2, "created_at": head.3,
        "submitted_at": head.4, "decided_at": head.5, "author": head.6,
        "requests": requests, "refs": refs,
    }))
}

/// What a bundle's requests point at, as it reads now: playlist names, the
/// items' content, and the assets with enough to preview them. A request only
/// carries ids, and a line reading "item #12 in playlist 3" says nothing a
/// week later. Pending uploads are included -- the asset list hides them from
/// everybody but their author, and the reviewer is who needs to see them.
async fn refs(pool: &sqlx::SqlitePool, requests: &[Value]) -> Value {
    let mut assets = std::collections::BTreeSet::new();
    let mut playlists = std::collections::BTreeSet::new();
    let mut items = std::collections::BTreeSet::new();
    let number = |v: &Value| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()));
    for request in requests {
        let path = request["path"].as_str().unwrap_or("");
        let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match segments.as_slice() {
            ["api", "assets", id, ..] => assets.extend(id.parse::<i64>().ok()),
            ["api", "playlists", id, ..] => playlists.extend(id.parse::<i64>().ok()),
            ["api", "playlist", id, ..] => items.extend(id.parse::<i64>().ok()),
            _ => {}
        }
        for side in [&request["body"], &request["before"]] {
            assets.extend(number(&side["asset_id"]));
            playlists.extend(number(&side["playlist_id"]));
            playlists.extend(number(&side["default_playlist_id"]));
            if let Some(windows) = side["windows"].as_array() {
                playlists.extend(windows.iter().filter_map(|w| number(&w["playlist_id"])));
            }
        }
    }
    let mut item_map = serde_json::Map::new();
    for id in items {
        let row: Option<(Option<String>, Option<i64>, Option<i64>)> = sqlx::query_as(
            "SELECT url, asset_id, playlist_id FROM playlist_items WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap_or(None);
        if let Some((url, asset_id, playlist_id)) = row {
            assets.extend(asset_id);
            playlists.extend(playlist_id);
            item_map.insert(id.to_string(), json!({ "url": url, "asset_id": asset_id, "playlist_id": playlist_id }));
        }
    }
    let mut asset_map = serde_json::Map::new();
    for id in assets {
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT filename, local_path, mimetype FROM assets WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap_or(None);
        if let Some((filename, local_path, mimetype)) = row {
            asset_map.insert(id.to_string(), json!({
                "id": id, "filename": filename, "local_path": local_path, "mimetype": mimetype,
            }));
        }
    }
    let mut playlist_map = serde_json::Map::new();
    for id in playlists {
        let name: Option<String> = sqlx::query_scalar("SELECT name FROM playlists WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await
            .unwrap_or(None);
        if let Some(name) = name {
            playlist_map.insert(id.to_string(), json!(name));
        }
    }
    json!({ "assets": asset_map, "playlists": playlist_map, "items": item_map })
}

async fn draft_id(pool: &sqlx::SqlitePool, who: &Identity) -> Option<i64> {
    sqlx::query_scalar("SELECT id FROM changesets WHERE author_id = ? AND state = 'draft'")
        .bind(who.user_id?)
        .fetch_optional(pool)
        .await
        .ok()?
}

async fn draft(State(state): State<AppState>, Extension(who): Extension<Identity>) -> Response {
    match draft_id(&state.pool, &who).await {
        Some(id) => Json(bundle(&state.pool, id).await.unwrap_or(Value::Null)).into_response(),
        None => Json(json!({ "id": null, "state": "draft", "requests": [] })).into_response(),
    }
}

async fn discard(State(state): State<AppState>, Extension(who): Extension<Identity>) -> Response {
    if let Some(id) = draft_id(&state.pool, &who).await {
        super::drop_pending_assets(&state, id).await;
        let _ = sqlx::query("DELETE FROM changesets WHERE id = ?").bind(id).execute(&state.pool).await;
    }
    Json(json!({ "ok": true })).into_response()
}

async fn drop_request(
    State(state): State<AppState>,
    Extension(who): Extension<Identity>,
    Path(request): Path<i64>,
) -> Response {
    let Some(changeset) = draft_id(&state.pool, &who).await else {
        return error(StatusCode::NOT_FOUND, "Kein Entwurf.");
    };
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT method, path FROM change_requests WHERE id = ? AND changeset_id = ?",
    )
    .bind(request)
    .bind(changeset)
    .fetch_optional(&state.pool)
    .await
    .unwrap_or(None);
    let Some((method, path)) = row else {
        return error(StatusCode::NOT_FOUND, "Diese Änderung ist nicht im Entwurf.");
    };
    // Dropping an upload drops its file too.
    if method == "UPLOAD" {
        if let Some(asset) = path.rsplit('/').next().and_then(|id| id.parse::<i64>().ok()) {
            let local: Option<String> = sqlx::query_scalar("SELECT local_path FROM assets WHERE id = ? AND pending_changeset = ?")
                .bind(asset).bind(changeset).fetch_optional(&state.pool).await.unwrap_or(None);
            if let Some(local) = local {
                let _ = tokio::fs::remove_file(state.args.assets_dir.join(&local)).await;
                let _ = sqlx::query("DELETE FROM assets WHERE id = ?").bind(asset).execute(&state.pool).await;
            }
        }
    }
    let _ = sqlx::query("DELETE FROM change_requests WHERE id = ?").bind(request).execute(&state.pool).await;
    Json(json!({ "ok": true })).into_response()
}

async fn submit(State(state): State<AppState>, Extension(who): Extension<Identity>) -> Response {
    let Some(id) = draft_id(&state.pool, &who).await else {
        return error(StatusCode::BAD_REQUEST, "Der Entwurf ist leer.");
    };
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM change_requests WHERE changeset_id = ?")
        .bind(id).fetch_one(&state.pool).await.unwrap_or(0);
    if count == 0 {
        return error(StatusCode::BAD_REQUEST, "Der Entwurf ist leer.");
    }
    let _ = sqlx::query("UPDATE changesets SET state = 'submitted', submitted_at = CURRENT_TIMESTAMP WHERE id = ?")
        .bind(id).execute(&state.pool).await;
    Json(json!({ "ok": true, "id": id })).into_response()
}

#[derive(Deserialize)]
struct ListQuery {
    state: Option<String>,
}

/// Bundles for review, oldest first; `?state=submitted` is what the approvals
/// page asks for.
async fn list(State(state): State<AppState>, Query(query): Query<ListQuery>) -> Response {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM changesets WHERE state != 'draft' AND (?1 IS NULL OR state = ?1)
         ORDER BY COALESCE(submitted_at, created_at) ASC, id ASC",
    )
    .bind(query.state)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(b) = bundle(&state.pool, id).await {
            out.push(b);
        }
    }
    Json(out).into_response()
}

async fn decide(pool: &sqlx::SqlitePool, id: i64, to: &str, who: &Identity, note: Option<&str>) {
    // Logged, not swallowed: a failed write here leaves the bundle in
    // `applying`, where nobody can approve or reject it any more.
    if let Err(e) = sqlx::query(
        "UPDATE changesets SET state = ?, note = COALESCE(?, note), decided_by = ?,
             decided_at = CURRENT_TIMESTAMP, author_seen = 0 WHERE id = ?",
    )
    .bind(to)
    .bind(note)
    .bind(who.user_id)
    .bind(id)
    .execute(pool)
    .await
    {
        tracing::error!("Failed to record the decision on bundle {}: {}", id, e);
    }
}

/// Apply a bundle: every snapshot is compared first, then the requests are
/// replayed in order through the application's own router as the approving
/// manager -- so validation, webhooks and signals are exactly those of a direct
/// write -- and it stops at the first failure.
async fn approve(
    State(state): State<AppState>,
    Extension(who): Extension<Identity>,
    Path(id): Path<i64>,
    body: Option<Json<RejectBody>>,
) -> Response {
    // A note on an approval too: "übernommen, aber die Dauer gekürzt" is worth
    // telling the editor as much as a reason for a rejection.
    let approval_note = body.and_then(|Json(b)| b.note).filter(|n| !n.trim().is_empty());
    // Claimed in one statement: two managers pressing Freigeben at once must
    // not both replay the bundle.
    let claimed = sqlx::query("UPDATE changesets SET state = 'applying' WHERE id = ? AND state = 'submitted'")
        .bind(id)
        .execute(&state.pool)
        .await
        .map(|r| r.rows_affected())
        .unwrap_or(0);
    if claimed == 0 {
        return error(StatusCode::CONFLICT, "Dieser Vorschlag ist nicht (mehr) zur Freigabe eingereicht.");
    }
    let rows: Vec<(i64, i64, String, String, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, position, method, path, body, placeholder, before
         FROM change_requests WHERE changeset_id = ? ORDER BY position ASC",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    // 1. Nothing is applied when anything moved since it was proposed.
    for (_, position, method, path, _, _, before) in &rows {
        let Some(before) = before.as_deref().and_then(|b| serde_json::from_str::<Value>(b).ok()) else {
            continue;
        };
        let Ok(method) = method.parse::<Method>() else { continue };
        let Some(target) = super::target(&method, path) else { continue };
        if super::snapshot(&state, &target).await.as_ref() != Some(&before) {
            let note = format!("Änderung {} betrifft etwas, das seit dem Vorschlag geändert wurde.", position + 1);
            decide(&state.pool, id, "stale", &who, Some(&note)).await;
            return (StatusCode::CONFLICT, Json(json!({ "error": note, "state": "stale" }))).into_response();
        }
    }

    // 2. Replay, resolving placeholders as their creates return ids.
    let mut ids: HashMap<String, i64> = HashMap::new();
    for (request, position, method, path, body, placeholder, _) in &rows {
        if method == "UPLOAD" {
            if let Some(asset) = path.rsplit('/').next().and_then(|v| v.parse::<i64>().ok()) {
                let _ = sqlx::query("UPDATE assets SET pending_changeset = NULL WHERE id = ?")
                    .bind(asset).execute(&state.pool).await;
            }
            let _ = sqlx::query("UPDATE change_requests SET applied = 1 WHERE id = ?")
                .bind(request).execute(&state.pool).await;
            continue;
        }
        let Ok(parsed_method) = method.parse::<Method>() else { continue };
        let path = super::substitute_path(path, &ids);
        let mut body: Option<Value> = body.as_deref().and_then(|b| serde_json::from_str(b).ok());
        if let Some(value) = body.as_mut() {
            super::substitute(value, &ids);
        }
        let (status, answer) = super::internal(&state, who.clone(), parsed_method, &path, body.as_ref()).await;
        let result = json!({ "status": status.as_u16(), "body": answer });
        let _ = sqlx::query("UPDATE change_requests SET result = ?, applied = ? WHERE id = ?")
            .bind(result.to_string())
            .bind(status.is_success())
            .bind(request)
            .execute(&state.pool)
            .await;
        if !status.is_success() {
            let reason = answer.get("error").and_then(Value::as_str).unwrap_or("");
            let note = format!("Änderung {} ist fehlgeschlagen (HTTP {}) {}", position + 1, status.as_u16(), reason);
            decide(&state.pool, id, "failed", &who, Some(note.trim())).await;
            return (StatusCode::CONFLICT, Json(json!({ "error": note.trim(), "state": "failed" }))).into_response();
        }
        if let (Some(placeholder), Some(new_id)) = (placeholder, answer.get("id").and_then(Value::as_i64)) {
            ids.insert(placeholder.clone(), new_id);
        }
    }
    decide(&state.pool, id, "applied", &who, approval_note.as_deref()).await;
    Json(json!({ "state": "applied" })).into_response()
}

#[derive(Deserialize, Default)]
struct RejectBody {
    note: Option<String>,
}

async fn reject(
    State(state): State<AppState>,
    Extension(who): Extension<Identity>,
    Path(id): Path<i64>,
    body: Option<Json<RejectBody>>,
) -> Response {
    let note = body.and_then(|Json(b)| b.note);
    let changed = sqlx::query(
        "UPDATE changesets SET state = 'rejected', note = COALESCE(?, note), decided_by = ?,
             decided_at = CURRENT_TIMESTAMP, author_seen = 0
         WHERE id = ? AND state IN ('submitted', 'stale')",
    )
    .bind(note.as_deref())
    .bind(who.user_id)
    .bind(id)
    .execute(&state.pool)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);
    if changed == 0 {
        return error(StatusCode::CONFLICT, "Dieser Vorschlag kann nicht (mehr) abgelehnt werden.");
    }
    super::drop_pending_assets(&state, id).await;
    Json(json!({ "state": "rejected" })).into_response()
}

/// The caller's own bundles, newest first, with the reviewer's note and how
/// many decisions they have not looked at yet. Only ever the caller's: an
/// editor has no business reading another editor's proposals.
async fn mine(State(state): State<AppState>, Extension(who): Extension<Identity>) -> Response {
    let Some(author) = who.user_id else {
        return Json(json!({ "bundles": [], "unseen": 0 })).into_response();
    };
    let ids: Vec<(i64, bool)> = sqlx::query_as(
        "SELECT id, author_seen FROM changesets
         WHERE author_id = ? AND state != 'draft' AND author_hidden = 0
         ORDER BY COALESCE(decided_at, submitted_at, created_at) DESC, id DESC LIMIT 50",
    )
    .bind(author)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    let unseen = ids.iter().filter(|(_, seen)| !seen).count();
    let mut bundles = Vec::with_capacity(ids.len());
    for (id, seen) in ids {
        if let Some(mut b) = bundle(&state.pool, id).await {
            b["seen"] = json!(seen);
            bundles.push(b);
        }
    }
    Json(json!({ "bundles": bundles, "unseen": unseen })).into_response()
}

/// Pull one's own open bundle back into the draft, before anybody decides it.
async fn withdraw(State(state): State<AppState>, Extension(who): Extension<Identity>, Path(id): Path<i64>) -> Response {
    let Some(author) = who.user_id else {
        return error(StatusCode::FORBIDDEN, "Nur ein Konto kann Vorschläge zurückziehen.");
    };
    let own: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM changesets WHERE id = ? AND author_id = ? AND state = 'submitted'",
    )
    .bind(id)
    .bind(author)
    .fetch_optional(&state.pool)
    .await
    .unwrap_or(None);
    if own.is_none() {
        return error(StatusCode::CONFLICT, "Nur ein eigener, noch offener Vorschlag kann zurückgezogen werden.");
    }
    match super::reopen(&state.pool, author, id).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => {
            tracing::error!("Failed to withdraw bundle {}: {}", id, e);
            error(StatusCode::INTERNAL_SERVER_ERROR, "Zurückziehen fehlgeschlagen.")
        }
    }
}

/// Hide a decided bundle from its author's list. Hidden, not deleted: a
/// manager or admin still has who changed what, and when -- the trail is what
/// an approval workflow is for.
async fn hide(State(state): State<AppState>, Extension(who): Extension<Identity>, Path(id): Path<i64>) -> Response {
    let hidden = sqlx::query(
        "UPDATE changesets SET author_hidden = 1, author_seen = 1
         WHERE id = ? AND author_id = ? AND state IN ('applied', 'rejected', 'stale', 'failed')",
    )
    .bind(id)
    .bind(who.user_id)
    .execute(&state.pool)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);
    if hidden == 0 {
        return error(StatusCode::CONFLICT, "Nur ein eigener, entschiedener Vorschlag kann ausgeblendet werden.");
    }
    Json(json!({ "ok": true })).into_response()
}

async fn hide_decided(State(state): State<AppState>, Extension(who): Extension<Identity>) -> Response {
    let _ = sqlx::query(
        "UPDATE changesets SET author_hidden = 1, author_seen = 1
         WHERE author_id = ? AND state IN ('applied', 'rejected', 'stale', 'failed')",
    )
    .bind(who.user_id)
    .execute(&state.pool)
    .await;
    Json(json!({ "ok": true })).into_response()
}

async fn mine_seen(State(state): State<AppState>, Extension(who): Extension<Identity>) -> Response {
    if let Some(author) = who.user_id {
        let _ = sqlx::query("UPDATE changesets SET author_seen = 1 WHERE author_id = ?")
            .bind(author)
            .execute(&state.pool)
            .await;
    }
    Json(json!({ "ok": true })).into_response()
}
