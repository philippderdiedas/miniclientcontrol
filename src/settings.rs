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
use serde::Serialize;

use crate::models::{Args, CastAuth};

static PBKDF2_ALG: pbkdf2::Algorithm = pbkdf2::PBKDF2_HMAC_SHA256;
const PBKDF2_ITERATIONS: u32 = 120_000;

const KEY_CAST_ENABLED: &str = "cast_enabled";
const KEY_CAST_AUTH: &str = "cast_auth";
const KEY_CAST_CODE: &str = "cast_code";
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
    }
}

/// Write the editable settings back. Values pinned by the command line are
/// written too, so removing the flag later leaves the operator's intent intact.
pub async fn persist(pool: &sqlx::SqlitePool, settings: &AppSettings) {
    let mut rows = vec![
        (KEY_CAST_ENABLED, settings.cast_enabled.to_string()),
        (KEY_CAST_AUTH, cast_auth_key(settings.cast_auth).to_string()),
        (KEY_CAST_CODE, settings.cast_code.clone()),
        (
            KEY_AUTH_USER,
            settings.auth_user.clone().unwrap_or_default(),
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
use serde::Deserialize;
use serde_json::json;

use crate::models::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/settings", get(read_settings).put(update_settings))
}

#[derive(Serialize)]
struct SettingsResponse {
    cast_enabled: bool,
    cast_auth: &'static str,
    cast_code: String,
    /// Whether the operator UI currently demands credentials.
    auth_enabled: bool,
    auth_user: Option<String>,
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

    let cast_turned_off = {
        let mut settings = state.settings.write().await;
        let was_enabled = settings.cast_enabled;
        *settings = next.clone();
        was_enabled && !next.cast_enabled
    };

    if credentials_changed {
        // The cache holds a header that was verified against the *old* password.
        *state.auth_cache.lock().await = None;
    }

    persist(&state.pool, &next).await;

    // Turning casting off has to interrupt whatever is running, or the switch
    // would only apply to the next person.
    if cast_turned_off {
        tracing::info!("Cast: disabled by the operator, ending any running session");
        crate::cast::end_session(&state).await;
    }

    read_settings(State(state)).await.into_response()
}
