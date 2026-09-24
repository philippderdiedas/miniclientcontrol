//! The provider's configuration (admin), what the login page may know (open),
//! and the flow's two endpoints (open).

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Extension;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::config::GroupMapping;
use crate::models::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/oidc/info", get(info))
        .route("/api/oidc/config", get(get_config).put(put_config))
        .route("/api/oidc/start", get(start))
        .route("/api/oidc/callback", get(callback))
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigUpdate {
    issuer: String,
    client_id: String,
    /// Absent or `null`: keep the stored secret. Write-only.
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    groups_claim: Option<String>,
    #[serde(default)]
    label: String,
    #[serde(default)]
    mapping: Vec<GroupMapping>,
    #[serde(default)]
    everyone_casts: bool,
    #[serde(default = "yes")]
    local_passwords: bool,
}
fn yes() -> bool { true }

async fn info(State(state): State<AppState>) -> Response {
    let config = state.oidc.read().await;
    Json(json!({
        "configured": config.configured(),
        "label": config.button_label(),
        "local_passwords": config.local_passwords,
    }))
    .into_response()
}

async fn get_config(State(state): State<AppState>) -> Response {
    let config = state.oidc.read().await.clone();
    Json(json!({
        "issuer": config.issuer,
        "client_id": config.client_id,
        "secret_set": !config.client_secret.is_empty(),
        "groups_claim": config.groups_claim,
        "label": config.label,
        "mapping": config.mapping,
        "everyone_casts": config.everyone_casts,
        "local_passwords": config.local_passwords,
        "redirect_uri": super::config::redirect_uri(&state),
        "redirect_stable": super::config::redirect_stable(&state),
    }))
    .into_response()
}

async fn put_config(State(state): State<AppState>, Json(body): Json<ConfigUpdate>) -> Response {
    let issuer = body.issuer.trim().trim_end_matches('/').to_string();
    if !issuer.is_empty() && !super::http::allowed_url(&issuer) {
        return error(StatusCode::BAD_REQUEST, "Der Anbieter muss über https erreichbar sein.");
    }
    let mut next = state.oidc.read().await.clone();
    next.issuer = issuer;
    next.client_id = body.client_id.trim().to_string();
    if let Some(secret) = body.client_secret {
        next.client_secret = secret;
    }
    next.groups_claim = body.groups_claim.filter(|c| !c.trim().is_empty()).unwrap_or_else(|| "groups".into());
    next.label = body.label.trim().to_string();
    next.mapping = body.mapping.into_iter().filter(|m| !m.group.trim().is_empty()).collect();
    next.everyone_casts = body.everyone_casts;
    if !body.local_passwords && !super::flow::sso_admin_exists(&state.pool).await {
        return error(StatusCode::BAD_REQUEST,
            "Lokale Passwörter lassen sich erst abschalten, wenn ein Admin per SSO angemeldet war.");
    }
    if !body.local_passwords && !next.configured() {
        return error(StatusCode::BAD_REQUEST, "Ohne eingerichteten Anbieter bleiben lokale Passwörter an.");
    }
    next.local_passwords = body.local_passwords;
    if let Err(e) = super::config::save(&state.pool, &next).await {
        tracing::error!("Failed to store the OpenID Connect configuration: {}", e);
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Speichern fehlgeschlagen.");
    }
    *state.oidc.write().await = next;
    // A Basic credential that verified under the old rule must not outlive it.
    state.basic_cache.lock().await.clear();
    super::flow::forget_discovery().await;
    Json(json!({ "ok": true })).into_response()
}

#[derive(Deserialize)]
struct StartQuery {
    #[serde(default)]
    next: Option<String>,
}

fn secure(tls: &Option<Extension<crate::accounts::middleware::ViaTls>>) -> &'static str {
    if tls.is_some() { "; Secure" } else { "" }
}

/// Send the browser to the provider, remembering what it must bring back.
async fn start(
    State(state): State<AppState>,
    tls: Option<Extension<crate::accounts::middleware::ViaTls>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    Query(query): Query<StartQuery>,
) -> Response {
    use super::flow::{challenge, random_token, safe_next, Pending, BINDING_COOKIE, PENDING_TTL};
    let config = state.oidc.read().await.clone();
    if !config.configured() {
        return Redirect::to("/login.html?sso=off").into_response();
    }
    // The provider calls back on the one address registered with it. Begin
    // there too, or the binding cookie set here and the session cookie set on
    // the callback land on two different hosts and the sign-in silently fails
    // for anyone who reached this page under another name.
    let redirect_uri = super::config::redirect_uri(&state);
    let canonical = url::Url::parse(&redirect_uri).ok();
    let here = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| uri.authority().map(|a| a.to_string()));
    if let Some(canonical) = &canonical {
        let wanted = match canonical.port() {
            Some(port) => format!("{}:{port}", canonical.host_str().unwrap_or_default()),
            None => canonical.host_str().unwrap_or_default().to_string(),
        };
        if tls.is_none() || here.as_deref() != Some(wanted.as_str()) {
            let next = safe_next(query.next);
            return Redirect::to(&format!(
                "{}/api/oidc/start?next={}",
                canonical.origin().ascii_serialization(),
                urlencoding::encode(&next)
            ))
            .into_response();
        }
    }
    let discovery = match super::flow::discover(&config).await {
        Ok((discovery, _)) => discovery,
        Err(e) => {
            tracing::warn!("OpenID Connect discovery failed: {:#}", e);
            return Redirect::to("/login.html?sso=unreachable").into_response();
        }
    };
    let (state_token, nonce, verifier, binding) = (random_token(), random_token(), random_token(), random_token());
    {
        let mut pending = state.oidc_pending.lock().await;
        pending.retain(|_, p| p.created.elapsed() < PENDING_TTL);
        pending.insert(state_token.clone(), Pending {
            nonce: nonce.clone(),
            verifier: verifier.clone(),
            next: safe_next(query.next),
            binding: binding.clone(),
            created: std::time::Instant::now(),
        });
    }
    let separator = if discovery.authorization_endpoint.contains('?') { '&' } else { '?' };
    // `groups` because several providers send the claim only when asked;
    // those that do not know the scope ignore it.
    let target = format!(
        "{}{}response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}&code_challenge={}&code_challenge_method=S256",
        discovery.authorization_endpoint,
        separator,
        urlencoding::encode(&config.client_id),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode("openid profile groups"),
        urlencoding::encode(&state_token),
        urlencoding::encode(&nonce),
        challenge(&verifier),
    );
    // Lax, not Strict: the callback is a top-level navigation from the
    // provider's site, and a Strict cookie would not come back with it.
    let cookie = format!(
        "{BINDING_COOKIE}={binding}; HttpOnly; SameSite=Lax; Path=/api/oidc; Max-Age={}{}",
        PENDING_TTL.as_secs(),
        secure(&tls)
    );
    ([(header::SET_COOKIE, cookie)], Redirect::to(&target)).into_response()
}

#[derive(Deserialize)]
struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// The page the callback answers with. A client-side navigation, not a 302:
/// the session cookie is SameSite=Strict, and a redirect chain that started on
/// the provider's site would not send it on the next request. The message is
/// one of a fixed set and `next` has passed `safe_next`; both are escaped anyway.
fn page(message: Option<&str>, next: &str, cookies: Vec<String>) -> Response {
    let escape = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('"', "&quot;");
    let body = match message {
        None => format!(
            "<!doctype html><meta charset=utf-8><meta http-equiv=\"refresh\" content=\"0;url={0}\">\
             <p>Angemeldet. <a href=\"{0}\">Weiter</a></p>",
            escape(next)
        ),
        Some(m) => format!(
            "<!doctype html><meta charset=utf-8><p>{}</p><p><a href=\"/login.html\">Zurück zur Anmeldung</a></p>",
            escape(m)
        ),
    };
    let mut response = ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response();
    for cookie in cookies {
        if let Ok(value) = cookie.parse() {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

async fn callback(
    State(state): State<AppState>,
    tls: Option<Extension<crate::accounts::middleware::ViaTls>>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    use super::flow::{BINDING_COOKIE, CAST_COOKIE, PENDING_TTL};
    let clear = format!("{BINDING_COOKIE}=; HttpOnly; SameSite=Lax; Path=/api/oidc; Max-Age=0{}", secure(&tls));
    let refuse = |message: &str| page(Some(message), "/login.html", vec![clear.clone()]);

    if let Some(error) = query.error.as_deref() {
        tracing::info!("OpenID Connect: the provider answered {}", error);
        return refuse("Die Anmeldung beim Anbieter wurde abgebrochen.");
    }
    let (Some(code), Some(state_token)) = (query.code, query.state) else {
        return refuse("Unvollständige Antwort des Anbieters.");
    };
    // Single use: taken out whether or not the rest succeeds.
    let pending = state.oidc_pending.lock().await.remove(&state_token);
    let binding = crate::accounts::middleware::cookie(&headers, BINDING_COOKIE);
    let Some(pending) = pending.filter(|p| {
        p.created.elapsed() < PENDING_TTL
            && binding.as_deref().is_some_and(|b| crate::settings::constant_time_eq(b.as_bytes(), p.binding.as_bytes()))
    }) else {
        return refuse("Die Anmeldung ist abgelaufen oder gehört zu einem anderen Browser. Bitte neu beginnen.");
    };

    let config = state.oidc.read().await.clone();
    let (discovery, mut keys) = match super::flow::discover(&config).await {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!("OpenID Connect discovery failed: {:#}", e);
            return refuse("Der Anbieter ist nicht erreichbar.");
        }
    };
    let redirect_uri = super::config::redirect_uri(&state);
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("code_verifier", pending.verifier.as_str()),
    ];
    if !discovery.basic_auth {
        form.push(("client_id", config.client_id.as_str()));
        form.push(("client_secret", config.client_secret.as_str()));
    }
    let basic = discovery.basic_auth.then_some((config.client_id.as_str(), config.client_secret.as_str()));
    let tokens = match super::http::post_form(&discovery.token_endpoint, &form, basic).await {
        Ok(tokens) => tokens,
        Err(e) => {
            tracing::warn!("OpenID Connect token exchange failed: {:#}", e);
            return refuse("Der Anbieter hat die Anmeldung nicht bestätigt.");
        }
    };
    let Some(id_token) = tokens.get("id_token").and_then(serde_json::Value::as_str) else {
        return refuse("Der Anbieter hat kein ID-Token geschickt.");
    };

    let expect = super::jwt::Expect {
        issuer: &config.issuer,
        client_id: &config.client_id,
        nonce: &pending.nonce,
        secret: &config.client_secret,
        now: chrono::Utc::now().timestamp(),
    };
    let mut verified = super::jwt::verify(id_token, &keys, &expect);
    if verified == Err(super::jwt::JwtError::UnknownKey) {
        if let Ok(fresh) = super::flow::refresh_keys(&config, &discovery).await {
            keys = fresh;
            verified = super::jwt::verify(id_token, &keys, &expect);
        }
    }
    let claims = match verified {
        Ok(claims) => claims,
        Err(e) => {
            tracing::warn!("OpenID Connect: refused an ID token: {}", e);
            return refuse("Das Anmelde-Token des Anbieters ist ungültig.");
        }
    };
    let subject = claims.get("sub").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
    let name = super::flow::display_name(&claims);
    let groups = super::flow::groups_of(&claims, &config.groups_claim);

    use super::config::Target;
    use crate::accounts::Role;
    let role = match super::flow::decide(&groups, &config) {
        Some(Target::Admin) => Some(Role::Admin),
        Some(Target::Manager) => Some(Role::Manager),
        Some(Target::Editor) => Some(Role::Editor),
        Some(Target::Cast) => None,
        None => {
            super::flow::disable_linked(&state.pool, &config.issuer, &subject).await;
            state.basic_cache.lock().await.clear();
            tracing::info!("OpenID Connect: {} has no mapped group, refused", name);
            return refuse("Anmeldung abgelehnt: keine passende Gruppe.");
        }
    };

    let session_cookie = match role {
        Some(role) => {
            let (user_id, account) =
                match super::flow::account_for(&state.pool, &config.issuer, &subject, &name, role).await {
                    Ok(found) => found,
                    Err(e) => {
                        tracing::error!("OpenID Connect: could not store the account: {:#}", e);
                        return refuse("Das Konto konnte nicht angelegt werden.");
                    }
                };
            state.basic_cache.lock().await.clear();
            tracing::info!("OpenID Connect: {} signed in as {} ({})", name, account, role.as_str());
            let token = crate::accounts::create_session(&state.pool, user_id).await;
            format!(
                "{}={token}; HttpOnly; SameSite=Strict; Path=/{}",
                crate::accounts::middleware::SESSION_COOKIE,
                secure(&tls)
            )
        }
        None => {
            tracing::info!("OpenID Connect: {} signed in to cast", name);
            let token = super::flow::create_cast_session(&state.pool, &name, &config.issuer, &subject).await;
            format!("{CAST_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/{}", secure(&tls))
        }
    };
    page(None, &pending.next, vec![clear, session_cookie])
}
