//! The operator's HTTP surface for webhooks: CRUD over the targets, the event
//! catalogue the admin page renders from, and a test send.
//!
//! Every route here is **operator-only**. None of these paths belongs in
//! `is_display_path` (which would open them to the whole LAN) or in
//! `cast::is_cast_public_path` (which would open them to every guest): a
//! target's headers are where an API token lives.

use std::collections::BTreeMap;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde_json::{json, Value};
use tracing::error;

use crate::models::AppState;
use super::{compile_check, load_enabled, Event, ALL_EVENTS};

/// Methods a webhook may use. `GET` is excluded because a `GET` with a body is
/// a contradiction, and every target here sends one.
const METHODS: [&str; 3] = ["POST", "PUT", "PATCH"];

/// A template -- the body or a header value -- comfortably above any real
/// use. The ceiling exists only so a pathological request cannot grow the
/// `webhooks` row without bound on a device where the SD card is the
/// component that dies; it is not a limit anyone should ever bump into.
const MAX_TEMPLATE_BYTES: usize = 16 * 1024;

/// Length-cap and compile-check one template, `label` naming it in the error
/// so a header failure says which header rather than just "a header".
fn check_template(label: &str, template: &str) -> Result<(), String> {
    if template.len() > MAX_TEMPLATE_BYTES {
        return Err(format!(
            "{label} ist zu groß (maximal {} KB).",
            MAX_TEMPLATE_BYTES / 1024
        ));
    }
    compile_check(template).map_err(|e| format!("{label}: {e}"))
}

/// Reject what could not work, before it is stored.
pub fn validate(
    name: &str,
    url: &str,
    method: &str,
    events: &[String],
    headers: &BTreeMap<String, String>,
    body: Option<&str>,
) -> Result<(), String> {
    // `record` and every log line identify a target by `name` alone -- a URL
    // may carry a token in its query string -- so an empty one leaves nothing
    // to trace a failure back to.
    if name.trim().is_empty() {
        return Err("Der Name darf nicht leer sein.".to_string());
    }
    let parsed = url::Url::parse(url).map_err(|e| format!("Ungültige URL: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("Nicht unterstütztes Schema: {}", parsed.scheme()));
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
    // `render` (in `mod.rs`) treats every header *value* as a minijinja
    // template just like the body, so a value that cannot compile must be
    // rejected here too -- otherwise it saves with a 200 and then fails on
    // every delivery at send time. The *name* is never templated, but hyper
    // rejects a malformed one at send time regardless, so check it here where
    // the operator gets a readable message instead of a cryptic dispatch log.
    for (header_name, value) in headers {
        if hyper::header::HeaderName::from_bytes(header_name.as_bytes()).is_err() {
            return Err(format!("Ungültiger Header-Name: {header_name}"));
        }
        check_template(&format!("Header '{header_name}'"), value)?;
    }
    if let Some(template) = body {
        if !template.is_empty() {
            check_template("Vorlage", template)?;
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

/// The full `/api/webhooks/events` payload: the catalogue plus how to turn one
/// of its entries into a placeholder, so the admin page never has to
/// hard-code the `data.` prefix or the envelope fields itself -- both are
/// facts about `envelope()` and `render()` in `mod.rs`, not about the UI.
///
/// `placeholder_suffix` is `| tojson`, and it belongs on **every** field, not
/// just the boolean one. A number happens to render as valid bare JSON and a
/// string breaks only on quoting, but "happens to" is exactly the drift this
/// endpoint exists to prevent, so the rule is published once rather than left
/// for the UI to special-case per field type. A chip is composed as
/// `{{ ` + `field_prefix` + field + `placeholder_suffix` + ` }}`, e.g.
/// `{{ data.reconnect | tojson }}`.
///
/// `test` is deliberately not offered here: `envelope()` only ever sets
/// `"test"` when it is `true`, so under `UndefinedBehavior::Lenient` a
/// `{{ test }}` chip would render as the empty string on every real
/// (non-test) delivery -- a placeholder that looks fine in the test send and
/// silently disappears from every delivery that matters.
fn events_payload() -> Value {
    json!({
        "events": catalogue(),
        "envelope": ["event", "timestamp", "device"],
        "field_prefix": "data.",
        "placeholder_suffix": " | tojson",
    })
}

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

/// Not `pub`: `routes()` is the module's entire interface, and nothing
/// outside it constructs one of these directly.
#[derive(serde::Deserialize)]
struct WebhookRequest {
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

/// A DB failure, not a rejected request -- the operator did nothing wrong, so
/// this must not read like `bad()`. The body shape still matches
/// `src/settings.rs`'s tone; only the status differs.
fn server_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "Interner Fehler, bitte erneut versuchen." })),
    )
        .into_response()
}

fn missing() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "Nicht gefunden." })),
    )
        .into_response()
}

async fn events() -> Response {
    Json(events_payload()).into_response()
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
    // Trimmed once here so the stored row and every later log line
    // (`Webhook '{name}'`) match what `validate` already treated as the name --
    // otherwise "  Foo  " passes the emptiness check and then sits in the
    // database and the logs with its padding intact.
    let payload = WebhookRequest {
        name: payload.name.trim().to_string(),
        ..payload
    };
    if let Err(message) = validate(
        &payload.name,
        &payload.url,
        &payload.method,
        &payload.events,
        &payload.headers,
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
            server_error()
        }
    }
}

async fn update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<WebhookRequest>,
) -> Response {
    // See `create`: trim before storing, not just before validating.
    let payload = WebhookRequest {
        name: payload.name.trim().to_string(),
        ..payload
    };
    if let Err(message) = validate(
        &payload.name,
        &payload.url,
        &payload.method,
        &payload.events,
        &payload.headers,
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
            server_error()
        }
    }
}

async fn remove(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    // Consistent with `update`: a DB failure here must not tell the operator
    // the row is gone when it is not, the same way a failed UPDATE does not
    // claim success. A missing row is still `{"ok":true}` -- deletion is
    // idempotent by design, so "no such row" and "deleted it" look the same.
    match sqlx::query("DELETE FROM webhooks WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await
    {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => {
            error!("Failed to delete webhook {}: {}", id, e);
            server_error()
        }
    }
}

/// Not `pub`, for the same reason as `WebhookRequest`.
#[derive(serde::Deserialize)]
struct TestRequest {
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

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn validation_rejects_what_cannot_work() {
        assert!(validate(
            "Test",
            "https://example.test/h",
            "POST",
            &["cast.started".into()],
            &BTreeMap::new(),
            None
        )
        .is_ok());
        assert!(
            validate("Test", "ftp://example.test/h", "POST", &[], &BTreeMap::new(), None).is_err(),
            "only http and https can be delivered to"
        );
        assert!(
            validate("Test", "https://example.test/h", "GET", &[], &BTreeMap::new(), None).is_err(),
            "a GET webhook with a body is a contradiction"
        );
        assert!(
            validate(
                "Test",
                "https://example.test/h",
                "POST",
                &["nope.invented".into()],
                &BTreeMap::new(),
                None
            )
            .is_err(),
            "an unknown event name would silently never fire"
        );
        assert!(
            validate(
                "Test",
                "https://example.test/h",
                "POST",
                &[],
                &BTreeMap::new(),
                Some("{{ unclosed ")
            )
            .is_err(),
            "a template that cannot compile must not be storable"
        );
        assert!(validate(
            "Test",
            "https://example.test/h",
            "PUT",
            &[],
            &BTreeMap::new(),
            Some("{{ event }}")
        )
        .is_ok());
        assert!(validate("Test", "https://example.test/h", "PATCH", &[], &BTreeMap::new(), None).is_ok());
    }

    #[test]
    fn an_empty_or_blank_name_is_rejected() {
        assert!(
            validate("", "https://example.test/h", "POST", &[], &BTreeMap::new(), None).is_err(),
            "an empty name leaves nothing for a log line to identify the target by"
        );
        assert!(
            validate("   ", "https://example.test/h", "POST", &[], &BTreeMap::new(), None).is_err(),
            "whitespace-only is empty in every way that matters"
        );
    }

    #[test]
    fn a_header_value_that_cannot_compile_is_rejected_by_name() {
        let err = validate(
            "Test",
            "https://example.test/h",
            "POST",
            &[],
            &headers(&[("X-Sig", "{{ unclosed ")]),
            None,
        )
        .unwrap_err();
        assert!(
            err.contains("X-Sig"),
            "the error must name the offending header, got: {err}"
        );
    }

    #[test]
    fn a_bad_header_name_is_rejected() {
        // A space is not a valid HTTP field-name character.
        let err = validate(
            "Test",
            "https://example.test/h",
            "POST",
            &[],
            &headers(&[("X Sig", "static")]),
            None,
        )
        .unwrap_err();
        assert!(err.contains("X Sig"), "got: {err}");
    }

    #[test]
    fn a_good_header_name_and_value_pass() {
        assert!(validate(
            "Test",
            "https://example.test/h",
            "POST",
            &[],
            &headers(&[("X-Event", "{{ event }}"), ("Authorization", "Bearer static")]),
            None
        )
        .is_ok());
    }

    #[test]
    fn an_oversized_body_template_is_rejected() {
        let huge = "x".repeat(MAX_TEMPLATE_BYTES + 1);
        assert!(
            validate("Test", "https://example.test/h", "POST", &[], &BTreeMap::new(), Some(&huge)).is_err()
        );
    }

    #[test]
    fn an_oversized_header_value_is_rejected() {
        let huge = "x".repeat(MAX_TEMPLATE_BYTES + 1);
        let err = validate(
            "Test",
            "https://example.test/h",
            "POST",
            &[],
            &headers(&[("X-Big", huge.as_str())]),
            None,
        )
        .unwrap_err();
        assert!(err.contains("X-Big"), "got: {err}");
    }

    #[test]
    fn the_events_payload_describes_how_to_build_a_placeholder() {
        let payload = events_payload();
        assert_eq!(payload["envelope"], json!(["event", "timestamp", "device"]));
        assert_eq!(payload["field_prefix"], "data.");
        assert_eq!(payload["placeholder_suffix"], " | tojson");

        // The composed form a chip actually inserts, pinned exactly: this is
        // the one place that proves `field_prefix` + field + `placeholder_suffix`
        // produces something minijinja renders as valid JSON for a boolean,
        // which a bare `{{ data.reconnect }}` does not.
        let field_prefix = payload["field_prefix"].as_str().unwrap();
        let placeholder_suffix = payload["placeholder_suffix"].as_str().unwrap();
        let placeholder = format!("{{{{ {field_prefix}reconnect{placeholder_suffix} }}}}");
        assert_eq!(placeholder, "{{ data.reconnect | tojson }}");

        let events = payload["events"].as_array().unwrap();
        let names: Vec<&str> = events.iter().map(|e| e["name"].as_str().unwrap()).collect();
        for name in ALL_EVENTS {
            assert!(names.contains(&name), "{name} is missing from the catalogue");
        }
        let item = events
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
        assert!(
            payload["events"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["name"] != "test"),
            "`test` is only ever present when true, so a lenient-undefined chip for it would render empty on every real delivery"
        );
    }

    #[test]
    fn the_catalogue_matches_all_events_exactly_in_both_directions() {
        let catalogue = catalogue();

        // No entry describes an event that does not exist.
        for entry in &catalogue {
            let name = entry["name"].as_str().unwrap();
            assert!(
                ALL_EVENTS.contains(&name),
                "{name} is in the catalogue but not in ALL_EVENTS"
            );
            assert!(
                !entry["description"].as_str().unwrap().is_empty(),
                "{name} has a blank description, which is the only text the UI shows for it"
            );
        }

        // Every event's actual `data()` keys match what the catalogue
        // advertises, in both directions -- a field rename in `mod.rs` that
        // forgets to update this file must fail a test, not ship silently.
        //
        // `cast.ended` and `guest_page.ended` have identical `data()` shapes
        // (`{reason, duration_secs}`), so the field comparison alone cannot
        // catch two arms of `sample_event` being swapped -- it would still
        // agree in both directions while a test send reported the wrong
        // event. The `.name()` assertion below is what catches that, and it
        // is also what makes the `_ =>` fallback arm in `sample_event`
        // honest: a new `ALL_EVENTS` entry with no matching arm falls into
        // `ItemChanged` and now fails loudly here instead of shipping quietly.
        for name in ALL_EVENTS {
            let entry = catalogue
                .iter()
                .find(|e| e["name"] == name)
                .unwrap_or_else(|| panic!("{name} is missing from the catalogue"));
            let advertised: std::collections::BTreeSet<String> = entry["fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f.as_str().unwrap().to_string())
                .collect();
            let sample = sample_event(name);
            assert_eq!(
                sample.name(),
                name,
                "sample_event({name}) built the wrong Event -- a test send for this event would report the wrong name"
            );
            let actual: std::collections::BTreeSet<String> = sample
                .data()
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            assert_eq!(
                advertised, actual,
                "{name}: catalogue fields and Event::data() keys disagree"
            );
        }
    }
}
