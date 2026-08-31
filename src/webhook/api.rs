//! The operator's HTTP surface for webhooks: CRUD over the targets, the event
//! catalogue the admin page renders from, and a test send.
//!
//! Every route here is **operator-only**. None of these paths belongs in
//! `is_display_path` (which would open them to the whole LAN) or in
//! `cast::is_cast_public_path` (which would open them to every guest): a
//! target's headers are where an API token lives.

use serde_json::{json, Value};
use std::collections::BTreeMap;

use super::{compile_check, ALL_EVENTS};

/// Methods a webhook may use. `GET` is excluded because a `GET` with a body is
/// a contradiction, and every target here sends one.
const METHODS: [&str; 3] = ["POST", "PUT", "PATCH"];

/// Reject what could not work, before it is stored.
pub fn validate(
    url: &str,
    method: &str,
    events: &[String],
    body: Option<&str>,
) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("Ungültige URL: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("Nicht unterstütztes Schema: {}", parsed.scheme()));
    }
    if parsed.host_str().is_none() {
        return Err("Die URL hat keinen Host.".to_string());
    }
    if !METHODS.contains(&method) {
        return Err(format!(
            "Methode muss POST, PUT oder PATCH sein, nicht {method}."
        ));
    }
    for event in events {
        if !ALL_EVENTS.contains(&event.as_str()) {
            return Err(format!("Unbekanntes Ereignis: {event}"));
        }
    }
    if let Some(template) = body {
        if !template.is_empty() {
            compile_check(template).map_err(|e| format!("Vorlage: {e}"))?;
        }
    }
    Ok(())
}

/// The event catalogue the admin page renders. The **only** source for it: a
/// page offering placeholders the server does not send is the overlay-preview
/// mistake in a new place.
pub fn catalogue() -> Vec<Value> {
    let entry = |name: &str, description: &str, fields: &[&str], chatty: bool| {
        json!({
            "name": name,
            "description": description,
            "fields": fields,
            "chatty": chatty,
        })
    };
    vec![
        entry(
            "playback.item_changed",
            "Ein Playlist-Element beginnt.",
            &["item_id", "kind", "title", "url", "duration"],
            true,
        ),
        entry(
            "playback.playlist_empty",
            "Die Playlist ist leer, der Ruhebildschirm läuft.",
            &[],
            false,
        ),
        entry(
            "override.set",
            "Etwas hat den Bildschirm übernommen.",
            &["url", "source"],
            false,
        ),
        entry(
            "override.cleared",
            "Die Übernahme ist beendet, die Playlist läuft weiter.",
            &["source"],
            false,
        ),
        entry(
            "cast.started",
            "Ein Gast überträgt seinen Bildschirm.",
            &["sender_ip", "mode"],
            false,
        ),
        entry(
            "cast.ended",
            "Die Übertragung ist beendet.",
            &["reason", "duration_secs"],
            false,
        ),
        entry(
            "guest_page.shown",
            "Ein Gast zeigt eine Webseite.",
            &["url", "sender_ip"],
            false,
        ),
        entry(
            "guest_page.ended",
            "Die Gast-Seite ist beendet.",
            &["reason", "duration_secs"],
            false,
        ),
        entry(
            "display.disconnected",
            "Die Verbindung zum Anzeige-Browser ist abgerissen.",
            &["error"],
            false,
        ),
        entry(
            "display.connected",
            "Der Anzeige-Browser ist verbunden.",
            &["reconnect"],
            false,
        ),
    ]
}

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use tracing::error;

use crate::models::AppState;
use super::{load_enabled, Event};

/// Operator-only, all four. See the module docs: nothing here may be added to
/// `is_display_path` or to `cast::is_cast_public_path`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/webhooks", get(list).post(create))
        // Registered before the `{id}` route only for readability; matchit
        // prefers the static segment either way, so `events` is never taken
        // for an id.
        .route("/api/webhooks/events", get(events))
        .route("/api/webhooks/{id}", put(update).delete(remove))
        .route("/api/webhooks/{id}/test", post(test_send))
}

#[derive(serde::Deserialize)]
pub struct WebhookRequest {
    name: String,
    url: String,
    #[serde(default = "default_method")]
    method: String,
    #[serde(default = "default_true")]
    is_enabled: bool,
    #[serde(default)]
    events: Vec<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    insecure_tls: bool,
}

fn default_method() -> String {
    "POST".to_string()
}
fn default_true() -> bool {
    true
}

fn bad(message: String) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

fn missing() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "Nicht gefunden." })),
    )
        .into_response()
}

async fn events() -> Response {
    Json(catalogue()).into_response()
}

async fn list(State(state): State<AppState>) -> Response {
    let rows = sqlx::query_as::<_, (i64, String, String, String, bool, String, String, Option<String>, bool)>(
        "SELECT id, name, url,
                COALESCE(method, 'POST'),
                COALESCE(is_enabled, 1),
                COALESCE(events, '[]'),
                COALESCE(headers, '{}'),
                body,
                COALESCE(insecure_tls, 0)
         FROM webhooks ORDER BY id ASC",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_else(|e| {
        error!("Failed to list webhooks: {}", e);
        Vec::new()
    });

    // Joined against the rows rather than returned as its own map: the
    // dispatcher's last-result map never evicts, so a deleted target's entry
    // outlives the row, and the response should only describe rows that exist.
    let last = state.webhooks.last_results().await;
    let out: Vec<Value> = rows
        .into_iter()
        .map(|(id, name, url, method, is_enabled, events, headers, body, insecure_tls)| {
            json!({
                "id": id,
                "name": name,
                "url": url,
                "method": method,
                "is_enabled": is_enabled,
                "events": serde_json::from_str::<Vec<String>>(&events).unwrap_or_default(),
                "headers": serde_json::from_str::<BTreeMap<String, String>>(&headers).unwrap_or_default(),
                "body": body,
                "insecure_tls": insecure_tls,
                "last_result": last.get(&id),
            })
        })
        .collect();
    Json(out).into_response()
}

async fn create(State(state): State<AppState>, Json(payload): Json<WebhookRequest>) -> Response {
    if let Err(message) = validate(
        &payload.url,
        &payload.method,
        &payload.events,
        payload.body.as_deref(),
    ) {
        return bad(message);
    }
    let result = sqlx::query_scalar::<_, i64>(
        "INSERT INTO webhooks (name, url, method, is_enabled, events, headers, body, insecure_tls)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(&payload.name)
    .bind(&payload.url)
    .bind(&payload.method)
    .bind(payload.is_enabled)
    .bind(serde_json::to_string(&payload.events).unwrap_or_else(|_| "[]".into()))
    .bind(serde_json::to_string(&payload.headers).unwrap_or_else(|_| "{}".into()))
    .bind(&payload.body)
    .bind(payload.insecure_tls)
    .fetch_one(&state.pool)
    .await;

    match result {
        Ok(id) => Json(json!({ "id": id })).into_response(),
        Err(e) => {
            error!("Failed to create webhook: {}", e);
            bad("Konnte nicht gespeichert werden.".to_string())
        }
    }
}

async fn update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<WebhookRequest>,
) -> Response {
    if let Err(message) = validate(
        &payload.url,
        &payload.method,
        &payload.events,
        payload.body.as_deref(),
    ) {
        return bad(message);
    }
    let result = sqlx::query(
        "UPDATE webhooks SET name = ?, url = ?, method = ?, is_enabled = ?,
                             events = ?, headers = ?, body = ?, insecure_tls = ?
         WHERE id = ?",
    )
    .bind(&payload.name)
    .bind(&payload.url)
    .bind(&payload.method)
    .bind(payload.is_enabled)
    .bind(serde_json::to_string(&payload.events).unwrap_or_else(|_| "[]".into()))
    .bind(serde_json::to_string(&payload.headers).unwrap_or_else(|_| "{}".into()))
    .bind(&payload.body)
    .bind(payload.insecure_tls)
    .bind(id)
    .execute(&state.pool)
    .await;

    match result {
        Ok(done) if done.rows_affected() == 0 => missing(),
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => {
            error!("Failed to update webhook {}: {}", id, e);
            bad("Konnte nicht gespeichert werden.".to_string())
        }
    }
}

async fn remove(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    if let Err(e) = sqlx::query("DELETE FROM webhooks WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await
    {
        error!("Failed to delete webhook {}: {}", id, e);
    }
    Json(json!({ "ok": true })).into_response()
}

#[derive(serde::Deserialize)]
pub struct TestRequest {
    #[serde(default)]
    event: Option<String>,
}

/// Render this target against a sample event and **deliver it for real**.
///
/// Not a dry run: a dry run proves the template compiles and nothing about
/// whether the receiver accepts it, which is the actual question.
async fn test_send(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<TestRequest>,
) -> Response {
    let target = match load_enabled(&state.pool)
        .await
        .into_iter()
        .find(|t| t.id == id)
    {
        Some(target) => target,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Nicht gefunden oder deaktiviert." })),
            )
                .into_response()
        }
    };
    let wanted = payload.event.as_deref().unwrap_or("playback.item_changed");
    if !ALL_EVENTS.contains(&wanted) {
        return bad(format!("Unbekanntes Ereignis: {wanted}"));
    }
    // The envelope is flagged `"test": true`, so a receiver can tell this from
    // something that actually happened -- and so can the admin page, through
    // `LastResult::test`.
    let outcome = state
        .webhooks
        .deliver_one(&target, &sample_event(wanted), true)
        .await;
    Json(json!({
        "ok": outcome.ok(),
        "outcome": outcome.describe(),
    }))
    .into_response()
}

/// A plausible instance of each event, for the test send.
fn sample_event(name: &str) -> Event {
    match name {
        "playback.playlist_empty" => Event::PlaylistEmpty,
        "override.set" => Event::OverrideSet {
            url: "http://127.0.0.1:3000/beispiel".into(),
            source: "operator",
        },
        "override.cleared" => Event::OverrideCleared { source: "operator" },
        "cast.started" => Event::CastStarted {
            sender_ip: "192.168.1.44".into(),
            mode: "cast".into(),
        },
        "cast.ended" => Event::CastEnded {
            reason: "operator",
            duration_secs: 312,
        },
        "guest_page.shown" => Event::GuestPageShown {
            url: "https://example.test/menu".into(),
            sender_ip: "192.168.1.44".into(),
        },
        "guest_page.ended" => Event::GuestPageEnded {
            reason: "grace",
            duration_secs: 84,
        },
        "display.disconnected" => Event::DisplayDisconnected {
            error: "the CDP connection was lost".into(),
        },
        "display.connected" => Event::DisplayConnected { reconnect: true },
        _ => Event::ItemChanged {
            item_id: 1,
            kind: "asset",
            title: "beispiel.jpg".into(),
            url: "http://127.0.0.1:3000/uploads/beispiel.jpg".into(),
            duration: 10,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rejects_what_cannot_work() {
        assert!(validate("https://example.test/h", "POST", &["cast.started".into()], None).is_ok());
        assert!(
            validate("ftp://example.test/h", "POST", &[], None).is_err(),
            "only http and https can be delivered to"
        );
        assert!(
            validate("https://example.test/h", "GET", &[], None).is_err(),
            "a GET webhook with a body is a contradiction"
        );
        assert!(
            validate("https://example.test/h", "POST", &["nope.invented".into()], None).is_err(),
            "an unknown event name would silently never fire"
        );
        assert!(
            validate("https://example.test/h", "POST", &[], Some("{{ unclosed ")).is_err(),
            "a template that cannot compile must not be storable"
        );
        assert!(validate("https://example.test/h", "PUT", &[], Some("{{ event }}")).is_ok());
        assert!(validate("https://example.test/h", "PATCH", &[], None).is_ok());
    }

    #[test]
    fn the_catalogue_describes_every_event_and_its_fields() {
        let catalogue = catalogue();
        let names: Vec<&str> = catalogue
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        for name in ALL_EVENTS {
            assert!(names.contains(&name), "{name} is missing from the catalogue");
        }
        let item = catalogue
            .iter()
            .find(|e| e["name"] == "playback.item_changed")
            .unwrap();
        let fields: Vec<&str> = item["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap())
            .collect();
        assert!(fields.contains(&"item_id"));
        assert!(fields.contains(&"title"));
        assert!(
            !item["description"].as_str().unwrap().is_empty(),
            "the UI shows this string"
        );
        assert_eq!(
            item["chatty"], true,
            "the UI warns beside this one specifically"
        );
    }
}
