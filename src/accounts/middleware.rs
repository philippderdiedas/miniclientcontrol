//! Who a request is, and whether it may.
//!
//! In order: the display's loopback paths and the cast guest paths stay exempt
//! exactly as before; then a session cookie, HTTP Basic against the accounts,
//! the command-line credential; with no account at all, open mode. What the
//! identity may do is `roles::required`'s answer.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use serde_json::json;

use super::roles::{required, Need};
use super::{Identity, Role};
use crate::models::AppState;

pub const SESSION_COOKIE: &str = "mcc_session";

/// Set on a request replayed from an approved proposal: who approved it. An
/// extension, never a header, so no client can send one.
#[derive(Clone)]
pub struct ReplayIdentity(pub Identity);

/// Set on a request authenticated by an API token -- and on a request the MCP
/// endpoint replays for one. A token must not manage the credentials of its own
/// account: an LLM that could mint tokens or change the password could lock the
/// person out who gave it access.
#[derive(Clone)]
pub struct ViaToken;

/// Set on requests that arrived over the TLS listener, so the session cookie
/// can be marked `Secure` there.
#[derive(Clone)]
pub struct ViaTls;

/// One cookie's value, if the request carries it non-empty.
pub fn cookie(headers: &HeaderMap, wanted: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == wanted)
        .map(|(_, value)| value.to_string())
        .filter(|value| !value.is_empty())
}

pub fn session_token(headers: &HeaderMap) -> Option<String> {
    cookie(headers, SESSION_COOKIE)
}

fn decode_basic(value: &str) -> Option<(String, String)> {
    use base64::Engine;
    let encoded = value.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, password) = text.split_once(':')?;
    Some((user.to_string(), password.to_string()))
}

/// Which credential a request presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Via {
    /// A session cookie -- what the `Origin` check is for.
    Cookie,
    /// An API token (`Authorization: Bearer`).
    Token,
    /// HTTP Basic, the command-line credential, or open mode.
    Other,
}

/// The identity behind a request, and which credential it came from.
async fn resolve(state: &AppState, headers: &HeaderMap) -> Option<(Identity, Via)> {
    if let Some(token) = session_token(headers) {
        if let Some(who) = super::session_identity(&state.pool, &token).await {
            return Some((who, Via::Cookie));
        }
    }
    if let Some(value) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        // A bearer that does not resolve is not retried as anything else: it
        // was meant as a token, and falling through to open mode would answer
        // a revoked token with full access on a device with no accounts left.
        if let Some(secret) = value.strip_prefix("Bearer ") {
            return super::tokens::identity(&state.pool, secret.trim())
                .await
                .map(|who| (who, Via::Token));
        }
        if let Some(who) = state.basic_cache.lock().await.get(value).cloned() {
            return Some((who, Via::Other));
        }
        if let Some((user, password)) = decode_basic(value) {
            if let Some((rescue_user, rescue_password)) = &state.rescue {
                if crate::settings::constant_time_eq(user.as_bytes(), rescue_user.as_bytes())
                    && crate::settings::constant_time_eq(password.as_bytes(), rescue_password.as_bytes())
                {
                    return Some((Identity::rescue(rescue_user), Via::Other));
                }
            }
            // With local passwords off only the command-line credential above
            // still works over Basic -- the way back in when the provider is gone.
            if state.oidc.read().await.local_passwords {
                if let Some(who) = super::verify_password(&state.pool, &user, &password).await {
                    state.basic_cache.lock().await.insert(value.to_string(), who.clone());
                    return Some((who, Via::Other));
                }
            }
        }
    }
    // Open only when nothing at all guards the device: no account *and* no
    // command-line credential. A venue that runs with only the flags today had
    // a protected admin, and an upgrade must not open it to the LAN.
    if state.rescue.is_none() && !super::any_user(&state.pool).await {
        return Some((Identity::open_mode(), Via::Other));
    }
    None
}

/// A cookie-authenticated write must come from a page of this host.
/// `SameSite=Strict` already stops most cross-site requests; this covers the
/// rest, and a missing `Origin` on a write is refused rather than trusted.
///
/// The host comes from the `Host` header or, over HTTP/2 -- which has none --
/// from the URI's authority, where hyper puts `:authority`. Reading only `Host`
/// refused every cookie write a browser made over HTTPS.
pub(crate) fn same_origin(headers: &HeaderMap, uri: &axum::http::Uri) -> bool {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| uri.authority().map(|a| a.as_str()));
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    match (host, origin) {
        (Some(host), Some(origin)) => origin
            .split_once("://")
            .map(|(_, rest)| rest.trim_end_matches('/') == host)
            .unwrap_or(false),
        _ => false,
    }
}

fn wants_html(method: &Method, path: &str, headers: &HeaderMap) -> bool {
    method == Method::GET
        && (path == "/" || path.ends_with(".html")
            || headers
                .get(header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|a| a.contains("text/html")))
}

fn refuse(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

pub async fn auth_middleware(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: axum::extract::Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    let method = request.method().clone();

    // The display browser is driven over CDP and cannot present credentials;
    // loopback-scoped, exactly as before.
    if peer.ip().is_loopback() && crate::is_display_path(&path) {
        return next.run(request).await;
    }
    // The guest is by definition not loopback; exempt regardless of address.
    // Exempt, but not blind: a signed-in person is recognised so a screen can
    // be kept for them (`cast::access`) -- an account's session, or a cast
    // session from single sign-on, which exists nowhere else. Attached as a
    // `Caster`, which carries no role. Never required -- a request without one
    // proceeds exactly as a guest's -- and only from a cookie: a cached Basic
    // credential must not turn a guest into an account. A write needs this
    // host's `Origin`, or another site could claim a screen with a member's
    // cookie; a read only tells the page who is signed in.
    if !state.args.disable_cast && crate::cast::is_cast_public_path(&path) {
        let writing = method != Method::GET && method != Method::HEAD;
        if !writing || same_origin(request.headers(), request.uri()) {
            let mut name = match session_token(request.headers()) {
                Some(token) => super::session_identity(&state.pool, &token).await.map(|who| who.name),
                None => None,
            };
            if name.is_none() {
                if let Some(token) = cookie(request.headers(), crate::oidc::flow::CAST_COOKIE) {
                    name = crate::oidc::flow::cast_session_name(&state.pool, &token).await;
                }
            }
            if let Some(name) = name {
                request.extensions_mut().insert(super::Caster { name });
            }
        }
        return next.run(request).await;
    }

    let need = required(&method, &path);
    let resolved = match request.extensions().get::<ReplayIdentity>() {
        Some(ReplayIdentity(who)) => Some((who.clone(), Via::Other)),
        None => resolve(&state, request.headers()).await,
    };

    let Some((identity, via)) = resolved else {
        if need == Need::Open {
            return next.run(request).await;
        }
        if wants_html(&method, &path, request.headers()) {
            let target = request.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
            return Redirect::to(&format!("/login.html?next={}", urlencoding::encode(target)))
                .into_response();
        }
        return refuse(StatusCode::UNAUTHORIZED, "Bitte anmelden.");
    };

    let writing = method != Method::GET && method != Method::HEAD;
    if via == Via::Cookie && writing && !same_origin(request.headers(), request.uri()) {
        return refuse(StatusCode::FORBIDDEN, "Anfrage von einer fremden Seite abgelehnt.");
    }

    let allowed = match need {
        Need::Open | Need::Read => true,
        Need::Admin => identity.role == Role::Admin,
        Need::Manager => identity.role >= Role::Manager,
        Need::Content => {
            if identity.role >= Role::Manager {
                true
            } else if method == Method::POST && path == "/api/assets" {
                // An upload cannot wait as JSON: the handler stores it at once,
                // marked pending in the editor's draft.
                true
            } else {
                return crate::proposals::intercept(&state, &identity, request).await;
            }
        }
    };
    if !allowed {
        return refuse(StatusCode::FORBIDDEN, "Dafür reicht die Rolle dieses Kontos nicht.");
    }

    if via == Via::Token {
        request.extensions_mut().insert(ViaToken);
    }
    request.extensions_mut().insert(identity);
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderValue, Uri};

    fn headers(host: Option<&str>, origin: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(host) = host {
            h.insert(header::HOST, HeaderValue::from_str(host).unwrap());
        }
        h.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
        h
    }

    #[test]
    fn an_origin_of_this_host_passes_over_http1() {
        let uri: Uri = "/api/users".parse().unwrap();
        assert!(same_origin(&headers(Some("dev.test:3443"), "https://dev.test:3443"), &uri));
        assert!(!same_origin(&headers(Some("dev.test:3443"), "https://evil.test"), &uri));
    }

    #[test]
    fn over_http2_the_host_is_the_uri_authority() {
        // HTTP/2 has no Host header; hyper puts `:authority` into the URI. A
        // check that read only `Host` refused every cookie write a browser made
        // over HTTPS -- found on the first real device, not by the HTTP/1 tests.
        let uri: Uri = "https://10-78-11-58.clientctrl.cc/api/users".parse().unwrap();
        assert!(same_origin(&headers(None, "https://10-78-11-58.clientctrl.cc"), &uri));
        assert!(!same_origin(&headers(None, "https://evil.test"), &uri));
        let bare: Uri = "/api/users".parse().unwrap();
        assert!(!same_origin(&headers(None, "https://10-78-11-58.clientctrl.cc"), &bare));
    }
}
