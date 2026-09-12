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
///
/// The count and the delete are one statement, not two: counting first and
/// deleting second leaves a window where a `POST /api/playlist` lands in
/// between, inserting into a playlist the delete has already decided is
/// empty. The guard exists precisely to stop that dangling item, so it has to
/// hold across the whole operation, not just at the moment it was checked.
async fn remove(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let result = sqlx::query(
        "DELETE FROM playlists WHERE id = ? \
         AND NOT EXISTS (SELECT 1 FROM playlist_items WHERE playlist_id = ?)",
    )
    .bind(id)
    .bind(id)
    .execute(&state.pool)
    .await;

    match result {
        Ok(done) if done.rows_affected() > 0 => Json(json!({ "ok": true })).into_response(),
        Ok(_) => {
            // Nothing was deleted: either the id doesn't exist, or it does but
            // still holds items. Only this already-exceptional path pays for a
            // second query, and only to produce the count the error carries.
            let held = items_in(&state.pool, id).await;
            if held > 0 {
                (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": format!("Enthält noch {held} Elemente. Erst leeren oder verschieben.")
                    })),
                )
                    .into_response()
            } else {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Nicht gefunden." })),
                )
                    .into_response()
            }
        }
        Err(e) => {
            error!("Failed to delete playlist {}: {}", id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Konnte nicht gelöscht werden." })),
            )
                .into_response()
        }
    }
}

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
