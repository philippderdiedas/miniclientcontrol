//! Signing in and out, the current account, and managing accounts.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::middleware::{session_token, ViaTls, ViaToken, SESSION_COOKIE};
use super::{AccountError, Identity, Role};
use crate::models::AppState;

/// Failures an address may make within the window before it is told to wait.
const LOGIN_BOUND: u32 = 10;
const LOGIN_WINDOW: Duration = Duration::from_secs(5 * 60);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/me/password", put(own_password))
        .route("/api/me/tokens", get(list_tokens).post(create_token))
        .route("/api/me/tokens/{id}", put(update_own_token).delete(revoke_own_token))
        // Every account's tokens, for an admin (`roles.rs`).
        .route("/api/tokens", get(list_all_tokens))
        .route("/api/tokens/{id}", put(update_any_token).delete(revoke_any_token))
        .route("/api/users", get(list_users).post(create))
        .route("/api/users/{id}", put(update).delete(remove))
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn account_error(e: AccountError) -> Response {
    match e {
        AccountError::Unknown => error(StatusCode::NOT_FOUND, e.message()),
        AccountError::Db(ref inner) => {
            tracing::error!("Account write failed: {}", inner);
            error(StatusCode::INTERNAL_SERVER_ERROR, e.message())
        }
        _ => error(StatusCode::BAD_REQUEST, e.message()),
    }
}

#[derive(Deserialize)]
struct Credentials {
    name: String,
    password: String,
}

async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    tls: Option<Extension<ViaTls>>,
    Json(body): Json<Credentials>,
) -> Response {
    // Checked and counted under one acquisition: a check separated from its
    // increment lets N concurrent guesses all pass the bound -- measured on the
    // cast code lockout before it was fixed.
    {
        let mut attempts = state.login_attempts.lock().await;
        let entry = attempts.entry(peer.ip()).or_insert((0, Instant::now()));
        if entry.1.elapsed() > LOGIN_WINDOW {
            *entry = (0, Instant::now());
        }
        if entry.0 >= LOGIN_BOUND {
            return error(StatusCode::TOO_MANY_REQUESTS, "Zu viele Fehlversuche. Bitte später erneut versuchen.");
        }
        entry.0 += 1;
    }
    // Single sign-on only: no local account signs in here. (The command-line
    // credential never did -- it has no account row -- and works over Basic.)
    if !state.oidc.read().await.local_passwords {
        return error(StatusCode::FORBIDDEN, "Lokale Anmeldung ist abgeschaltet – bitte per SSO anmelden.");
    }
    let Some(who) = super::verify_password(&state.pool, &body.name, &body.password).await else {
        return error(StatusCode::UNAUTHORIZED, "Name oder Passwort stimmt nicht.");
    };
    state.login_attempts.lock().await.remove(&peer.ip());
    let Some(user_id) = who.user_id else {
        return error(StatusCode::UNAUTHORIZED, "Name oder Passwort stimmt nicht.");
    };
    let token = super::create_session(&state.pool, user_id).await;
    let secure = if tls.is_some() { "; Secure" } else { "" };
    let cookie = format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/{secure}");
    (
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "name": who.name, "role": who.role })),
    )
        .into_response()
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = session_token(&headers) {
        super::end_session(&state.pool, &token).await;
    }
    let cookie = format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    ([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response()
}

async fn me(Extension(who): Extension<Identity>) -> Response {
    Json(json!({ "name": who.name, "role": who.role, "open": who.open, "account": who.user_id.is_some() }))
        .into_response()
}

#[derive(Deserialize)]
struct PasswordChange {
    current: String,
    new: String,
}

async fn own_password(
    State(state): State<AppState>,
    Extension(who): Extension<Identity>,
    token: Option<Extension<ViaToken>>,
    Json(body): Json<PasswordChange>,
) -> Response {
    if token.is_some() {
        return error(StatusCode::FORBIDDEN, TOKEN_REFUSED);
    }
    let Some(id) = who.user_id else {
        return error(StatusCode::BAD_REQUEST, "Dieser Zugang hat kein eigenes Passwort.");
    };
    if super::verify_password(&state.pool, &who.name, &body.current).await.is_none() {
        return error(StatusCode::BAD_REQUEST, "Das bisherige Passwort stimmt nicht.");
    }
    match super::update_user(&state.pool, id, None, Some(&body.new), None).await {
        Ok(()) => {
            state.basic_cache.lock().await.clear();
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => account_error(e),
    }
}

/// A token manages nothing about its own account's credentials -- neither the
/// password nor other tokens. An LLM holding one could otherwise mint itself a
/// credential its owner never sees, or lock that owner out.
const TOKEN_REFUSED: &str = "Mit einem API-Token lassen sich die Zugangsdaten des Kontos nicht ändern.";

use super::tokens::{Scope, TokenError};

/// Refused for a request made with a token: managing tokens takes a real
/// sign-in, for one's own as for everybody's.
fn not_by_token(token: &Option<Extension<ViaToken>>) -> Result<(), Response> {
    match token {
        Some(_) => Err(error(StatusCode::FORBIDDEN, TOKEN_REFUSED)),
        None => Ok(()),
    }
}

/// The account whose tokens these are: only a real one has any.
fn own_account(who: &Identity, token: &Option<Extension<ViaToken>>) -> Result<i64, Response> {
    not_by_token(token)?;
    who.user_id.ok_or_else(|| {
        error(
            StatusCode::BAD_REQUEST,
            "API-Tokens gehören zu einem Konto – dieser Zugang hat keins.",
        )
    })
}

fn own_scope(who: &Identity, token: &Option<Extension<ViaToken>>) -> Result<Scope, Response> {
    own_account(who, token).map(Scope::Own)
}

fn token_error(e: TokenError) -> Response {
    match e {
        TokenError::Db(ref inner) => {
            tracing::error!("API token write failed: {}", inner);
            error(StatusCode::INTERNAL_SERVER_ERROR, e.message())
        }
        _ => error(StatusCode::BAD_REQUEST, e.message()),
    }
}

async fn list_tokens_in(state: &AppState, scope: Scope) -> Response {
    match super::tokens::list(&state.pool, scope).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => {
            tracing::error!("Failed to list API tokens: {}", e);
            error(StatusCode::INTERNAL_SERVER_ERROR, "Tokens konnten nicht gelesen werden.")
        }
    }
}

async fn list_tokens(
    State(state): State<AppState>,
    Extension(who): Extension<Identity>,
    token: Option<Extension<ViaToken>>,
) -> Response {
    match own_scope(&who, &token) {
        Ok(scope) => list_tokens_in(&state, scope).await,
        Err(refused) => refused,
    }
}

async fn list_all_tokens(State(state): State<AppState>, token: Option<Extension<ViaToken>>) -> Response {
    match not_by_token(&token) {
        Ok(()) => list_tokens_in(&state, Scope::All).await,
        Err(refused) => refused,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewToken {
    name: String,
    /// Days until it expires; absent or `null` for never.
    #[serde(default)]
    days: Option<u32>,
}

async fn create_token(
    State(state): State<AppState>,
    Extension(who): Extension<Identity>,
    token: Option<Extension<ViaToken>>,
    Json(body): Json<NewToken>,
) -> Response {
    let user_id = match own_account(&who, &token) {
        Ok(id) => id,
        Err(refused) => return refused,
    };
    match super::tokens::create(&state.pool, user_id, &body.name, body.days).await {
        // The secret is in this answer and nowhere else, ever.
        Ok((id, secret)) => (StatusCode::CREATED, Json(json!({ "id": id, "token": secret }))).into_response(),
        Err(e) => token_error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenChange {
    name: Option<String>,
    /// A new lifetime from now; `null` for never, absent to keep it.
    #[serde(default, deserialize_with = "crate::handlers::double_option")]
    days: Option<Option<u32>>,
}

async fn update_token_in(state: &AppState, scope: Scope, id: i64, body: TokenChange) -> Response {
    match super::tokens::update(&state.pool, scope, id, body.name.as_deref(), body.days).await {
        Ok(true) => Json(json!({ "ok": true })).into_response(),
        Ok(false) => error(StatusCode::NOT_FOUND, "Diesen Token gibt es nicht."),
        Err(e) => token_error(e),
    }
}

async fn update_own_token(
    State(state): State<AppState>,
    Extension(who): Extension<Identity>,
    token: Option<Extension<ViaToken>>,
    Path(id): Path<i64>,
    Json(body): Json<TokenChange>,
) -> Response {
    match own_scope(&who, &token) {
        Ok(scope) => update_token_in(&state, scope, id, body).await,
        Err(refused) => refused,
    }
}

async fn update_any_token(
    State(state): State<AppState>,
    token: Option<Extension<ViaToken>>,
    Path(id): Path<i64>,
    Json(body): Json<TokenChange>,
) -> Response {
    match not_by_token(&token) {
        Ok(()) => update_token_in(&state, Scope::All, id, body).await,
        Err(refused) => refused,
    }
}

async fn revoke_token_in(state: &AppState, scope: Scope, id: i64) -> Response {
    match super::tokens::revoke(&state.pool, scope, id).await {
        Ok(true) => Json(json!({ "ok": true })).into_response(),
        Ok(false) => error(StatusCode::NOT_FOUND, "Diesen Token gibt es nicht."),
        Err(e) => {
            tracing::error!("Failed to revoke an API token: {}", e);
            error(StatusCode::INTERNAL_SERVER_ERROR, "Token konnte nicht widerrufen werden.")
        }
    }
}

async fn revoke_own_token(
    State(state): State<AppState>,
    Extension(who): Extension<Identity>,
    token: Option<Extension<ViaToken>>,
    Path(id): Path<i64>,
) -> Response {
    match own_scope(&who, &token) {
        Ok(scope) => revoke_token_in(&state, scope, id).await,
        Err(refused) => refused,
    }
}

async fn revoke_any_token(
    State(state): State<AppState>,
    token: Option<Extension<ViaToken>>,
    Path(id): Path<i64>,
) -> Response {
    match not_by_token(&token) {
        Ok(()) => revoke_token_in(&state, Scope::All, id).await,
        Err(refused) => refused,
    }
}

async fn list_users(State(state): State<AppState>) -> Response {
    let rows: Result<Vec<(i64, String, String, bool)>, _> =
        sqlx::query_as("SELECT id, name, role, disabled FROM users ORDER BY name ASC")
            .fetch_all(&state.pool)
            .await;
    match rows {
        Ok(rows) => Json(
            rows.into_iter()
                .map(|(id, name, role, disabled)| json!({ "id": id, "name": name, "role": role, "disabled": disabled }))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => {
            tracing::error!("Failed to list accounts: {}", e);
            error(StatusCode::INTERNAL_SERVER_ERROR, "Konten konnten nicht gelesen werden.")
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewUser {
    name: String,
    password: String,
    role: String,
}

async fn create(State(state): State<AppState>, Json(body): Json<NewUser>) -> Response {
    let Some(role) = Role::parse(&body.role) else {
        return error(StatusCode::BAD_REQUEST, "Unbekannte Rolle.");
    };
    if body.name.trim().is_empty() {
        return error(StatusCode::BAD_REQUEST, "Der Name darf nicht leer sein.");
    }
    match super::create_user(&state.pool, &body.name, &body.password, role).await {
        Ok(id) => (StatusCode::CREATED, Json(json!({ "id": id }))).into_response(),
        Err(e) => account_error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserChange {
    role: Option<String>,
    password: Option<String>,
    disabled: Option<bool>,
}

async fn update(State(state): State<AppState>, Path(id): Path<i64>, Json(body): Json<UserChange>) -> Response {
    let role = match body.role.as_deref().map(Role::parse) {
        Some(None) => return error(StatusCode::BAD_REQUEST, "Unbekannte Rolle."),
        Some(Some(role)) => Some(role),
        None => None,
    };
    match super::update_user(&state.pool, id, role, body.password.as_deref(), body.disabled).await {
        Ok(()) => {
            state.basic_cache.lock().await.clear();
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => account_error(e),
    }
}

async fn remove(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match super::delete_user(&state.pool, id).await {
        Ok(()) => {
            state.basic_cache.lock().await.clear();
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => account_error(e),
    }
}
