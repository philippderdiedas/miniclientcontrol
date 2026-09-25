//! Content the controller renders itself -- a clock, a banner, a QR code, a
//! countdown -- as a widget in a layout or a standalone playlist item. Both go
//! through `resolve`, which encodes a self-contained payload into the URL of one
//! page, `web/widget.html`. The server is the only place that resolves a
//! built-in, so the QR/cast/locale it needs cannot drift from the overlay's.

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::models::AppState;

/// The four built-in kinds. Internally tagged by `kind`, so a `Source::Builtin`
/// object is `{ "kind": "clock", ... }`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Builtin {
    Clock {
        #[serde(default = "yes")]
        format_24h: bool,
        #[serde(default)]
        show_seconds: bool,
        #[serde(default)]
        show_date: bool,
        #[serde(default)]
        timezone: String,
        /// Text size in vmin, like the overlay's `size`. `None` fills the cell.
        #[serde(default)]
        text_size: Option<f32>,
        #[serde(flatten)]
        style: Style,
    },
    Banner {
        #[serde(default)]
        text: String,
        /// Text size in vmin. `None` is a headline that fills the cell on one
        /// line; a size is wrapped body text at that size.
        #[serde(default)]
        text_size: Option<f32>,
        /// For a fixed size (wrapped text): "center" | "left" | "right".
        #[serde(default = "center")]
        align: String,
        /// What happens when the text does not fit: "clip" (cut off) or
        /// "marquee" (a one-line ticker that scrolls it).
        #[serde(default = "clip")]
        overflow: String,
        #[serde(flatten)]
        style: Style,
    },
    Qr {
        /// "text" (use `qr_text`) or "cast" (the guest URL, resolved here).
        #[serde(default = "text_src")]
        source: String,
        #[serde(default)]
        qr_text: String,
        #[serde(default)]
        label: String,
        #[serde(flatten)]
        style: Style,
    },
    Countdown {
        /// ISO datetime the countdown targets.
        #[serde(default)]
        target: String,
        #[serde(default)]
        label: String,
        /// Shown once the target has passed.
        #[serde(default)]
        done_text: String,
        #[serde(default)]
        show_seconds: bool,
        #[serde(default)]
        show_ms: bool,
        /// Digital `DD:HH:MM:SS` instead of "3 T 6 Std 52 Min".
        #[serde(default)]
        digital: bool,
        /// Text size in vmin, like the overlay's `size`. `None` fills the cell.
        #[serde(default)]
        text_size: Option<f32>,
        #[serde(flatten)]
        style: Style,
    },
}

/// Shared look. Kept minimal on purpose -- background and text colour are the
/// whole style surface, so every built-in looks like it belongs to the display.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Style {
    #[serde(default = "black")]
    pub background_color: String,
    #[serde(default = "white")]
    pub text_color: String,
    /// A font family the device has offline: "sans" | "serif" | "mono".
    #[serde(default = "sans")]
    pub font: String,
}

impl Default for Style {
    fn default() -> Self {
        Self { background_color: black(), text_color: white(), font: sans() }
    }
}

fn yes() -> bool { true }
fn center() -> String { "center".into() }
fn clip() -> String { "clip".into() }
fn sans() -> String { "sans".into() }
fn text_src() -> String { "text".into() }
fn black() -> String { "#000000".into() }
fn white() -> String { "#ffffff".into() }

impl Builtin {
    /// Cap the free-text fields so a widget URL cannot grow without bound. Not an
    /// error: the display has nobody in front of it.
    pub fn sanitized(mut self) -> Self {
        let cap = |s: &mut String, n: usize| {
            if s.chars().count() > n {
                *s = s.chars().take(n).collect();
            }
        };
        // vmin, in the same range the overlay clamps its `size` to.
        let size = |s: &mut Option<f32>| {
            if let Some(v) = s {
                *v = v.clamp(0.5, 40.0);
            }
        };
        match &mut self {
            Builtin::Banner { text, text_size, .. } => {
                cap(text, 500);
                size(text_size);
            }
            Builtin::Qr { qr_text, label, .. } => {
                cap(qr_text, 500);
                cap(label, 100);
            }
            Builtin::Countdown { label, done_text, text_size, .. } => {
                cap(label, 100);
                cap(done_text, 100);
                size(text_size);
            }
            Builtin::Clock { text_size, .. } => size(text_size),
        }
        self
    }
}

/// The `/widget.html?c=...` URL whose payload the page renders, with the fields
/// only the server can supply (the QR matrix, the resolved guest URL, the
/// locale). `display` scopes a `cast`-source QR to one screen when known.
pub async fn resolve(
    state: &AppState,
    display: Option<&crate::models::Display>,
    b: &Builtin,
) -> String {
    let mut payload = serde_json::to_value(b).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(object) = payload.as_object_mut() {
        let (locale, cast_enabled, cast_qr_target) = {
            let s = state.settings.read().await;
            (s.locale.clone(), s.cast_enabled, s.cast_qr_target)
        };
        object.insert("locale".into(), serde_json::json!(locale));
        if let Builtin::Qr { source, qr_text, .. } = b {
            let target = if source == "cast" {
                // Nothing when casting is off -- advertising a share that would be
                // refused is worse than silence, exactly as in the overlay.
                if cast_enabled {
                    match cast_qr_target {
                        crate::settings::CastQrTarget::Screen => {
                            crate::cast::sender_url(state, display)
                        }
                        crate::settings::CastQrTarget::Chooser => {
                            crate::cast::sender_url(state, None)
                        }
                    }
                } else {
                    String::new()
                }
            } else {
                qr_text.clone()
            };
            object.insert("qr_modules".into(), serde_json::json!(crate::cast::qr_matrix(&target)));
        }
    }
    widget_url(state.args.port, &payload)
}

/// Encode a payload into the widget page's URL.
pub fn widget_url(port: u16, payload: &serde_json::Value) -> String {
    let json = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    let c = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json.as_bytes());
    format!("http://127.0.0.1:{port}/widget.html?c={c}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_kind() -> Vec<Builtin> {
        vec![
            Builtin::Clock {
                format_24h: true,
                show_seconds: true,
                show_date: true,
                timezone: "Europe/Berlin".into(),
                text_size: None,
                style: Style::default(),
            },
            Builtin::Banner {
                text: "Zu".into(),
                text_size: None,
                align: "center".into(),
                overflow: "clip".into(),
                style: Style::default(),
            },
            Builtin::Qr {
                source: "cast".into(),
                qr_text: String::new(),
                label: "Teilen".into(),
                style: Style::default(),
            },
            Builtin::Countdown {
                target: "2030-01-01T00:00:00".into(),
                label: "Bis".into(),
                done_text: "Vorbei".into(),
                show_seconds: true,
                show_ms: false,
                digital: false,
                text_size: None,
                style: Style::default(),
            },
        ]
    }

    #[test]
    fn each_kind_round_trips() {
        for b in every_kind() {
            let v = serde_json::to_value(&b).unwrap();
            assert_eq!(serde_json::from_value::<Builtin>(v).unwrap(), b);
        }
    }

    #[test]
    fn the_kind_tag_is_present_and_snake_case() {
        let v = serde_json::to_value(&every_kind()[0]).unwrap();
        assert_eq!(v["kind"], "clock");
        let v = serde_json::to_value(&every_kind()[3]).unwrap();
        assert_eq!(v["kind"], "countdown");
    }

    #[test]
    fn a_long_banner_is_capped() {
        let long = "x".repeat(1000);
        let Builtin::Banner { text, .. } = (Builtin::Banner {
            text: long,
            text_size: None,
            align: center(),
            overflow: clip(),
            style: Style::default(),
        })
        .sanitized() else {
            panic!("expected a banner");
        };
        assert_eq!(text.chars().count(), 500);
    }

    #[test]
    fn widget_url_is_the_widget_page_with_a_payload() {
        let url = widget_url(3000, &serde_json::json!({"kind": "clock"}));
        assert!(url.starts_with("http://127.0.0.1:3000/widget.html?c="));
        let c = url.split("c=").nth(1).unwrap();
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(c).unwrap();
        let back: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(back["kind"], "clock");
    }
}
