//! Outbound webhooks: the controller telling somebody else what happened.
//!
//! Not the picklecast `--webhook` returning. That was glue between two
//! processes and is dead because both sides live in one binary now; this tells
//! a *third party*, and nothing on the display path waits for it.

use minijinja::{AutoEscape, Environment, UndefinedBehavior};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tracing::error;

/// Every event name the catalogue offers, in the order the admin page shows them.
///
/// The UI reads this through `GET /api/webhooks/events` and never hard-codes a
/// copy: a page offering placeholders the server does not send is the
/// overlay-preview mistake in a new place.
pub const ALL_EVENTS: [&str; 10] = [
    "playback.item_changed",
    "playback.playlist_empty",
    "override.set",
    "override.cleared",
    "cast.started",
    "cast.ended",
    "guest_page.shown",
    "guest_page.ended",
    "display.disconnected",
    "display.connected",
];

/// Something worth telling somebody about.
///
/// Every URL in here is **already redacted** by the emit site
/// (`guest_page::redact`). The type takes `String`, not `Url`, so a caller
/// cannot accidentally hand over one carrying credentials.
#[derive(Debug, Clone)]
pub enum Event {
    ItemChanged {
        item_id: i64,
        kind: &'static str,
        title: String,
        url: String,
        duration: u64,
    },
    PlaylistEmpty,
    OverrideSet {
        url: String,
        source: &'static str,
    },
    OverrideCleared {
        source: &'static str,
    },
    CastStarted {
        sender_ip: String,
        mode: String,
    },
    CastEnded {
        reason: &'static str,
        duration_secs: i64,
    },
    GuestPageShown {
        url: String,
        sender_ip: String,
    },
    GuestPageEnded {
        reason: &'static str,
        duration_secs: i64,
    },
    DisplayDisconnected {
        error: String,
    },
    DisplayConnected {
        reconnect: bool,
    },
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Event::ItemChanged { .. } => "playback.item_changed",
            Event::PlaylistEmpty => "playback.playlist_empty",
            Event::OverrideSet { .. } => "override.set",
            Event::OverrideCleared { .. } => "override.cleared",
            Event::CastStarted { .. } => "cast.started",
            Event::CastEnded { .. } => "cast.ended",
            Event::GuestPageShown { .. } => "guest_page.shown",
            Event::GuestPageEnded { .. } => "guest_page.ended",
            Event::DisplayDisconnected { .. } => "display.disconnected",
            Event::DisplayConnected { .. } => "display.connected",
        }
    }

    pub fn data(&self) -> Value {
        match self {
            Event::ItemChanged { item_id, kind, title, url, duration } => json!({
                "item_id": item_id,
                "kind": kind,
                "title": title,
                "url": url,
                "duration": duration,
            }),
            Event::PlaylistEmpty => json!({}),
            Event::OverrideSet { url, source } => json!({ "url": url, "source": source }),
            Event::OverrideCleared { source } => json!({ "source": source }),
            Event::CastStarted { sender_ip, mode } => {
                json!({ "sender_ip": sender_ip, "mode": mode })
            }
            Event::CastEnded { reason, duration_secs } => {
                json!({ "reason": reason, "duration_secs": duration_secs })
            }
            Event::GuestPageShown { url, sender_ip } => {
                json!({ "url": url, "sender_ip": sender_ip })
            }
            Event::GuestPageEnded { reason, duration_secs } => {
                json!({ "reason": reason, "duration_secs": duration_secs })
            }
            Event::DisplayDisconnected { error } => json!({ "error": error }),
            Event::DisplayConnected { reconnect } => json!({ "reconnect": reconnect }),
        }
    }
}

/// The object a target receives, and the context a template renders against.
///
/// One shape for both, so a target with a template and a target without one see
/// identical data.
pub fn envelope(event: &Event, device: &str, test: bool) -> Value {
    let mut value = json!({
        "event": event.name(),
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "device": device,
        "data": event.data(),
    });
    if test {
        value["test"] = json!(true);
    }
    value
}

/// The machine's hostname, read once at startup.
///
/// Deliberately not a setting: a receiver needs to tell two displays apart and
/// the hostname already does that. Empty when it cannot be read — a webhook
/// must not be the reason a device fails to start.
pub fn device_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// One configured receiver.
///
/// `headers` is a `BTreeMap` so the order a target sends its headers in is
/// stable, which makes a test assertion possible and a log readable.
#[derive(Debug, Clone)]
pub struct Target {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub method: String,
    pub events: Vec<String>,
    pub headers: BTreeMap<String, String>,
    pub body: Option<String>,
    pub insecure_tls: bool,
}

impl Target {
    pub fn wants(&self, event_name: &str) -> bool {
        self.events.iter().any(|e| e == event_name)
    }
}

/// The enabled targets, read fresh on every event.
///
/// Not cached: an operator who disables a target expects the *next* event to
/// respect it, and this is a table with single-digit rows.
///
/// The JSON columns are `COALESCE`d because a real SQL `NULL` fails to decode
/// and would take the whole query with it -- one hand-written row would
/// otherwise silence every webhook.
pub async fn load_enabled(pool: &sqlx::SqlitePool) -> Vec<Target> {
    let rows = sqlx::query_as::<_, (i64, String, String, String, String, String, Option<String>, bool)>(
        "SELECT id, name, url,
                COALESCE(method, 'POST'),
                COALESCE(events, '[]'),
                COALESCE(headers, '{}'),
                body,
                COALESCE(insecure_tls, 0)
         FROM webhooks
         WHERE is_enabled = 1
         ORDER BY id ASC",
    )
    .fetch_all(pool)
    .await;

    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => {
            error!("Failed to read webhook targets: {}", e);
            return Vec::new();
        }
    };

    rows.into_iter()
        .map(|(id, name, url, method, events, headers, body, insecure_tls)| Target {
            id,
            name,
            url,
            method,
            events: serde_json::from_str(&events).unwrap_or_default(),
            headers: serde_json::from_str(&headers).unwrap_or_default(),
            body,
            insecure_tls,
        })
        .collect()
}

/// A target's payload, ready to send.
#[derive(Debug)]
pub struct Rendered {
    pub body: String,
    /// Lower-cased header names, so the content-type default cannot end up
    /// duplicated by a target that spelled it differently.
    pub headers: BTreeMap<String, String>,
}

/// A minijinja environment with this project's two non-default decisions.
fn environment() -> Environment<'static> {
    let mut env = Environment::new();
    // minijinja escapes for HTML by default, which turns `&` into `&amp;`
    // inside a JSON string. Nothing here is ever HTML.
    env.set_auto_escape_callback(|_| AutoEscape::None);
    // A field missing from *this* event must not fail the delivery -- a
    // template written for one event is routinely subscribed to another, and a
    // missing title cannot be allowed to silence a display-disconnected notice.
    env.set_undefined_behavior(UndefinedBehavior::Lenient);
    env
}

/// Compile a template without rendering it, for validation on save.
///
/// The only check possible up front: a template valid for one event and
/// nonsense for another is what the test send is for.
pub fn compile_check(template: &str) -> Result<(), String> {
    environment()
        .template_from_str(template)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Render a target's body and headers against one event.
pub fn render(target: &Target, context: &Value) -> Result<Rendered, String> {
    let env = environment();

    let body = match &target.body {
        Some(template) => env
            .render_str(template, context)
            .map_err(|e| format!("body template: {e}"))?,
        None => context.to_string(),
    };

    let mut headers = BTreeMap::new();
    for (name, template) in &target.headers {
        let value = env
            .render_str(template, context)
            .map_err(|e| format!("header {name}: {e}"))?;
        headers.insert(name.to_ascii_lowercase(), value);
    }
    headers
        .entry("content-type".to_string())
        .or_insert_with(|| "application/json".to_string());

    Ok(Rendered { body, headers })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_names_itself() {
        assert_eq!(Event::PlaylistEmpty.name(), "playback.playlist_empty");
        assert_eq!(
            Event::CastStarted { sender_ip: "192.168.1.44".into(), mode: "cast".into() }.name(),
            "cast.started"
        );
    }

    #[test]
    fn the_envelope_carries_event_timestamp_device_and_data() {
        let value = envelope(
            &Event::CastStarted { sender_ip: "192.168.1.44".into(), mode: "cast".into() },
            "foyer-pi",
            false,
        );
        assert_eq!(value["event"], "cast.started");
        assert_eq!(value["device"], "foyer-pi");
        assert_eq!(value["data"]["sender_ip"], "192.168.1.44");
        assert_eq!(value["data"]["mode"], "cast");
        assert!(value["timestamp"].as_str().unwrap().ends_with('Z'));
        assert!(value.get("test").is_none(), "a real delivery is not flagged as a test");
    }

    #[test]
    fn a_test_delivery_says_so() {
        let value = envelope(&Event::PlaylistEmpty, "foyer-pi", true);
        assert_eq!(value["test"], true);
    }

    #[test]
    fn a_url_with_credentials_is_redacted_in_the_payload() {
        let url = url::Url::parse("https://bob:hunter2@dash.example.test/panel").unwrap();
        let event = Event::GuestPageShown {
            url: crate::guest_page::redact(&url),
            sender_ip: "192.168.1.44".into(),
        };
        let text = event.data().to_string();
        assert!(!text.contains("hunter2"), "the password reached the payload: {text}");
        assert!(!text.contains("bob"), "the username reached the payload: {text}");
        assert!(text.contains("dash.example.test"));
    }

    #[test]
    fn the_catalogue_lists_every_variant_exactly_once() {
        let mut seen: Vec<&str> = ALL_EVENTS.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), ALL_EVENTS.len(), "ALL_EVENTS has a duplicate");
        assert_eq!(ALL_EVENTS.len(), 10);
    }

    fn target_wanting(events: &[&str]) -> Target {
        Target {
            id: 1,
            name: "Test".into(),
            url: "https://example.test/hook".into(),
            method: "POST".into(),
            events: events.iter().map(|e| e.to_string()).collect(),
            headers: Default::default(),
            body: None,
            insecure_tls: false,
        }
    }

    #[test]
    fn a_target_wants_only_the_events_it_subscribed_to() {
        let target = target_wanting(&["cast.started", "cast.ended"]);
        assert!(target.wants("cast.started"));
        assert!(!target.wants("playback.item_changed"));
        assert!(!target.wants("cast.startedX"), "matching must be exact, not a prefix");
    }

    #[test]
    fn a_target_subscribed_to_nothing_wants_nothing() {
        let target = target_wanting(&[]);
        for name in ALL_EVENTS {
            assert!(!target.wants(name));
        }
    }

    #[tokio::test]
    async fn load_enabled_skips_disabled_rows_and_decodes_json_columns() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();

        sqlx::query(
            "INSERT INTO webhooks (name, url, events, headers, body, is_enabled)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind("Discord")
        .bind("https://example.test/a")
        .bind(r#"["cast.started"]"#)
        .bind(r#"{"X-Token":"abc"}"#)
        .bind("{{ event }}")
        .bind(true)
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("INSERT INTO webhooks (name, url, is_enabled) VALUES (?, ?, ?)")
            .bind("Switched off")
            .bind("https://example.test/b")
            .bind(false)
            .execute(&pool)
            .await
            .unwrap();

        let targets = load_enabled(&pool).await;
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name, "Discord");
        assert_eq!(targets[0].events, vec!["cast.started".to_string()]);
        assert_eq!(targets[0].headers.get("X-Token").map(String::as_str), Some("abc"));
        assert_eq!(targets[0].body.as_deref(), Some("{{ event }}"));
        assert_eq!(targets[0].method, "POST", "the column default is POST");
    }

    #[tokio::test]
    async fn a_null_json_column_does_not_take_the_whole_query_down() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();

        // A row written by hand, or by an older version, can hold a real NULL.
        // One such row must not blank the whole list.
        sqlx::query("INSERT INTO webhooks (name, url, events, headers) VALUES (?, ?, NULL, NULL)")
            .bind("Hand-written")
            .bind("https://example.test/c")
            .execute(&pool)
            .await
            .unwrap();

        let targets = load_enabled(&pool).await;
        assert_eq!(targets.len(), 1, "the COALESCE on the read path is missing");
        assert!(targets[0].events.is_empty());
        assert!(targets[0].headers.is_empty());
    }

    fn ctx() -> serde_json::Value {
        envelope(
            &Event::GuestPageShown {
                url: "https://dash.example.test/a?x=1&y=2".into(),
                sender_ip: "192.168.1.44".into(),
            },
            "foyer-pi",
            false,
        )
    }

    #[test]
    fn no_template_sends_the_envelope_as_json() {
        let target = target_wanting(&["guest_page.shown"]);
        let rendered = render(&target, &ctx()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered.body).unwrap();
        assert_eq!(parsed["event"], "guest_page.shown");
        assert_eq!(
            rendered.headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
    }

    #[test]
    fn tojson_escapes_a_value_into_valid_json() {
        let mut target = target_wanting(&["guest_page.shown"]);
        // A title with a quote in it is what breaks the naive "{{ x }}" form.
        target.body = Some(r#"{"text": {{ data.url | tojson }}}"#.into());
        let rendered = render(&target, &ctx()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered.body)
            .expect("tojson must produce parseable JSON");
        assert_eq!(parsed["text"], "https://dash.example.test/a?x=1&y=2");
    }

    #[test]
    fn autoescape_is_off_so_an_ampersand_survives() {
        let mut target = target_wanting(&["guest_page.shown"]);
        target.body = Some("{{ data.url }}".into());
        let rendered = render(&target, &ctx()).unwrap();
        assert!(
            rendered.body.contains("x=1&y=2"),
            "HTML escaping turned & into &amp;: {}",
            rendered.body
        );
    }

    #[test]
    fn an_undefined_field_renders_empty_instead_of_failing() {
        let mut target = target_wanting(&["guest_page.shown"]);
        target.body = Some("title=[{{ data.title }}]".into());
        let rendered = render(&target, &ctx()).expect("a missing field must not fail the delivery");
        assert_eq!(rendered.body, "title=[]");
    }

    #[test]
    fn header_values_are_templates_too() {
        let mut target = target_wanting(&["guest_page.shown"]);
        target.headers.insert("X-Event".into(), "{{ event }}".into());
        target.headers.insert("Authorization".into(), "Bearer static-token".into());
        let rendered = render(&target, &ctx()).unwrap();
        assert_eq!(rendered.headers.get("x-event").map(String::as_str), Some("guest_page.shown"));
        assert_eq!(
            rendered.headers.get("authorization").map(String::as_str),
            Some("Bearer static-token")
        );
    }

    #[test]
    fn a_target_setting_its_own_content_type_keeps_it() {
        let mut target = target_wanting(&["guest_page.shown"]);
        target.body = Some("plain words".into());
        target.headers.insert("Content-Type".into(), "text/plain".into());
        let rendered = render(&target, &ctx()).unwrap();
        assert_eq!(rendered.headers.get("content-type").map(String::as_str), Some("text/plain"));
    }

    #[test]
    fn a_broken_template_is_an_error_with_a_position_not_a_panic() {
        let mut target = target_wanting(&["guest_page.shown"]);
        target.body = Some("{{ unclosed ".into());
        let error = render(&target, &ctx()).unwrap_err();
        assert!(!error.is_empty());
        assert!(compile_check("{{ unclosed ").is_err());
        assert!(compile_check("{{ data.url | tojson }}").is_ok());
    }
}
