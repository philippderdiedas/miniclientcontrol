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

/// Set on requests that arrived over the TLS listener, so the session cookie
/// can be marked `Secure` there.
#[derive(Clone)]
pub struct ViaTls;

pub fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value.to_string())
        .filter(|value| !value.is_empty())
}

fn decode_basic(value: &str) -> Option<(String, String)> {
    use base64::Engine;
    let encoded = value.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, password) = text.split_once(':')?;
    Some((user.to_string(), password.to_string()))
}

/// The identity behind a request, and whether it came from the cookie (which
/// is what the `Origin` check is for).
async fn resolve(state: &AppState, headers: &HeaderMap) -> Option<(Identity, bool)> {
    if let Some(token) = session_token(headers) {
        if let Some(who) = super::session_identity(&state.pool, &token).await {
            return Some((who, true));
        }
    }
    if let Some(value) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(who) = state.basic_cache.lock().await.get(value).cloned() {
            return Some((who, false));
        }
        if let Some((user, password)) = decode_basic(value) {
            if let Some((rescue_user, rescue_password)) = &state.rescue {
                if crate::settings::constant_time_eq(user.as_bytes(), rescue_user.as_bytes())
                    && crate::settings::constant_time_eq(password.as_bytes(), rescue_password.as_bytes())
                {
                    return Some((Identity::rescue(rescue_user), false));
                }
            }
            if let Some(who) = super::verify_password(&state.pool, &user, &password).await {
                state.basic_cache.lock().await.insert(value.to_string(), who.clone());
                return Some((who, false));
            }
        }
    }
    // Open only when nothing at all guards the device: no account *and* no
    // command-line credential. A venue that runs with only the flags today had
    // a protected admin, and an upgrade must not open it to the LAN.
    if state.rescue.is_none() && !super::any_user(&state.pool).await {
        return Some((Identity::open_mode(), false));
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
fn same_origin(headers: &HeaderMap, uri: &axum::http::Uri) -> bool {
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
    if !state.args.disable_cast && crate::cast::is_cast_public_path(&path) {
        return next.run(request).await;
    }

    let need = required(&method, &path);
    let resolved = match request.extensions().get::<ReplayIdentity>() {
        Some(ReplayIdentity(who)) => Some((who.clone(), false)),
        None => resolve(&state, request.headers()).await,
    };

    let Some((identity, via_cookie)) = resolved else {
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
    if via_cookie && writing && !same_origin(request.headers(), request.uri()) {
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
