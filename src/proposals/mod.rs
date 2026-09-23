//! An editor's content writes, kept as a bundle until a manager applies it.
//!
//! Proposals are *recorded requests*, not a second write path: the request is
//! stored with a snapshot of the object it touches, and applying it replays it
//! through the application's own router (`api.rs`). The handlers stay as they
//! are. Uploads are the one exception -- a file cannot wait as JSON -- and are
//! stored at once as an asset marked pending.

pub mod api;

use std::collections::HashMap;
use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::accounts::middleware::ReplayIdentity;
use crate::accounts::{Identity, Role};
use crate::models::AppState;

/// What the snapshot of a write compares: which object it touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Item(String),
    Playlist(String),
    Asset(String),
    Schedule(String),
    /// A move reorders the item's whole playlist, so that order is the snapshot.
    Order(String),
    /// A create: nothing to compare, and a placeholder to hand out.
    Create,
}

/// The object a content write touches, from its method and path.
pub fn target(method: &Method, path: &str) -> Option<Target> {
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method.as_str(), segments.as_slice()) {
        ("POST", ["api", "playlists"]) | ("POST", ["api", "playlist"]) => Some(Target::Create),
        ("PUT" | "DELETE", ["api", "playlist", id]) => Some(Target::Item(id.to_string())),
        ("POST", ["api", "playlist", id, "move"]) => Some(Target::Order(id.to_string())),
        ("PUT" | "DELETE", ["api", "playlists", id]) => Some(Target::Playlist(id.to_string())),
        ("PUT" | "DELETE", ["api", "assets", id]) => Some(Target::Asset(id.to_string())),
        ("PUT", ["api", "displays", name, "schedule"]) => Some(Target::Schedule(name.to_string())),
        _ => None,
    }
}

pub fn is_placeholder(text: &str) -> bool {
    text.strip_prefix("new:").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Replace every placeholder in `value` -- any JSON string that is exactly one
/// -- with the id its create returned.
pub fn substitute(value: &mut Value, ids: &HashMap<String, i64>) {
    match value {
        Value::String(text) if is_placeholder(text) => {
            if let Some(id) = ids.get(text.as_str()) {
                *value = Value::from(*id);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| substitute(v, ids)),
        Value::Object(fields) => fields.values_mut().for_each(|v| substitute(v, ids)),
        _ => {}
    }
}

/// The same for the segments of a path.
pub fn substitute_path(path: &str, ids: &HashMap<String, i64>) -> String {
    path.split('/')
        .map(|segment| match ids.get(segment) {
            Some(id) if is_placeholder(segment) => id.to_string(),
            _ => segment.to_string(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Who internal reads and replays run as when no person is behind them.
fn system() -> Identity {
    Identity { user_id: None, name: "Vorschläge".into(), role: Role::Admin, open: false }
}

/// A request through the application's own router, as `who`. Carries the
/// `ConnectInfo` the auth middleware extracts -- it panics without one -- and a
/// `ReplayIdentity`, which no client can send.
pub async fn internal(
    state: &AppState,
    who: Identity,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let Some(router) = state.router.get() else {
        return (StatusCode::SERVICE_UNAVAILABLE, Value::Null);
    };
    let mut builder = Request::builder().method(method).uri(path);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let payload = body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty);
    let Ok(mut request) = builder.body(payload) else {
        return (StatusCode::BAD_REQUEST, Value::Null);
    };
    request.extensions_mut().insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))));
    request.extensions_mut().insert(ReplayIdentity(who));
    let response = match router.clone().oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap_or_default();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn read_list(state: &AppState, path: &str) -> Vec<Value> {
    match internal(state, system(), Method::GET, path, None).await {
        (status, Value::Array(rows)) if status.is_success() => rows,
        _ => Vec::new(),
    }
}

fn find_by_id(rows: &[Value], id: &str) -> Option<Value> {
    rows.iter().find(|row| row.get("id").map(|v| v.to_string()) == Some(id.to_string())).cloned()
}

/// The object `target` names, as its read route shows it now. `None` for a
/// create, and for an object that is itself a placeholder in the bundle.
pub async fn snapshot(state: &AppState, target: &Target) -> Option<Value> {
    match target {
        Target::Create => None,
        Target::Item(id) | Target::Playlist(id) | Target::Asset(id) | Target::Order(id)
            if is_placeholder(id) => None,
        Target::Item(id) => find_by_id(&read_list(state, "/api/playlist").await, id),
        Target::Playlist(id) => find_by_id(&read_list(state, "/api/playlists").await, id),
        Target::Asset(id) => find_by_id(&read_list(state, "/api/assets").await, id),
        Target::Order(id) => {
            let items = read_list(state, "/api/playlist").await;
            let playlist = find_by_id(&items, id)?.get("playlist_id").cloned()?;
            Some(Value::Array(
                items.iter()
                    .filter(|item| item.get("playlist_id") == Some(&playlist))
                    .filter_map(|item| item.get("id").cloned())
                    .collect(),
            ))
        }
        Target::Schedule(name) => {
            let (status, body) =
                internal(state, system(), Method::GET, &format!("/api/displays/{name}/schedule"), None).await;
            // Only what the operator set: `now` changes with the clock and
            // `overlaps` follows from the windows, so neither may make a
            // bundle stale.
            status.is_success().then(|| json!({
                "default_playlist_id": body.get("default_playlist_id"),
                "windows": body.get("windows"),
            }))
        }
    }
}

/// The author's draft, created if they have none.
pub async fn ensure_draft(pool: &sqlx::SqlitePool, author: i64) -> Result<i64, sqlx::Error> {
    if let Some(id) = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM changesets WHERE author_id = ? AND state = 'draft'",
    )
    .bind(author)
    .fetch_optional(pool)
    .await?
    {
        return Ok(id);
    }
    let inserted = sqlx::query("INSERT INTO changesets (author_id, state) VALUES (?, 'draft')")
        .bind(author)
        .execute(pool)
        .await?;
    Ok(inserted.last_insert_rowid())
}

pub async fn append(
    pool: &sqlx::SqlitePool,
    changeset: i64,
    method: &str,
    path: &str,
    body: Option<&Value>,
    placeholder: Option<&str>,
    before: Option<&Value>,
) -> Result<i64, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT INTO change_requests (changeset_id, position, method, path, body, placeholder, before)
         SELECT ?1, COALESCE(MAX(position), -1) + 1, ?2, ?3, ?4, ?5, ?6 FROM change_requests WHERE changeset_id = ?1",
    )
    .bind(changeset)
    .bind(method)
    .bind(path)
    .bind(body.map(|b| b.to_string()))
    .bind(placeholder)
    .bind(before.map(|b| b.to_string()))
    .execute(pool)
    .await?;
    Ok(inserted.last_insert_rowid())
}

fn refuse(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// An editor's content write: recorded into their draft instead of executed.
pub async fn intercept(state: &AppState, identity: &Identity, request: axum::extract::Request) -> Response {
    let Some(author) = identity.user_id else {
        return refuse(StatusCode::FORBIDDEN, "Nur ein Konto kann Änderungen vorschlagen.");
    };
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let Some(target) = target(&method, &path) else {
        return refuse(StatusCode::FORBIDDEN, "Diese Änderung kann nicht vorgeschlagen werden.");
    };
    let bytes = match axum::body::to_bytes(request.into_body(), 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => return refuse(StatusCode::PAYLOAD_TOO_LARGE, "Anfrage zu groß."),
    };
    let body: Option<Value> = if bytes.is_empty() {
        None
    } else {
        match serde_json::from_slice(&bytes) {
            Ok(value) => Some(value),
            Err(_) => return refuse(StatusCode::BAD_REQUEST, "Die Anfrage ist kein gültiges JSON."),
        }
    };

    // An object in the editor's own bundle that nobody has decided on yet: the
    // bundle comes back into the draft first, so the change merges with the one
    // already proposed instead of racing it. A manager never approves a bundle
    // that is still being changed.
    let mut reopened = None;
    if target != Target::Create {
        let pending: Option<i64> = sqlx::query_scalar(
            "SELECT c.id FROM changesets c JOIN change_requests r ON r.changeset_id = c.id
             WHERE c.author_id = ? AND c.state = 'submitted' AND r.path = ? LIMIT 1",
        )
        .bind(author)
        .bind(&path)
        .fetch_optional(&state.pool)
        .await
        .unwrap_or(None);
        if let Some(bundle) = pending {
            if let Err(e) = reopen(&state.pool, author, bundle).await {
                tracing::error!("Failed to bring bundle {} back into the draft: {}", bundle, e);
                return refuse(StatusCode::INTERNAL_SERVER_ERROR, "Vorschlag konnte nicht zurückgeholt werden.");
            }
            reopened = Some(bundle);
        }
    }

    let changeset = match ensure_draft(&state.pool, author).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Failed to open a draft: {}", e);
            return refuse(StatusCode::INTERNAL_SERVER_ERROR, "Entwurf konnte nicht angelegt werden.");
        }
    };

    match merge(&state.pool, changeset, &method, &path, &target, body.as_ref()).await {
        Ok(Some(change)) => {
            return (
                StatusCode::ACCEPTED,
                Json(json!({ "proposed": true, "change": change, "merged": true, "reopened": reopened })),
            )
                .into_response();
        }
        Ok(None) => {}
        Err(e) => {
            tracing::error!("Failed to merge a proposal: {}", e);
            return refuse(StatusCode::INTERNAL_SERVER_ERROR, "Vorschlag konnte nicht gespeichert werden.");
        }
    }
    let placeholder = if target == Target::Create {
        let creates: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM change_requests WHERE changeset_id = ? AND placeholder IS NOT NULL",
        )
        .bind(changeset)
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);
        Some(format!("new:{}", creates + 1))
    } else {
        None
    };
    let before = snapshot(state, &target).await;
    match append(&state.pool, changeset, method.as_str(), &path, body.as_ref(), placeholder.as_deref(), before.as_ref()).await {
        Ok(change) => (
            StatusCode::ACCEPTED,
            Json(json!({ "proposed": true, "change": change, "placeholder": placeholder, "reopened": reopened })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Failed to record a proposal: {}", e);
            refuse(StatusCode::INTERNAL_SERVER_ERROR, "Vorschlag konnte nicht gespeichert werden.")
        }
    }
}

fn merged_object(old: Option<Value>, new: Option<&Value>) -> Option<Value> {
    match (old, new) {
        (Some(Value::Object(mut fields)), Some(Value::Object(update))) => {
            for (key, value) in update {
                fields.insert(key.clone(), value.clone());
            }
            Some(Value::Object(fields))
        }
        (_, new) => new.cloned(),
    }
}

/// Fold a change into the draft when the draft already holds one for the same
/// object: one line per object for the reviewer, with the end state. A later
/// field overrides an earlier one, a delete replaces an edit, and a change to an
/// object the draft itself creates goes into that create. The first snapshot is
/// kept -- it is what the object was when the editor started. Moves are an
/// order of operations and are not merged. Returns the request it merged into.
async fn merge(
    pool: &sqlx::SqlitePool,
    changeset: i64,
    method: &Method,
    path: &str,
    target: &Target,
    body: Option<&Value>,
) -> Result<Option<i64>, sqlx::Error> {
    let id = match target {
        Target::Item(id) | Target::Playlist(id) | Target::Asset(id) | Target::Schedule(id) => id,
        Target::Order(_) | Target::Create => return Ok(None),
    };
    if is_placeholder(id) {
        let create: Option<(i64, Option<String>)> = sqlx::query_as(
            "SELECT id, body FROM change_requests WHERE changeset_id = ? AND placeholder = ?",
        )
        .bind(changeset)
        .bind(id)
        .fetch_optional(pool)
        .await?;
        let Some((create, old)) = create else { return Ok(None) };
        if method == Method::DELETE {
            sqlx::query("DELETE FROM change_requests WHERE id = ?").bind(create).execute(pool).await?;
        } else {
            let old = old.and_then(|b| serde_json::from_str(&b).ok());
            let merged = merged_object(old, body);
            sqlx::query("UPDATE change_requests SET body = ? WHERE id = ?")
                .bind(merged.map(|b| b.to_string()))
                .bind(create)
                .execute(pool)
                .await?;
        }
        return Ok(Some(create));
    }
    let existing: Option<(i64, String, Option<String>)> = sqlx::query_as(
        "SELECT id, method, body FROM change_requests
         WHERE changeset_id = ? AND path = ? AND method IN ('PUT', 'DELETE')",
    )
    .bind(changeset)
    .bind(path)
    .fetch_optional(pool)
    .await?;
    let Some((request, old_method, old_body)) = existing else { return Ok(None) };
    let (new_method, new_body) = if method == Method::DELETE {
        ("DELETE".to_string(), None)
    } else if old_method == "PUT" {
        let old = old_body.and_then(|b| serde_json::from_str(&b).ok());
        ("PUT".to_string(), merged_object(old, body))
    } else {
        (method.as_str().to_string(), body.cloned())
    };
    sqlx::query("UPDATE change_requests SET method = ?, body = ? WHERE id = ?")
        .bind(new_method)
        .bind(new_body.map(|b| b.to_string()))
        .bind(request)
        .execute(pool)
        .await?;
    Ok(Some(request))
}

fn rename_placeholders(value: &mut Value, names: &HashMap<String, String>) {
    match value {
        Value::String(text) => {
            if let Some(name) = names.get(text.as_str()) {
                *text = name.clone();
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| rename_placeholders(v, names)),
        Value::Object(fields) => fields.values_mut().for_each(|v| rename_placeholders(v, names)),
        _ => {}
    }
}

/// Bring the author's own submitted bundle back into their draft -- on
/// "Zurückziehen", and when they change an object it holds. With no draft the
/// bundle simply becomes it; otherwise its requests move after the draft's,
/// their placeholders renumbered past the draft's so `new:1` cannot mean two
/// things, and its pending uploads move with them.
pub async fn reopen(pool: &sqlx::SqlitePool, author: i64, bundle: i64) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let draft: Option<i64> =
        sqlx::query_scalar("SELECT id FROM changesets WHERE author_id = ? AND state = 'draft'")
            .bind(author)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(draft) = draft else {
        sqlx::query("UPDATE changesets SET state = 'draft', submitted_at = NULL WHERE id = ? AND author_id = ?")
            .bind(bundle)
            .bind(author)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(());
    };
    let (offset, creates): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(MAX(position), -1) + 1, COUNT(placeholder) FROM change_requests WHERE changeset_id = ?",
    )
    .bind(draft)
    .fetch_one(&mut *tx)
    .await?;
    let moved: Vec<(i64, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, path, body, placeholder FROM change_requests WHERE changeset_id = ? ORDER BY position",
    )
    .bind(bundle)
    .fetch_all(&mut *tx)
    .await?;
    let names: HashMap<String, String> = moved
        .iter()
        .filter_map(|(_, _, _, p)| p.clone())
        .filter_map(|p| {
            let n: i64 = p.strip_prefix("new:")?.parse().ok()?;
            Some((p, format!("new:{}", n + creates)))
        })
        .collect();
    for (id, path, body, placeholder) in moved {
        let path = path
            .split('/')
            .map(|s| names.get(s).cloned().unwrap_or_else(|| s.to_string()))
            .collect::<Vec<_>>()
            .join("/");
        let body = body.and_then(|b| serde_json::from_str::<Value>(&b).ok()).map(|mut v| {
            rename_placeholders(&mut v, &names);
            v.to_string()
        });
        let placeholder = placeholder.map(|p| names.get(&p).cloned().unwrap_or(p));
        sqlx::query(
            "UPDATE change_requests SET changeset_id = ?, position = position + ?, path = ?, body = ?, placeholder = ?
             WHERE id = ?",
        )
        .bind(draft)
        .bind(offset)
        .bind(path)
        .bind(body)
        .bind(placeholder)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE assets SET pending_changeset = ? WHERE pending_changeset = ?")
        .bind(draft)
        .bind(bundle)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM changesets WHERE id = ?").bind(bundle).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// An editor's upload: stored at once, marked pending, and recorded, so the
/// bundle shows it and applying it clears the mark.
pub async fn record_upload(pool: &sqlx::SqlitePool, author: i64, asset_id: i64, filename: &str) -> Result<(), sqlx::Error> {
    let changeset = ensure_draft(pool, author).await?;
    sqlx::query("UPDATE assets SET pending_changeset = ? WHERE id = ?")
        .bind(changeset)
        .bind(asset_id)
        .execute(pool)
        .await?;
    append(pool, changeset, "UPLOAD", &format!("/api/assets/{asset_id}"),
           Some(&json!({ "filename": filename })), None, None).await?;
    Ok(())
}

/// Delete a bundle's pending uploads, files included -- on reject and discard.
pub async fn drop_pending_assets(state: &AppState, changeset: i64) {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, local_path FROM assets WHERE pending_changeset = ?",
    )
    .bind(changeset)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    for (id, local_path) in rows {
        let _ = tokio::fs::remove_file(state.args.assets_dir.join(&local_path)).await;
        let _ = sqlx::query("DELETE FROM assets WHERE id = ?").bind(id).execute(&state.pool).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_replaced_in_paths_and_nested_json() {
        let ids: HashMap<String, i64> = [("new:2".to_string(), 41)].into_iter().collect();
        assert_eq!(substitute_path("/api/playlist/new:2/move", &ids), "/api/playlist/41/move");
        assert_eq!(substitute_path("/api/playlist/7", &ids), "/api/playlist/7");
        let mut body = json!({ "playlist_id": "new:2", "overlay": { "image_asset_id": "new:2" },
                               "url": "https://new:2.test/", "duration": 5, "list": ["new:2"] });
        substitute(&mut body, &ids);
        assert_eq!(body["playlist_id"], json!(41));
        assert_eq!(body["overlay"]["image_asset_id"], json!(41));
        assert_eq!(body["list"][0], json!(41));
        assert_eq!(body["url"], json!("https://new:2.test/"), "only a value that *is* a placeholder");
        assert_eq!(body["duration"], json!(5));
        let mut unknown = json!({ "playlist_id": "new:9" });
        substitute(&mut unknown, &ids);
        assert_eq!(unknown["playlist_id"], json!("new:9"));
    }

    #[test]
    fn a_write_path_names_the_object_it_touches() {
        assert_eq!(target(&Method::PUT, "/api/playlist/9"), Some(Target::Item("9".into())));
        assert_eq!(target(&Method::DELETE, "/api/playlist/9"), Some(Target::Item("9".into())));
        assert_eq!(target(&Method::POST, "/api/playlist/9/move"), Some(Target::Order("9".into())));
        assert_eq!(target(&Method::PUT, "/api/playlists/2"), Some(Target::Playlist("2".into())));
        assert_eq!(target(&Method::DELETE, "/api/assets/4"), Some(Target::Asset("4".into())));
        assert_eq!(target(&Method::PUT, "/api/displays/foyer/schedule"), Some(Target::Schedule("foyer".into())));
        assert_eq!(target(&Method::POST, "/api/playlists"), Some(Target::Create));
        assert_eq!(target(&Method::POST, "/api/playlist"), Some(Target::Create));
        assert_eq!(target(&Method::PUT, "/api/settings"), None);
        assert!(is_placeholder("new:12"));
        assert!(!is_placeholder("new:"));
        assert!(!is_placeholder("new:x"));
    }
}
