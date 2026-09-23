//! `GET`/`PUT /api/displays/{name}/schedule`.
//!
//! The timetable is a resource of its own: default and windows together are the
//! whole answer to "what does this screen play, when", with their own validation
//! and their own derived state. Operator-only -- in neither `is_display_path`
//! nor `cast::is_cast_public_path` -- which is also what allows the path to name
//! the screen.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{active, format_time, iso_from_mask, load, now, overlaps, validate, Window, WindowInput};
use crate::models::AppState;

/// The timetable as `GET` answers it and `GET /api/displays` embeds it.
pub(crate) async fn body(pool: &sqlx::SqlitePool, display: &str) -> Result<Value, sqlx::Error> {
    let (default, windows) = load(pool, display).await?;
    let live = active(default, &windows, now());
    Ok(json!({
        "default_playlist_id": default,
        "windows": windows.iter().map(|w| json!({
            "weekdays": iso_from_mask(w.weekdays),
            "from": format_time(w.start_minute),
            "to": format_time(w.end_minute),
            "playlist_id": w.playlist_id,
        })).collect::<Vec<_>>(),
        "overlaps": overlaps(&windows).into_iter().map(|(i, j)| [i, j]).collect::<Vec<_>>(),
        "now": { "playlist_id": live.playlist_id, "window": live.window },
    }))
}

fn bad_request(message: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message.into() }))).into_response()
}

// `screen`, not `display`: inside a `tracing` macro that name resolves to
// tracing's own `display()` helper rather than to the parameter.
fn unreadable(screen: &str, e: sqlx::Error) -> Response {
    tracing::error!("Failed to read the timetable of {}: {}", screen, e);
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "Zeitplan konnte nicht gelesen werden." })),
    )
        .into_response()
}

pub(crate) async fn get_schedule(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if let Err(response) = crate::display::known_display(&state, &name).await {
        return response;
    }
    match body(&state.pool, &name).await {
        Ok(value) => Json(value).into_response(),
        Err(e) => unreadable(&name, e),
    }
}

/// Both fields required. `Option` only so a missing one gets a sentence rather
/// than serde's `422`: a partial update of an ordered list has no good meaning,
/// and `default_playlist_id: null` ("no default") must not be confused with
/// "not sent".
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScheduleInput {
    #[serde(default, deserialize_with = "crate::handlers::double_option")]
    default_playlist_id: Option<Option<i64>>,
    windows: Option<Vec<WindowInput>>,
}

pub(crate) async fn put_schedule(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(input): Json<ScheduleInput>,
) -> Response {
    if let Err(response) = crate::display::known_display(&state, &name).await {
        return response;
    }
    let Some(default) = input.default_playlist_id else {
        return bad_request("default_playlist_id fehlt – null heißt „keine Standard-Playlist“.");
    };
    let Some(inputs) = input.windows else {
        return bad_request("windows fehlt – eine leere Liste heißt „kein Zeitfenster“.");
    };
    let windows = match validate(&inputs) {
        Ok(windows) => windows,
        Err(message) => return bad_request(message),
    };

    match write(&state.pool, &name, default, &windows).await {
        Ok(Written::Done) => {}
        Ok(Written::UnknownDefault) => return bad_request("Unbekannte Standard-Playlist."),
        Ok(Written::UnknownPlaylist(row)) => {
            return bad_request(format!("Zeile {row}: unbekannte Playlist."))
        }
        Err(e) => {
            tracing::error!("Failed to save the timetable of {}: {}", name, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Zeitplan konnte nicht gespeichert werden." })),
            )
                .into_response();
        }
    }

    // The loop re-resolves every pass, but poking it means a change to what is
    // live lands now rather than at the end of the item -- and it recomputes its
    // boundary timer from the new windows.
    if let Some(display) = state.display(&name) {
        display.playlist_signal.notify_one();
    }

    match body(&state.pool, &name).await {
        Ok(value) => Json(value).into_response(),
        Err(e) => unreadable(&name, e),
    }
}

enum Written {
    Done,
    UnknownDefault,
    /// Counted from 1, as the operator sees the rows.
    UnknownPlaylist(usize),
}

/// The whole timetable in one transaction. Every playlist is checked in the
/// statement that writes it, for the reason the item move does it: a check
/// before the write leaves a window for the playlist to go. An early return
/// drops `tx`, which rolls back -- the default included.
async fn write(
    pool: &sqlx::SqlitePool,
    display: &str,
    default: Option<i64>,
    windows: &[Window],
) -> Result<Written, sqlx::Error> {
    let mut tx = pool.begin().await?;
    // `assignment_decided` in the same statement, as every assignment has it: an
    // operator's "(keine)" is a decision, and a power loss must not tear the
    // record of it from what was chosen.
    let updated = sqlx::query(
        "UPDATE displays SET default_playlist_id = ?1, assignment_decided = 1
         WHERE name = ?2 AND (?1 IS NULL OR EXISTS (SELECT 1 FROM playlists WHERE id = ?1))",
    )
    .bind(default)
    .bind(display)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Ok(Written::UnknownDefault);
    }
    sqlx::query("DELETE FROM schedule_windows WHERE display = ?")
        .bind(display)
        .execute(&mut *tx)
        .await?;
    for (position, window) in windows.iter().enumerate() {
        let inserted = sqlx::query(
            "INSERT INTO schedule_windows
                 (display, position, weekdays, start_minute, end_minute, playlist_id)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6
             WHERE EXISTS (SELECT 1 FROM playlists WHERE id = ?6)",
        )
        .bind(display)
        .bind(position as i64)
        .bind(window.weekdays as i64)
        .bind(window.start_minute as i64)
        .bind(window.end_minute as i64)
        .bind(window.playlist_id)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            return Ok(Written::UnknownPlaylist(position + 1));
        }
    }
    tx.commit().await?;
    Ok(Written::Done)
}
