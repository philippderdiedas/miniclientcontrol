//! Settings the operator can change at runtime, and how they relate to the
//! command line.
//!
//! ## The command line always wins
//!
//! A flag that was actually passed pins that setting: the admin UI shows it as
//! locked and refuses to change it. Anything not passed falls back to the stored
//! value and stays editable.
//!
//! Two reasons. The obvious one is that whoever wrote the unit file meant it.
//! The load-bearing one is recovery: an operator who enables basic auth in the
//! UI and then forgets the password would otherwise have locked themselves out
//! of the only place that can undo it. With this precedence, adding
//! `--basic-auth-user/--basic-auth-password` to the command line always gets
//! them back in.
//!
//! Passwords are stored as a PBKDF2 hash rather than plaintext, so a copy of the
//! database is not a copy of the credentials.

use std::num::NonZeroU32;

use base64::Engine;
use rand::Rng;
use ring::pbkdf2;
use serde::{Deserialize, Serialize};

use crate::models::{Args, CastAuth};

static PBKDF2_ALG: pbkdf2::Algorithm = pbkdf2::PBKDF2_HMAC_SHA256;
const PBKDF2_ITERATIONS: u32 = 120_000;

const KEY_CAST_ENABLED: &str = "cast_enabled";
const KEY_CAST_AUTH: &str = "cast_auth";
const KEY_CAST_CODE: &str = "cast_code";
const KEY_OVERLAY: &str = "overlay_config";
const KEY_LOCALE: &str = "locale";

/// Overlay images travel base64-encoded inside every apply, so this caps what one
/// costs. Generous for a logo, small enough that a re-apply stays cheap on a Pi.
pub const OVERLAY_IMAGE_MAX_BYTES: usize = 512 * 1024;
const KEY_AUTH_USER: &str = "basic_auth_user";
const KEY_AUTH_HASH: &str = "basic_auth_hash";

#[derive(Clone, Debug)]
pub struct AppSettings {
    pub cast_enabled: bool,
    pub cast_auth: CastAuth,
    pub cast_code: String,
    /// Basic-auth user. `None` means the operator UI is open.
    pub auth_user: Option<String>,
    /// PBKDF2 hash, or the marker for a plaintext password supplied on the CLI.
    pub auth_secret: Option<Secret>,
    /// The badge drawn on top of whatever is playing.
    pub overlay: Overlay,
    /// BCP-47 tag for the dates and times the display shows. Empty means
    /// "whatever the display browser would use on its own", which follows from
    /// `--browser-language`.
    pub locale: String,
}

/// What the overlay shows and where.
///
/// Stored as one JSON blob rather than a column per field: it is a handful of
/// presentation knobs that only ever travel together, and the display runtime is
/// the only thing that interprets them. The server validates the ranges it can
/// (a nonsense size or an unknown corner would be visible on the screen and
/// awkward to undo from a page nobody can read) and otherwise passes it through.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Overlay {
    pub enabled: bool,
    pub text: String,
    /// An uploaded asset, referenced the way the playlist references one.
    pub image_asset_id: Option<i64>,
    pub show_clock: bool,
    pub show_seconds: bool,
    pub show_date: bool,
    pub qr_text: String,
    /// `"text"` uses `qr_text`; `"cast"` uses the screen-share URL, resolved when
    /// the overlay is drawn.
    ///
    /// A resolved source rather than a typed one because that URL is not stable:
    /// `--public-url` decides its shape, an occupied `--cast-tls-port` moves it to
    /// the next free port, and the LAN address changes on a DHCP lease. A typed
    /// copy would go quietly wrong on a screen nobody is checking.
    pub qr_source: String,
    pub qr_label: String,
    pub position: String,
    /// vmin, so one setting reads the same on a 1080p panel and a portrait 4K one.
    pub size: f32,
    pub margin: f32,
    pub max_width: f32,
    pub qr_size: f32,
    /// Box background, as a colour and an alpha rather than a CSS string: the
    /// browser drops an invalid declaration without a word, so a typo in a string
    /// would mean a box with no background and no error anywhere.
    pub background_color: String,
    pub background_alpha: f32,
    /// No box at all -- the content sits directly on the page. Its own flag
    /// rather than alpha 0, because it also drops the padding and the corners.
    pub plain: bool,
    /// Escape hatch, used verbatim when set: gradients and the like. Validated to
    /// be a *value* and nothing more, so it cannot end the declaration and
    /// rewrite the rest of the box's stylesheet.
    pub background_css: String,
    pub color: String,
    pub color_alpha: f32,
    /// Legacy `rgba(...)`/hex string, migrated into the fields above on load and
    /// never written again.
    #[serde(skip_serializing)]
    pub background: Option<String>,
    /// Legacy whole-box opacity. Folded into the two alphas: one word for two
    /// different things is a trap, and washed-out text on signage is rarely what
    /// anybody wanted.
    #[serde(skip_serializing)]
    pub opacity: Option<f32>,
}

impl Default for Overlay {
    fn default() -> Self {
        Self {
            enabled: false,
            text: String::new(),
            image_asset_id: None,
            show_clock: false,
            show_seconds: false,
            show_date: false,
            qr_text: String::new(),
            qr_source: "text".to_string(),
            qr_label: String::new(),
            position: "bottom-right".to_string(),
            size: 2.4,
            margin: 3.0,
            max_width: 40.0,
            qr_size: 14.0,
            background_color: "#000000".to_string(),
            background_alpha: 0.65,
            plain: false,
            background_css: String::new(),
            color: "#ffffff".to_string(),
            color_alpha: 1.0,
            background: None,
            opacity: None,
        }
    }
}

/// A playlist item's own overlay, drawn *in addition* to the global one.
///
/// Content and a corner only: colours, sizes and opacity come from the global
/// overlay, so a display does not change character item by item and the playlist
/// card stays small enough to edit next to everything else on it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ItemOverlay {
    pub enabled: bool,
    pub text: String,
    pub image_asset_id: Option<i64>,
    pub qr_text: String,
    pub qr_label: String,
    /// Empty means "wherever the global overlay is", which puts both in one box.
    pub position: String,
}

impl ItemOverlay {
    pub fn sanitized(mut self) -> Self {
        if !self.position.is_empty() && !OVERLAY_POSITIONS.contains(&self.position.as_str()) {
            self.position = String::new();
        }
        self.text = self.text.chars().take(500).collect();
        self.qr_text = self.qr_text.chars().take(500).collect();
        self.qr_label = self.qr_label.chars().take(100).collect();
        self
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
            && self.image_asset_id.is_none()
            && self.qr_text.trim().is_empty()
    }

    pub fn draws(&self) -> bool {
        self.enabled && !self.is_empty()
    }
}

fn is_hex_colour(value: &str) -> bool {
    let Some(digits) = value.strip_prefix('#') else {
        return false;
    };
    matches!(digits.len(), 3 | 6 | 8) && digits.chars().all(|c| c.is_ascii_hexdigit())
}

/// A colour as `(#rrggbb, alpha)`, for the shapes an operator could plausibly
/// have typed into the old free-text field.
///
/// `None` means "not a plain colour" -- a gradient, say -- and the caller keeps
/// the string as the escape-hatch value rather than discarding what was on
/// screen.
fn parse_colour(raw: &str) -> Option<(String, f32)> {
    let raw = raw.trim();

    if let Some(digits) = raw.strip_prefix('#') {
        if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let expand = |c: char| {
            let d = c.to_digit(16)? as u8;
            Some(d * 17)
        };
        let (rgb, alpha) = match digits.len() {
            3 => {
                let mut it = digits.chars();
                let r = expand(it.next()?)?;
                let g = expand(it.next()?)?;
                let b = expand(it.next()?)?;
                ([r, g, b], 1.0)
            }
            6 | 8 => {
                let byte = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).ok();
                let rgb = [byte(0)?, byte(2)?, byte(4)?];
                let alpha = if digits.len() == 8 {
                    byte(6)? as f32 / 255.0
                } else {
                    1.0
                };
                (rgb, alpha)
            }
            _ => return None,
        };
        return Some((format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]), alpha));
    }

    let lower = raw.to_ascii_lowercase();
    let body = lower
        .strip_prefix("rgba(")
        .or_else(|| lower.strip_prefix("rgb("))?
        .strip_suffix(')')?;
    let parts: Vec<&str> = body.split(',').map(str::trim).collect();
    if parts.len() < 3 || parts.len() > 4 {
        return None;
    }
    let channel = |part: &str| -> Option<u8> {
        // Percentages exist in CSS but nobody typed them into this field, and
        // guessing wrong is worse than falling back to the escape hatch.
        part.parse::<f32>().ok().map(|v| v.clamp(0.0, 255.0) as u8)
    };
    let r = channel(parts[0])?;
    let g = channel(parts[1])?;
    let b = channel(parts[2])?;
    let alpha = match parts.get(3) {
        Some(value) => value.parse::<f32>().ok()?.clamp(0.0, 1.0),
        None => 1.0,
    };
    Some((format!("#{:02x}{:02x}{:02x}", r, g, b), alpha))
}

/// Keep a raw CSS value a *value*.
///
/// It is interpolated into a declaration inside the overlay's own stylesheet, so
/// a `;` or a `}` would let whoever typed it rewrite the rest of the box. That is
/// only ever their own box -- shadow styles do not reach the page -- but a
/// stylesheet nobody can see breaking is worth not allowing at all.
fn sanitize_css_value(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let forbidden = |c: char| matches!(c, ';' | '{' | '}' | '<' | '>' | '@' | '\\' | '"' | '\'');
    if trimmed.len() > 200 || trimmed.contains(forbidden) || trimmed.contains("*/") {
        return String::new();
    }
    trimmed.to_string()
}

pub const OVERLAY_POSITIONS: &[&str] = &[
    "top-left", "top-right", "bottom-left", "bottom-right", "top-center", "bottom-center",
];

impl Overlay {
    /// Clamp what would otherwise be visible nonsense on a screen nobody is
    /// standing in front of. Deliberately forgiving: out-of-range numbers are
    /// pulled into range rather than rejected, because an overlay that is a bit
    /// too big still beats a 400 the operator has to decode.
    pub fn sanitized(mut self) -> Self {
        if !OVERLAY_POSITIONS.contains(&self.position.as_str()) {
            self.position = "bottom-right".to_string();
        }
        if !matches!(self.qr_source.as_str(), "text" | "cast") {
            self.qr_source = "text".to_string();
        }
        self.size = self.size.clamp(0.5, 20.0);
        self.margin = self.margin.clamp(0.0, 40.0);
        self.max_width = self.max_width.clamp(5.0, 100.0);
        self.qr_size = self.qr_size.clamp(4.0, 60.0);
        self.migrate_legacy_style();
        if !is_hex_colour(&self.background_color) {
            self.background_color = "#000000".to_string();
        }
        if !is_hex_colour(&self.color) {
            self.color = "#ffffff".to_string();
        }
        self.background_alpha = self.background_alpha.clamp(0.0, 1.0);
        // Not down to zero: invisible text is indistinguishable from a broken
        // overlay, and "no box" already has its own switch.
        self.color_alpha = self.color_alpha.clamp(0.1, 1.0);
        self.background_css = sanitize_css_value(&self.background_css);
        self.text = self.text.chars().take(500).collect();
        self.qr_text = self.qr_text.chars().take(500).collect();
        self.qr_label = self.qr_label.chars().take(100).collect();
        self
    }

    /// Fold a stored `background`/`opacity` from before the split into the
    /// current fields, so nobody's configuration silently resets to the default.
    ///
    /// Only when the new fields are untouched: an operator who has since set a
    /// colour has made the newer decision.
    fn migrate_legacy_style(&mut self) {
        let legacy = self.background.take();
        let opacity = self.opacity.take();
        let defaults = Self::default();
        if self.background_color != defaults.background_color
            || (self.background_alpha - defaults.background_alpha).abs() > f32::EPSILON
            || !self.background_css.is_empty()
            || self.plain
        {
            return;
        }

        if let Some(raw) = legacy {
            let raw = raw.trim();
            if raw.eq_ignore_ascii_case("transparent") {
                self.plain = true;
            } else if let Some((hex, alpha)) = parse_colour(raw) {
                self.background_color = hex;
                self.background_alpha = alpha;
            } else if !raw.is_empty() {
                // Something exotic (a gradient, say): keep it as the escape hatch
                // rather than throwing away what was on screen.
                self.background_css = raw.to_string();
            }
        }
        if let Some(opacity) = opacity {
            // The old knob dimmed the whole box. The background alpha is the
            // closest single home for it, and the text stays readable.
            self.background_alpha = (self.background_alpha * opacity).clamp(0.0, 1.0);
        }
    }

    /// True when the overlay would draw nothing at all. An enabled overlay with
    /// no content is a switch that looks on and does nothing, which is worth
    /// telling the operator about rather than shipping to the display.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
            && self.image_asset_id.is_none()
            && !self.show_clock
            && !self.show_date
            && !self.draws_qr()
    }

    /// The cast source needs no text of its own, so "has a QR" is not the same
    /// question as "has qr_text".
    pub fn draws_qr(&self) -> bool {
        match self.qr_source.as_str() {
            "cast" => true,
            _ => !self.qr_text.trim().is_empty(),
        }
    }
}

/// Either a hash read from the database, or a password handed over on the
/// command line. Kept apart so the CLI case avoids a PBKDF2 round per request.
#[derive(Clone, Debug)]
pub enum Secret {
    Plain(String),
    Hash(String),
}

impl Secret {
    pub fn verify(&self, password: &str) -> bool {
        match self {
            Secret::Plain(expected) => constant_time_eq(expected.as_bytes(), password.as_bytes()),
            Secret::Hash(encoded) => verify_hash(encoded, password),
        }
    }
}

/// Which settings the command line has pinned. Reported to the admin UI so a
/// locked control can be shown as locked instead of silently ignoring edits.
#[derive(Clone, Copy, Debug, Serialize, Default)]
pub struct Locks {
    pub cast_enabled: bool,
    pub cast_auth: bool,
    pub cast_code: bool,
    pub basic_auth: bool,
    pub locale: bool,
}

impl Locks {
    pub fn from_args(args: &Args) -> Self {
        Self {
            // --disable-cast is a hard switch: it decides whether the listener
            // binds at all, so it cannot be undone from the UI either way.
            cast_enabled: args.disable_cast,
            cast_auth: args.cast_auth.is_some(),
            cast_code: args.cast_code.is_some(),
            basic_auth: args.basic_auth_user.is_some(),
            locale: args.locale.is_some(),
        }
    }
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

pub fn hash_password(password: &str) -> String {
    let salt: [u8; 16] = rand::rng().random();
    let mut derived = [0u8; 32];
    pbkdf2::derive(
        PBKDF2_ALG,
        NonZeroU32::new(PBKDF2_ITERATIONS).expect("non-zero"),
        &salt,
        password.as_bytes(),
        &mut derived,
    );
    format!(
        "pbkdf2-sha256${}${}${}",
        PBKDF2_ITERATIONS,
        b64(&salt),
        b64(&derived)
    )
}

fn verify_hash(encoded: &str, password: &str) -> bool {
    let mut parts = encoded.split('$');
    if parts.next() != Some("pbkdf2-sha256") {
        return false;
    }
    let Some(iterations) = parts.next().and_then(|raw| raw.parse::<u32>().ok()) else {
        return false;
    };
    let (Some(salt), Some(expected)) = (
        parts.next().and_then(unb64),
        parts.next().and_then(unb64),
    ) else {
        return false;
    };
    let Some(iterations) = NonZeroU32::new(iterations) else {
        return false;
    };
    pbkdf2::verify(PBKDF2_ALG, iterations, &salt, password.as_bytes(), &expected).is_ok()
}

/// The locale the surrounding system is configured for, as a BCP-47 tag.
///
/// POSIX spells it `de_DE.UTF-8@euro` where `Intl` wants `de-DE`. `C` and `POSIX`
/// mean "no locale chosen", which is not worth passing on: the display browser's
/// own default is a better answer than forcing one.
pub fn system_locale() -> Option<String> {
    let raw = ["LC_ALL", "LC_TIME", "LANG"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|value| !value.is_empty())?;

    let tag = raw
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .replace('_', "-");

    (!tag.is_empty() && tag != "C" && tag != "POSIX").then_some(tag)
}

/// Accepts a language tag and nothing that could pass for markup or a second
/// value: this string is handed to `Intl` inside the displayed page.
fn is_locale_tag(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 35
        && raw.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && raw.starts_with(|c: char| c.is_ascii_alphabetic())
}

pub fn parse_cast_auth(raw: &str) -> Option<CastAuth> {
    match raw {
        "none" => Some(CastAuth::None),
        "code" => Some(CastAuth::Code),
        "pairing" => Some(CastAuth::Pairing),
        _ => None,
    }
}

pub fn cast_auth_key(auth: CastAuth) -> &'static str {
    match auth {
        CastAuth::None => "none",
        CastAuth::Code => "code",
        CastAuth::Pairing => "pairing",
    }
}

/// Stored values, with anything given on the command line taking precedence.
pub async fn load(pool: &sqlx::SqlitePool, args: &Args) -> AppSettings {
    let stored_enabled = crate::db::load_setting(pool, KEY_CAST_ENABLED)
        .await
        .map(|raw| raw == "true")
        .unwrap_or(true);
    let stored_auth = crate::db::load_setting(pool, KEY_CAST_AUTH)
        .await
        .and_then(|raw| parse_cast_auth(&raw));
    let stored_code = crate::db::load_setting(pool, KEY_CAST_CODE).await;
    let stored_locale = crate::db::load_setting(pool, KEY_LOCALE).await;
    let stored_overlay = crate::db::load_setting(pool, KEY_OVERLAY)
        .await
        .and_then(|raw| serde_json::from_str::<Overlay>(&raw).ok())
        .unwrap_or_default()
        .sanitized();
    let stored_user = crate::db::load_setting(pool, KEY_AUTH_USER).await;
    let stored_hash = crate::db::load_setting(pool, KEY_AUTH_HASH).await;

    let (auth_user, auth_secret) = match (&args.basic_auth_user, &args.basic_auth_password) {
        (Some(user), Some(password)) => (Some(user.clone()), Some(Secret::Plain(password.clone()))),
        _ => match (stored_user, stored_hash) {
            (Some(user), Some(hash)) if !user.is_empty() => (Some(user), Some(Secret::Hash(hash))),
            _ => (None, None),
        },
    };

    AppSettings {
        // --disable-cast forces off; otherwise the stored switch decides.
        cast_enabled: !args.disable_cast && stored_enabled,
        cast_auth: args.cast_auth.or(stored_auth).unwrap_or(CastAuth::None),
        cast_code: args
            .cast_code
            .clone()
            .or(stored_code)
            .unwrap_or_default(),
        auth_user,
        auth_secret,
        overlay: stored_overlay,
        // Command line, then what the operator stored, then the system this runs
        // on. The last step is the point: a box already set up in German should
        // not have to be told a second time.
        locale: args
            .locale
            .clone()
            .filter(|tag| is_locale_tag(tag))
            .or_else(|| stored_locale.filter(|tag| is_locale_tag(tag)))
            .or_else(system_locale)
            .unwrap_or_default(),
    }
}

/// Write the editable settings back. Values pinned by the command line are
/// written too, so removing the flag later leaves the operator's intent intact.
pub async fn persist(pool: &sqlx::SqlitePool, settings: &AppSettings) {
    let mut rows = vec![
        (KEY_CAST_ENABLED, settings.cast_enabled.to_string()),
        (KEY_CAST_AUTH, cast_auth_key(settings.cast_auth).to_string()),
        (KEY_CAST_CODE, settings.cast_code.clone()),
        (KEY_LOCALE, settings.locale.clone()),
        (
            KEY_AUTH_USER,
            settings.auth_user.clone().unwrap_or_default(),
        ),
        (
            KEY_OVERLAY,
            serde_json::to_string(&settings.overlay).unwrap_or_default(),
        ),
    ];
    // Only a hash is ever written; a CLI password stays out of the database.
    if let Some(Secret::Hash(hash)) = &settings.auth_secret {
        rows.push((KEY_AUTH_HASH, hash.clone()));
    } else if settings.auth_user.is_none() {
        rows.push((KEY_AUTH_HASH, String::new()));
    }

    // Swallow-and-log, like every other write here: a failed settings write must
    // not take down the request, let alone the display.
    for (key, value) in rows {
        if let Err(e) = crate::db::save_setting(pool, key, &value).await {
            tracing::error!("Failed to persist {}: {}", key, e);
        }
    }
}

// -------------------------------------------------------------------- http

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::models::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/settings", get(read_settings).put(update_settings))
        .route("/api/overlay", get(read_overlay))
}

/// The overlay as the display runtime wants it: asset ids already resolved to
/// URLs, because the page has no way to look one up.
///
/// Includes the layer of the item *currently on screen*, so the admin preview
/// shows what is really out there rather than the global half of it.
pub async fn read_overlay(State(state): State<AppState>) -> impl IntoResponse {
    let current = *state.current_item_id.lock().await;
    let item = match current {
        Some(id) => crate::db::load_item_overlay(&state.pool, id).await,
        None => None,
    };
    Json(overlay_payload(&state, item.as_ref()).await)
}

/// The layers handed to `__ov.apply()`, on the display and in the preview.
///
/// Built in one place so the operator's preview and the screen cannot disagree:
/// two renderings of the same settings that drift apart would send someone
/// hunting a display bug that is really a UI bug.
///
/// The global overlay comes first and the item's second, which is also the order
/// they stack in when both want the same corner -- and the reason the global one
/// decides that box's style.
pub async fn overlay_payload(state: &AppState, item: Option<&ItemOverlay>) -> serde_json::Value {
    // One acquisition for all three: the cast switch decides whether a cast QR is
    // drawn at all, and taking the lock more than once in a single build invites a
    // reader to wonder whether the parts can disagree.
    let (overlay, cast_enabled, locale) = {
        let settings = state.settings.read().await;
        (
            settings.overlay.clone(),
            settings.cast_enabled,
            settings.locale.clone(),
        )
    };
    let mut layers: Vec<serde_json::Value> = Vec::new();

    if overlay.enabled && !overlay.is_empty() {
        let mut layer = serde_json::to_value(&overlay).unwrap_or_else(|_| json!({}));
        if let Some(object) = layer.as_object_mut() {
            object.insert(
                "image_data".to_string(),
                json!(image_data_uri(state, overlay.image_asset_id).await),
            );
            let qr_target = match overlay.qr_source.as_str() {
                // Resolved here, not stored: see the note on `qr_source`. Nothing
                // is drawn when casting is switched off -- advertising a way to
                // share a screen that refuses every sender is worse than silence.
                "cast" if cast_enabled => crate::cast::sender_url(state),
                "cast" => String::new(),
                _ => overlay.qr_text.clone(),
            };
            object.insert("qr_modules".to_string(), json!(qr_modules(&qr_target)));
        }
        layers.push(layer);
    }

    if let Some(item) = item.filter(|item| item.draws()) {
        // An item that names no corner joins the global overlay's box, which is
        // the arrangement that needs no thought from whoever fills in the card.
        let position = if item.position.is_empty() {
            overlay.position.clone()
        } else {
            item.position.clone()
        };
        layers.push(json!({
            "position": position,
            "text": item.text,
            "image_data": image_data_uri(state, item.image_asset_id).await,
            "qr_modules": qr_modules(&item.qr_text),
            "qr_label": item.qr_label,
            // Style follows the global overlay, so a shared box is uniform and a
            // separate one still looks like it belongs to the same display.
            "size": overlay.size,
            "margin": overlay.margin,
            "max_width": overlay.max_width,
            "qr_size": overlay.qr_size,
            "background_color": overlay.background_color,
            "background_alpha": overlay.background_alpha,
            "plain": overlay.plain,
            "background_css": overlay.background_css,
            "color": overlay.color,
            "color_alpha": overlay.color_alpha,
        }));
    }

    json!({
        // Nothing in here is a URL, and that is the point: the overlay's DOM lives
        // in the displayed page's document, where Chromium's Local Network Access
        // refuses any request to 127.0.0.1 -- both `fetch` and `<img>` -- without a
        // permission click that a kiosk has nobody to make.
        // Empty is meaningful: `Intl` then falls back to the display browser's own
        // locale, which follows from --browser-language.
        "locale": locale,
        "layers": layers,
    })
}

/// Is this asset usable as an overlay image?
///
/// A dangling id would render as a broken image on a screen nobody is standing in
/// front of, and an image too large to inline would silently not draw -- both are
/// worth saying at the moment somebody picks it.
pub async fn check_overlay_image(state: &AppState, asset_id: i64) -> Result<(), String> {
    let row = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT local_path, mimetype FROM assets WHERE id = ?",
    )
    .bind(asset_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("Failed to check overlay asset {}: {}", asset_id, e);
        "Das Bild konnte nicht geprüft werden.".to_string()
    })?
    .ok_or_else(|| "Das gewählte Bild gibt es nicht.".to_string())?;

    let (local_path, mimetype) = row;
    if !mimetype.unwrap_or_default().starts_with("image/") {
        return Err("Das gewählte Asset ist kein Bild.".to_string());
    }

    let path = state.args.assets_dir.join(&local_path);
    let size = tokio::fs::metadata(&path)
        .await
        .map(|meta| meta.len() as usize)
        .map_err(|_| "Die Bilddatei fehlt auf der Platte.".to_string())?;
    if size > OVERLAY_IMAGE_MAX_BYTES {
        return Err(format!(
            "Das Bild ist zu groß für ein Overlay ({} KB, erlaubt sind {} KB).",
            size / 1024,
            OVERLAY_IMAGE_MAX_BYTES / 1024
        ));
    }
    Ok(())
}

fn qr_modules(text: &str) -> Option<Vec<String>> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    crate::cast::qr_matrix(text)
}

/// The image inlined as a `data:` URI.
///
/// An `<img>` is the one thing the overlay cannot draw itself, so this is the
/// only way to get a logo onto a foreign page without a request. A strict
/// `img-src` CSP still refuses it -- best effort, like the injection itself.
async fn image_data_uri(state: &AppState, asset_id: Option<i64>) -> Option<String> {
    let id = asset_id?;
    let row = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT local_path, mimetype FROM assets WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .unwrap_or_else(|e| {
        tracing::error!("Failed to resolve overlay asset {}: {}", id, e);
        None
    })?;

    let (local_path, mimetype) = row;
    let path = state.args.assets_dir.join(&local_path);
    let bytes = tokio::fs::read(&path).await.ok().or_else(|| {
        tracing::error!("Overlay image {} is missing on disk", path.display());
        None
    })?;
    if bytes.len() > OVERLAY_IMAGE_MAX_BYTES {
        // Every apply carries this string over CDP, and the display re-applies on
        // every item. Refusing here matches the `413` the API answers when the
        // image is picked, so the two cannot disagree.
        tracing::error!(
            "Overlay image {} is {} bytes, over the {} limit -- not drawing it",
            path.display(),
            bytes.len(),
            OVERLAY_IMAGE_MAX_BYTES
        );
        return None;
    }

    let mime = mimetype.unwrap_or_else(|| "application/octet-stream".to_string());
    Some(format!(
        "data:{};base64,{}",
        mime,
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    ))
}

#[derive(Serialize)]
struct SettingsResponse {
    cast_enabled: bool,
    cast_auth: &'static str,
    cast_code: String,
    /// Whether the operator UI currently demands credentials.
    auth_enabled: bool,
    auth_user: Option<String>,
    overlay: Overlay,
    locale: String,
    /// What the surrounding system reports, so the admin page can offer it.
    system_locale: Option<String>,
    locks: Locks,
}

pub async fn read_settings(State(state): State<AppState>) -> impl IntoResponse {
    let settings = state.settings.read().await;
    // The password, hashed or not, is never sent back.
    Json(SettingsResponse {
        cast_enabled: settings.cast_enabled,
        cast_auth: cast_auth_key(settings.cast_auth),
        cast_code: settings.cast_code.clone(),
        auth_enabled: settings.auth_user.is_some(),
        auth_user: settings.auth_user.clone(),
        overlay: settings.overlay.clone(),
        locale: settings.locale.clone(),
        system_locale: system_locale(),
        locks: state.locks,
    })
}

#[derive(Deserialize)]
pub struct UpdateRequest {
    cast_enabled: Option<bool>,
    cast_auth: Option<String>,
    cast_code: Option<String>,
    auth_enabled: Option<bool>,
    auth_user: Option<String>,
    auth_password: Option<String>,
    overlay: Option<Overlay>,
    locale: Option<String>,
}

pub async fn update_settings(
    State(state): State<AppState>,
    Json(payload): Json<UpdateRequest>,
) -> Response {
    let bad = |message: String| {
        (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
    };
    let locked = |flag: &str| {
        (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!("Per Kommandozeile festgelegt ({}) und hier nicht änderbar.", flag)
            })),
        )
            .into_response()
    };

    let mut next = state.settings.read().await.clone();
    let mut credentials_changed = false;

    if let Some(enabled) = payload.cast_enabled {
        if enabled != next.cast_enabled {
            if state.locks.cast_enabled {
                return locked("--disable-cast");
            }
            next.cast_enabled = enabled;
        }
    }

    if let Some(auth) = payload.cast_auth {
        match parse_cast_auth(&auth) {
            Some(parsed) => {
                if parsed != next.cast_auth {
                    if state.locks.cast_auth {
                        return locked("--cast-auth");
                    }
                    next.cast_auth = parsed;
                }
            }
            None => return bad("Unbekannter Auth-Modus.".to_string()),
        }
    }

    if let Some(code) = payload.cast_code {
        // Laxer than the generated-code alphabet on purpose: a pairing code is
        // read off a screen where I/O/0/1 are easy to confuse, but an operator
        // types their own from a label and would be annoyed to be told "1234"
        // is invalid. The sender page filters input to the same set.
        let code = code.trim().to_uppercase();
        if !code.is_empty()
            && (code.chars().count() != 4
                || !code.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()))
        {
            return bad("Der Code muss aus genau 4 Buchstaben oder Ziffern bestehen.".to_string());
        }
        if code != next.cast_code {
            if state.locks.cast_code {
                return locked("--cast-code");
            }
            next.cast_code = code;
        }
    }

    if let Some(locale) = payload.locale {
        let locale = locale.trim().to_string();
        if !locale.is_empty() && !is_locale_tag(&locale) {
            return bad("Ungültiges Sprachkennzeichen, erwartet wird z. B. de-DE.".to_string());
        }
        if locale != next.locale {
            if state.locks.locale {
                return locked("--locale");
            }
            next.locale = locale;
        }
    }

    if next.cast_auth == crate::models::CastAuth::Code && next.cast_code.is_empty() {
        return bad("Für den Code-Modus muss ein Code gesetzt sein.".to_string());
    }

    // --- operator credentials ---

    if payload.auth_enabled.is_some() || payload.auth_user.is_some() || payload.auth_password.is_some() {
        if state.locks.basic_auth {
            return locked("--basic-auth-user/--basic-auth-password");
        }

        let wants_auth = payload.auth_enabled.unwrap_or(next.auth_user.is_some());
        if !wants_auth {
            next.auth_user = None;
            next.auth_secret = None;
            credentials_changed = true;
        } else {
            let user = payload
                .auth_user
                .clone()
                .or_else(|| next.auth_user.clone())
                .unwrap_or_default();
            let user = user.trim().to_string();
            if user.is_empty() {
                return bad("Benutzername darf nicht leer sein.".to_string());
            }

            match payload.auth_password.as_deref() {
                Some(password) if !password.is_empty() => {
                    if password.chars().count() < 8 {
                        return bad("Das Passwort muss mindestens 8 Zeichen haben.".to_string());
                    }
                    next.auth_secret = Some(Secret::Hash(hash_password(password)));
                    credentials_changed = true;
                }
                // Keeping the existing password is fine; turning auth *on*
                // without ever setting one is not, or the UI would lock behind
                // credentials nobody knows.
                _ if next.auth_secret.is_none() => {
                    return bad("Für die Anmeldung muss ein Passwort gesetzt werden.".to_string());
                }
                _ => {}
            }

            if next.auth_user.as_deref() != Some(user.as_str()) {
                credentials_changed = true;
            }
            next.auth_user = Some(user);
        }
    }

    let mut overlay_changed = false;
    if let Some(overlay) = payload.overlay {
        let overlay = overlay.sanitized();
        if overlay.enabled && overlay.is_empty() {
            return bad("Das Overlay ist eingeschaltet, zeigt aber nichts an.".to_string());
        }
        if let Some(id) = overlay.image_asset_id {
            if let Err(message) = check_overlay_image(&state, id).await {
                return bad(message);
            }
        }
        overlay_changed = serde_json::to_string(&overlay).ok()
            != serde_json::to_string(&next.overlay).ok();
        next.overlay = overlay;
    }

    let cast_turned_off = {
        let mut settings = state.settings.write().await;
        let was_enabled = settings.cast_enabled;
        *settings = next.clone();
        was_enabled && !next.cast_enabled
    };

    if credentials_changed {
        // Otherwise the cache keeps accepting the previous password.
        *state.auth_cache.lock().await = None;
    }

    persist(&state.pool, &next).await;

    // notify_one(), never notify_waiters(): the loop is only parked on this for
    // part of its cycle, and a dropped notification here means an overlay edit
    // that silently never reaches the screen.
    if overlay_changed {
        state.overlay_signal.notify_one();
    }

    // Turning casting off has to interrupt whatever is running, or the switch
    // would only apply to the next person.
    if cast_turned_off {
        tracing::info!("Cast: disabled by the operator, ending any running session");
        crate::cast::end_session(&state).await;
    }

    read_settings(State(state)).await.into_response()
}
