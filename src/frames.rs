//! Making framed dashboards work in layouts.
//!
//! A layout page (`web/layout.html`) puts each widget in an `<iframe>`. A page
//! that forbids framing (`X-Frame-Options`, a CSP `frame-ancestors`) refuses to
//! render there, and a page that keeps its login in a `SameSite=Lax` cookie
//! loses that cookie as a cross-site frame. Measured in a lab and confirmed by
//! `tests/cast/test_layouts.py` case `[184]`: the fix is to strip the framing
//! headers and to relay a blocked login cookie as a partitioned
//! `SameSite=None`, per response, for frames *under a layout page only*.
//!
//! This runs on its own raw CDP connection to the display browser, not through
//! chromiumoxide, for two reasons: it needs the browser-level target stream and
//! every frame session flattened onto one socket, which chromiumoxide does not
//! expose; and interception must never sit in the path of the control loop's own
//! commands. A deliberate weakening of clickjacking protection, confined to
//! frames the operator put into a layout on a screen nobody clicks.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info};

use crate::models::AppState;

/// How long a paused response waits for its cookie events before release.
const SETTLE: Duration = Duration::from_millis(150);
/// The path of a page we build and whose frames we unlock.
const LAYOUT_PATH: &str = "/layout.html";
/// A partitioned cookie is valid only while the top-level page is this site --
/// our layout page, always served from loopback. That partition is what keeps a
/// guest's page from riding a dashboard's session.
const PARTITION_SITE: &str = "http://127.0.0.1";

/// A Content-Security-Policy value without its `frame-ancestors` directive, or
/// `None` when that was all it said. The rest of the policy still protects the
/// framed page, so only the one directive that blocks framing is dropped.
pub fn without_frame_ancestors(csp: &str) -> Option<String> {
    let kept: Vec<&str> = csp
        .split(';')
        .map(str::trim)
        .filter(|d| !d.is_empty() && !d.to_ascii_lowercase().starts_with("frame-ancestors"))
        .collect();
    (!kept.is_empty()).then(|| kept.join("; "))
}

/// Response headers with the two framing headers taken out.
fn strip_framing(headers: &[Value]) -> Vec<Value> {
    let mut out = Vec::with_capacity(headers.len());
    for h in headers {
        let name = h.get("name").and_then(Value::as_str).unwrap_or("");
        let lower = name.to_ascii_lowercase();
        if lower == "x-frame-options" {
            continue;
        }
        if lower == "content-security-policy" {
            let value = h.get("value").and_then(Value::as_str).unwrap_or("");
            if let Some(kept) = without_frame_ancestors(value) {
                out.push(json!({ "name": name, "value": kept }));
            }
            continue;
        }
        out.push(h.clone());
    }
    out
}

/// Whether a target's or frame's URL is a layout page of ours.
fn is_layout_url(url: &str) -> bool {
    url::Url::parse(url).map(|u| u.path() == LAYOUT_PATH).unwrap_or(false)
}

/// Keep the display browser's frames unlocked for as long as the process runs,
/// reconnecting after any drop -- like `browser_loop`.
pub async fn run(_state: AppState, cdp_url: String) {
    loop {
        match session(&cdp_url).await {
            Ok(()) => {}
            Err(e) => debug!("Frame unlocker for {} ended: {}", cdp_url, e),
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// The browser's own debugger websocket, from `/json/version`.
async fn browser_ws(cdp_url: &str) -> anyhow::Result<String> {
    let base = cdp_url.trim_end_matches('/');
    let body = http_get(&format!("{base}/json/version")).await?;
    let doc: Value = serde_json::from_slice(&body)?;
    doc.get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("no webSocketDebuggerUrl"))
}

/// A tiny loopback HTTP GET, on hyper like the rest of the tree (no reqwest).
async fn http_get(url: &str) -> anyhow::Result<Vec<u8>> {
    use http_body_util::BodyExt;
    use hyper_util::rt::TokioIo;
    let parsed = url::Url::parse(url)?;
    let host = parsed.host_str().ok_or_else(|| anyhow::anyhow!("no host"))?;
    let port = parsed.port().unwrap_or(80);
    let stream = tokio::net::TcpStream::connect((host, port)).await?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let path = match parsed.query() {
        Some(q) => format!("{}?{}", parsed.path(), q),
        None => parsed.path().to_string(),
    };
    // Host with the port: Chrome builds `webSocketDebuggerUrl` from this header,
    // and a bare host would drop the port from it.
    let request = hyper::Request::builder().uri(path).header("host", format!("{host}:{port}")).body(String::new())?;
    let response = sender.send_request(request).await?;
    Ok(response.into_body().collect().await?.to_bytes().to_vec())
}

/// A response paused by `Fetch`, waiting for its cookies before it is let go.
struct Held {
    request_id: String,
    session: String,
    headers: Vec<Value>,
    status: i64,
    since: Instant,
}

/// A `getResponseBody` in flight: when its reply comes, fulfil the response.
struct AwaitingBody {
    request_id: String,
    session: String,
    headers: Vec<Value>,
    status: i64,
}

async fn session(cdp_url: &str) -> anyhow::Result<()> {
    let ws_url = browser_ws(cdp_url).await.map_err(|e| anyhow::anyhow!("reading the browser ws url: {e}"))?;
    let (ws, _) = tokio_tungstenite::connect_async(&ws_url).await
        .map_err(|e| anyhow::anyhow!("connecting to {ws_url}: {e}"))?;
    let (mut sink, mut stream) = ws.split();

    // One writer task: everything sending to the socket goes through this
    // channel, so the reader loop never blocks on a slow write.
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let mut next_id: u64 = 0;
    let mut send = |method: &str, params: Value, session: Option<&str>| -> u64 {
        next_id += 1;
        let mut msg = json!({ "id": next_id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let _ = tx.send(msg.to_string());
        next_id
    };

    // Discover layout pages and attach to them; every frame under one is then
    // auto-attached, so interception reaches nested frames too.
    send("Target.setDiscoverTargets", json!({ "discover": true }), None);

    // The frame ids of layout pages' *main* frames: their own document is ours
    // and is never touched; their child frames are what we unlock.
    let mut layout_main_frames: HashSet<String> = HashSet::new();
    let mut attached_targets: HashSet<String> = HashSet::new();
    // Login cookies blocked for SameSite, from any widget frame -- only widget
    // frames have `Network` enabled, so every blocked cookie here is one. Keyed
    // by (name, domain), and relayed once. Global rather than per request,
    // because a reload splits a navigation across two ids and they would not
    // match (measured).
    let mut blocked_cookies: HashMap<(String, String), Value> = HashMap::new();
    let mut relayed: HashSet<(String, String)> = HashSet::new();
    let mut held: Vec<Held> = Vec::new();
    let mut awaiting: HashMap<u64, AwaitingBody> = HashMap::new();
    let mut unlocked_frames: HashSet<String> = HashSet::new();
    let mut reloaded: HashSet<String> = HashSet::new();
    let mut iframe_sessions: HashSet<String> = HashSet::new();
    let scroll_runtime = crate::browser::scroll_runtime_script();

    let mut tick = tokio::time::interval(Duration::from_millis(50));

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let mut i = 0;
                while i < held.len() {
                    // A short settle so the response's own `extraInfo` (its
                    // blocked cookies) has arrived before the redirect is
                    // followed; capped by HOLD.
                    let ready = held[i].since.elapsed() >= SETTLE;
                    if !ready {
                        i += 1;
                        continue;
                    }
                    let h = held.remove(i);
                    // Set every known blocked login cookie, once. Cheap and
                    // idempotent; the redirect that needs it is followed next.
                    for (key, cookie) in &blocked_cookies {
                        if relayed.insert(key.clone()) {
                            relay_cookie(cookie, &mut send);
                        }
                    }
                    let headers = strip_framing(&h.headers);
                    if (300..400).contains(&h.status) {
                        // A redirect has no body to fetch; fulfil it empty so
                        // Chromium follows it itself.
                        send("Fetch.fulfillRequest",
                             json!({ "requestId": h.request_id, "responseCode": h.status, "responseHeaders": headers, "body": "" }),
                             Some(&h.session));
                    } else {
                        // The body must be fetched and put back: new headers on
                        // `continueResponse` alone do not lift the framing block.
                        let id = send("Fetch.getResponseBody", json!({ "requestId": h.request_id }), Some(&h.session));
                        awaiting.insert(id, AwaitingBody {
                            request_id: h.request_id,
                            session: h.session,
                            headers,
                            status: h.status,
                        });
                    }
                }
            }
            frame = stream.next() => {
                let Some(frame) = frame else { break };
                let text = match frame {
                    Ok(Message::Text(t)) => t,
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => continue,
                };
                let Ok(msg): Result<Value, _> = serde_json::from_str(&text) else { continue };

                // A reply to one of our getResponseBody commands.
                if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                    if let Some(a) = awaiting.remove(&id) {
                        match msg.get("result") {
                            Some(result) => {
                                let body = result["body"].as_str().unwrap_or("").to_string();
                                let encoded = result["base64Encoded"].as_bool().unwrap_or(false);
                                // Fulfil always sends base64: encode a plain body.
                                let body = if encoded {
                                    body
                                } else {
                                    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, body)
                                };
                                send("Fetch.fulfillRequest",
                                     json!({ "requestId": a.request_id, "responseCode": a.status,
                                             "responseHeaders": a.headers, "body": body }),
                                     Some(&a.session));
                            }
                            None => {
                                // The body could not be read: show the page as it
                                // was rather than hang the frame on it.
                                send("Fetch.continueRequest", json!({ "requestId": a.request_id }), Some(&a.session));
                            }
                        }
                    }
                    continue;
                }

                let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                let params = &msg["params"];
                let event_session = msg["sessionId"].as_str().unwrap_or("").to_string();

                match method {
                    "Page.loadEventFired" if iframe_sessions.contains(&event_session) => {
                        // Install the scroll runtime in this widget frame; it
                        // registers the message listener `layout.html` sends to.
                        send("Runtime.evaluate",
                             json!({ "expression": scroll_runtime, "awaitPromise": false }),
                             Some(&event_session));
                    }
                    "Target.targetCreated" | "Target.targetInfoChanged" => {
                        let info = &params["targetInfo"];
                        if info["type"].as_str() == Some("page")
                            && is_layout_url(info["url"].as_str().unwrap_or(""))
                        {
                            let tid = info["targetId"].as_str().unwrap_or("").to_string();
                            if attached_targets.insert(tid.clone()) {
                                send("Target.attachToTarget", json!({ "targetId": tid, "flatten": true }), None);
                            }
                        }
                    }
                    "Target.attachedToTarget" => {
                        let session = params["sessionId"].as_str().unwrap_or("").to_string();
                        let info = &params["targetInfo"];
                        let url = info["url"].as_str().unwrap_or("");
                        let is_layout_page = info["type"].as_str() == Some("page") && is_layout_url(url);
                        if is_layout_url(url) {
                            layout_main_frames.insert(info["targetId"].as_str().unwrap_or("").to_string());
                        }
                        // An iframe target is a widget's frame: it gets the
                        // scroll runtime after it loads, so `layout.html`'s
                        // per-widget postMessage has a listener to reach.
                        if info["type"].as_str() == Some("iframe") {
                            iframe_sessions.insert(session.clone());
                            send("Page.enable", json!({}), Some(&session));
                        }
                        send("Network.enable", json!({}), Some(&session));
                        send("Fetch.enable",
                             json!({ "patterns": [{ "resourceType": "Document", "requestStage": "Response" }] }),
                             Some(&session));
                        // Wait a new child frame at its start, so its first
                        // document response is caught -- an iframe that was
                        // already loading when we attached would be missed.
                        send("Target.setAutoAttach",
                             json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
                             Some(&session));
                        // A target we auto-attached is paused at start; let it
                        // run now that its interception is set. Harmless for the
                        // page target, which was not waiting.
                        send("Runtime.runIfWaitingForDebugger", json!({}), Some(&session));
                        // The layout page was likely already loaded when we
                        // attached, so its iframes never passed through the
                        // interception just set up. Reload it once to send them
                        // through -- a brief flash, only on the first attach.
                        if is_layout_page && reloaded.insert(session.clone()) {
                            send("Page.enable", json!({}), Some(&session));
                            send("Page.reload", json!({ "ignoreCache": false }), Some(&session));
                        }
                    }
                    "Network.responseReceivedExtraInfo" => {
                        if let Some(list) = params["blockedCookies"].as_array() {
                            for bc in list {
                                let same_site = bc["blockedReasons"].as_array()
                                    .map(|rs| rs.iter().any(|r| r.as_str().unwrap_or("").contains("SameSite")))
                                    .unwrap_or(false);
                                if same_site {
                                    if let Some(cookie) = bc.get("cookie") {
                                        let key = (
                                            cookie["name"].as_str().unwrap_or_default().to_string(),
                                            cookie["domain"].as_str().unwrap_or_default().to_string(),
                                        );
                                        blocked_cookies.entry(key).or_insert_with(|| cookie.clone());
                                    }
                                }
                            }
                        }
                    }
                    "Fetch.requestPaused" => {
                        let session = msg["sessionId"].as_str().unwrap_or("").to_string();
                        let request_id = params["requestId"].as_str().unwrap_or("").to_string();
                        let frame_id = params["frameId"].as_str().unwrap_or("").to_string();
                        // The layout page's own document is ours -- untouched.
                        if layout_main_frames.contains(&frame_id) {
                            send("Fetch.continueRequest", json!({ "requestId": request_id }), Some(&session));
                            continue;
                        }
                        if unlocked_frames.insert(frame_id) {
                            let url = params["request"]["url"].as_str().unwrap_or("");
                            info!("Layout: unlocking a widget frame at {}", crate::browser::redact_str(url));
                        }
                        held.push(Held {
                            request_id,
                            session,
                            headers: params["responseHeaders"].as_array().cloned().unwrap_or_default(),
                            status: params["responseStatusCode"].as_i64().unwrap_or(200),
                            since: Instant::now(),
                        });
                    }
                    _ => {}
                }
            }
        }
    }

    drop(tx);
    writer.abort();
    Ok(())
}

fn relay_cookie(cookie: &Value, send: &mut impl FnMut(&str, Value, Option<&str>) -> u64) {
    let name = cookie["name"].as_str().unwrap_or_default();
    send("Storage.setCookies",
         json!({ "cookies": [{
             "name": name,
             "value": cookie["value"].as_str().unwrap_or_default(),
             "domain": cookie["domain"].as_str().unwrap_or_default(),
             "path": cookie["path"].as_str().unwrap_or("/"),
             "secure": true,
             "httpOnly": cookie["httpOnly"].as_bool().unwrap_or(false),
             "sameSite": "None",
             "partitionKey": { "topLevelSite": PARTITION_SITE, "hasCrossSiteAncestor": true },
         }]}),
         None);
    debug!("Layout: relayed a login cookie '{}' as partitioned SameSite=None", name);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_frame_ancestors_goes() {
        assert_eq!(without_frame_ancestors("frame-ancestors 'none'"), None);
        assert_eq!(
            without_frame_ancestors("default-src 'self'; frame-ancestors 'none'; img-src *").as_deref(),
            Some("default-src 'self'; img-src *")
        );
        assert_eq!(without_frame_ancestors("default-src 'self'").as_deref(), Some("default-src 'self'"));
    }

    #[test]
    fn framing_headers_are_removed_and_csp_kept() {
        let headers = vec![
            json!({ "name": "X-Frame-Options", "value": "DENY" }),
            json!({ "name": "Content-Security-Policy", "value": "default-src 'self'; frame-ancestors 'none'" }),
            json!({ "name": "Content-Type", "value": "text/html" }),
        ];
        let out = strip_framing(&headers);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|h| h["name"].as_str().unwrap().to_ascii_lowercase() != "x-frame-options"));
        assert_eq!(out[0]["value"], "default-src 'self'");
        assert_eq!(out[1]["name"], "Content-Type");
    }
}
