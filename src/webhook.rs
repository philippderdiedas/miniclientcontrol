//! Outbound webhooks: the controller telling somebody else what happened.
//!
//! Not the picklecast `--webhook` returning. That was glue between two
//! processes and is dead because both sides live in one binary now; this tells
//! a *third party*, and nothing on the display path waits for it.

use serde_json::{json, Value};

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
}
