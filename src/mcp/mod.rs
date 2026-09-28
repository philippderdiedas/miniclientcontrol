//! A Model Context Protocol server, so an LLM client can drive the device.
//!
//! Streamable HTTP at `POST /mcp`, answering plain JSON (the transport allows
//! a server to never open an event stream). It is deliberately thin: there is
//! no second API here. `api_request` replays a call through the application's
//! own router as the caller -- the way an approved proposal is applied -- so
//! roles, proposals, validation, webhooks and signals are exactly those of a
//! direct request by that account. An editor's token proposes; it never writes.
//! The reference the LLM reads is the README's API section, compiled in, so
//! it cannot describe an API this binary does not have.
//!
//! The two other tools exist because JSON in, JSON out cannot carry them: a
//! screenshot comes back as an image the model can look at, and an upload is
//! built into the multipart request `/api/assets` expects.

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Extension, Json, Router};
use base64::Engine;
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::accounts::middleware::{same_origin, ReplayIdentity, ViaToken};
use crate::accounts::Identity;
use crate::models::AppState;

/// The newest revision this server speaks; offered when a client asks for one
/// it does not know. The older ones differ only in what this server never uses.
const LATEST: &str = "2025-06-18";
const SUPPORTED: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// A tool result is text the model reads in full; a playlist dump of a busy
/// venue is large, and past this it is cut with a note saying so.
const MAX_TEXT: usize = 100_000;
/// An upload arrives base64 inside JSON-RPC: 48 MB of file, give or take.
const MAX_BODY: usize = 64 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/mcp", post(handle).get(no_stream).delete(no_stream))
        .layer(DefaultBodyLimit::max(MAX_BODY))
}

/// No server-initiated stream and no session to end: the transport's answer
/// to both is `405`.
async fn no_stream() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST")]).into_response()
}

/// Who the tool calls run as, carried from the `/mcp` request onto each replay.
#[derive(Clone)]
struct Caller {
    who: Identity,
    peer: SocketAddr,
    token: bool,
}

async fn handle(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Extension(who): Extension<Identity>,
    token: Option<Extension<ViaToken>>,
    headers: HeaderMap,
    uri: Uri,
    Json(message): Json<Value>,
) -> Response {
    // The transport requires it, against DNS rebinding: a page on another site
    // must not reach this through a victim's browser. No `Origin` is a client
    // that is not a browser, which is the usual case.
    if headers.contains_key(header::ORIGIN) && !same_origin(&headers, &uri) {
        return (StatusCode::FORBIDDEN, Json(json!({ "error": "Fremder Origin." }))).into_response();
    }
    let caller = Caller { who, peer, token: token.is_some() };
    match message {
        // Batches were dropped in 2025-06-18 but cost nothing to answer.
        Value::Array(batch) => {
            let mut answers = Vec::new();
            for entry in batch {
                if let Some(answer) = dispatch(&state, &caller, entry).await {
                    answers.push(answer);
                }
            }
            if answers.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                Json(Value::Array(answers)).into_response()
            }
        }
        single => match dispatch(&state, &caller, single).await {
            Some(answer) => Json(answer).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

fn reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn failure(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// One JSON-RPC message. `None` for a notification or a client's response,
/// which get no answer.
async fn dispatch(state: &AppState, caller: &Caller, message: Value) -> Option<Value> {
    let method = message.get("method").and_then(Value::as_str)?.to_string();
    let id = message.get("id").cloned()?;
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    Some(match method.as_str() {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(LATEST);
            let version = if SUPPORTED.contains(&asked) { asked } else { LATEST };
            reply(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "miniclientcontrol", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": instructions(&caller.who),
                }),
            )
        }
        "ping" => reply(id, json!({})),
        "tools/list" => reply(id, json!({ "tools": tools() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
            let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match call_tool(state, caller, name, &arguments).await {
                Some(result) => reply(id, result),
                None => failure(id, -32602, &format!("Unknown tool: {name}")),
            }
        }
        _ => failure(id, -32601, &format!("Method not found: {method}")),
    })
}

fn instructions(who: &Identity) -> String {
    let role = if who.open { "admin (no accounts exist, the device is open)".to_string() } else {
        format!("{} (account '{}')", who.role.as_str(), who.name)
    };
    format!(
        "This server controls a digital-signage device (miniclientcontrol): screens, \
         playlists, items, assets, timetables, overrides, settings. You act as {role}; \
         everything you do is exactly what that account may do through the HTTP API. \
         Call `api_reference` once before your first `api_request`. With several \
         screens declared, use the display-scoped paths (`/api/displays/{{name}}/…`) — \
         the unscoped ones answer 409. An editor's content writes answer 202 and wait \
         in a draft; submit it with POST /api/changesets/draft/submit when the user \
         wants a manager to review it. Use `screenshot` to see what a screen shows."
    )
}

fn tools() -> Value {
    json!([
        {
            "name": "api_reference",
            "title": "API reference",
            "description": "The device's HTTP API: every endpoint with its body and its rules. Read it before the first api_request.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "api_request",
            "title": "Call the API",
            "description": "Make one request to the device's HTTP API as the signed-in account and get the status and JSON answer back. Roles apply exactly as for any client: a write the account may not make is refused, an editor's content write becomes a proposal (202).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "method": { "type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"] },
                    "path": { "type": "string", "description": "Starts with /api/, may carry a query string, e.g. /api/playlist?playlist_id=2" },
                    "body": { "description": "JSON body for POST/PUT/PATCH/DELETE; omit for none." }
                },
                "required": ["method", "path"],
                "additionalProperties": false
            },
            "annotations": { "destructiveHint": true, "openWorldHint": false }
        },
        {
            "name": "screenshot",
            "title": "Look at a screen",
            "description": "What a display shows right now, as an image (at the screen's own size, cached up to 10 s). Display names come from GET /api/displays.",
            "inputSchema": {
                "type": "object",
                "properties": { "display": { "type": "string" } },
                "required": ["display"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "upload_asset",
            "title": "Upload an asset",
            "description": "Upload a file (image, video, PDF) as a new asset. Returns the created asset(s); add it to a playlist with POST /api/playlist and its asset_id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "filename": { "type": "string", "description": "With extension; the type is guessed from it." },
                    "content_base64": { "type": "string" },
                    "duration": { "type": "number", "description": "A video's length in seconds, if known." }
                },
                "required": ["filename", "content_base64"],
                "additionalProperties": false
            }
        }
    ])
}

fn text_result(text: String, is_error: bool) -> Value {
    let text = if text.len() > MAX_TEXT {
        let mut cut = MAX_TEXT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n… [cut: {} of {} bytes shown — narrow the request]", &text[..cut], cut, text.len())
    } else {
        text
    };
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

async fn call_tool(state: &AppState, caller: &Caller, name: &str, arguments: &Value) -> Option<Value> {
    Some(match name {
        "api_reference" => text_result(api_reference().to_string(), false),
        "api_request" => {
            let method = arguments.get("method").and_then(Value::as_str).unwrap_or_default();
            let path = arguments.get("path").and_then(Value::as_str).unwrap_or_default();
            let Some(method) = allowed_method(method) else {
                return Some(text_result(format!("Unsupported method '{method}'."), true));
            };
            if let Err(why) = check_path(path) {
                return Some(text_result(why.to_string(), true));
            }
            let body = arguments.get("body").filter(|b| !b.is_null());
            let (status, _, bytes) = match body {
                Some(body) => send(state, caller, method, path, Some("application/json"), body.to_string().into_bytes()).await,
                None => send(state, caller, method, path, None, Vec::new()).await,
            };
            let shown = match serde_json::from_slice::<Value>(&bytes) {
                Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_default(),
                Err(_) => String::from_utf8_lossy(&bytes).into_owned(),
            };
            text_result(format!("HTTP {}\n{}", status.as_u16(), shown), !status.is_success())
        }
        "screenshot" => {
            let display = arguments.get("display").and_then(Value::as_str).unwrap_or_default();
            // It becomes a path segment; the same alphabet a display name has.
            if display.is_empty() || !display.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
                return Some(text_result("Not a display name.".into(), true));
            }
            let path = format!("/api/displays/{display}/screenshot");
            let (status, headers, bytes) = send(state, caller, Method::GET, &path, None, Vec::new()).await;
            if !status.is_success() {
                return Some(text_result(
                    format!("HTTP {}\n{}", status.as_u16(), String::from_utf8_lossy(&bytes)),
                    true,
                ));
            }
            let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
            let mut note = format!("Screen '{display}'");
            if let Some(age) = header("x-screenshot-age") {
                note.push_str(&format!(", picture {age} s old"));
            }
            if let Some(since) = header("x-screen-frozen-since") {
                note.push_str(&format!(", FROZEN since {since}"));
            }
            json!({
                "content": [
                    { "type": "image", "data": base64::engine::general_purpose::STANDARD.encode(&bytes), "mimeType": "image/jpeg" },
                    { "type": "text", "text": note }
                ],
                "isError": false
            })
        }
        "upload_asset" => {
            let filename = arguments.get("filename").and_then(Value::as_str).unwrap_or_default();
            let content = arguments.get("content_base64").and_then(Value::as_str).unwrap_or_default();
            if filename.trim().is_empty() {
                return Some(text_result("A filename is required.".into(), true));
            }
            let Ok(file) = base64::engine::general_purpose::STANDARD.decode(content.trim()) else {
                return Some(text_result("content_base64 is not valid base64.".into(), true));
            };
            if file.is_empty() {
                return Some(text_result("The file is empty.".into(), true));
            }
            let duration = arguments.get("duration").and_then(Value::as_f64);
            let (content_type, body) = multipart(filename, &file, duration);
            let (status, _, bytes) = send(state, caller, Method::POST, "/api/assets", Some(&content_type), body).await;
            text_result(
                format!("HTTP {}\n{}", status.as_u16(), String::from_utf8_lossy(&bytes)),
                !status.is_success(),
            )
        }
        _ => return None,
    })
}

fn allowed_method(raw: &str) -> Option<Method> {
    match raw.to_ascii_uppercase().as_str() {
        "GET" => Some(Method::GET),
        "POST" => Some(Method::POST),
        "PUT" => Some(Method::PUT),
        "PATCH" => Some(Method::PATCH),
        "DELETE" => Some(Method::DELETE),
        _ => None,
    }
}

/// Only the operator API. The guest's cast protocol (claim, pair, the socket)
/// is a browser's, not a tool's -- and its routes skip authentication, so
/// through here they would run with no account at all. Signing in and out has
/// no meaning for a caller that is already authenticated.
fn check_path(path: &str) -> Result<(), &'static str> {
    if !path.starts_with("/api/") {
        return Err("The path must start with /api/.");
    }
    let bare = path.split(['?', '#']).next().unwrap_or(path);
    if bare.split('/').any(|segment| segment == "." || segment == "..") {
        return Err("Dot segments are not allowed in the path.");
    }
    if path.parse::<Uri>().is_err() {
        return Err("Not a valid path; percent-encode what needs it.");
    }
    if crate::cast::is_cast_public_path(bare) {
        return Err("The guest cast endpoints are for a guest's browser, not for this tool.");
    }
    if matches!(bare, "/api/login" | "/api/logout") {
        return Err("Signing in or out is not needed here: the call already runs as the account.");
    }
    Ok(())
}

/// One request through the application's own router, as the caller. The real
/// peer, not loopback: a replay from loopback would pass the display
/// exemption on paths a remote caller has no claim to. A token's replay still
/// carries `ViaToken`, so a token cannot reach its account's credentials by
/// going round through here.
async fn send(
    state: &AppState,
    caller: &Caller,
    method: Method,
    path: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let Some(router) = state.router.get() else {
        return (StatusCode::SERVICE_UNAVAILABLE, HeaderMap::new(), Vec::new());
    };
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    let Ok(mut request) = builder.body(Body::from(body)) else {
        return (StatusCode::BAD_REQUEST, HeaderMap::new(), Vec::new());
    };
    request.extensions_mut().insert(ConnectInfo(caller.peer));
    request.extensions_mut().insert(ReplayIdentity(caller.who.clone()));
    if caller.token {
        request.extensions_mut().insert(ViaToken);
    }
    let response = match router.clone().oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 32 * 1024 * 1024)
        .await
        .map(|b| b.to_vec())
        .unwrap_or_default();
    (status, headers, bytes)
}

/// The body `/api/assets` reads: an optional `duration` part, then the file.
/// No `Content-Type` on the file part, so the handler guesses it from the name
/// exactly as it does for a browser that sends none.
fn multipart(filename: &str, file: &[u8], duration: Option<f64>) -> (String, Vec<u8>) {
    let boundary = format!("mcc-{}", uuid::Uuid::new_v4().simple());
    let safe_name: String = filename.chars().filter(|c| !matches!(c, '"' | '\r' | '\n' | '\\')).collect();
    let mut body = Vec::with_capacity(file.len() + 512);
    if let Some(seconds) = duration {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"duration\"\r\n\r\n{seconds}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{safe_name}\"\r\n\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

const README: &str = include_str!("../../README.md");

/// The README's API section, from its heading to the next top-level section.
/// A test holds both markers in place, so moving them fails the build's tests
/// rather than handing the model an empty reference.
fn api_reference() -> &'static str {
    let start = README.find("## API Overview").unwrap_or(0);
    let end = README[start..].find("\n## Screen Casting").map(|n| start + n).unwrap_or(README.len());
    &README[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reference_is_the_readme_api_section() {
        let reference = api_reference();
        assert!(reference.starts_with("## API Overview"));
        assert!(reference.contains("/api/displays/{name}/schedule"));
        assert!(!reference.contains("## Screen Casting"));
        assert!(reference.len() < README.len() / 2, "the end marker went missing");
    }

    #[test]
    fn only_the_operator_api_is_reachable() {
        assert!(check_path("/api/playlist?playlist_id=2").is_ok());
        assert!(check_path("/api/displays/foyer/override").is_ok());
        assert!(check_path("/admin.html").is_err());
        assert!(check_path("/api/../admin.html").is_err());
        assert!(check_path("/api/cast/claim").is_err());
        assert!(check_path("/api/cast/ws?role=sender").is_err());
        assert!(check_path("/api/login").is_err());
        assert!(check_path("/api/play list").is_err());
    }

    #[test]
    fn the_upload_body_is_what_the_asset_handler_reads() {
        let (content_type, body) = multipart("a\"b.png", b"PNG", Some(12.5));
        let boundary = content_type.strip_prefix("multipart/form-data; boundary=").unwrap();
        let text = String::from_utf8_lossy(&body);
        let duration = text.find("name=\"duration\"").unwrap();
        let file = text.find("filename=\"ab.png\"").unwrap();
        assert!(duration < file, "the duration applies to the parts after it");
        assert!(text.ends_with(&format!("--{boundary}--\r\n")));
    }
}
