//! Screen casting: signaling relay and session lifecycle.
//!
//! A sender (someone's laptop on the LAN) and the display browser exchange a
//! WebRTC offer/answer through this server, then stream peer-to-peer. The server
//! never touches the media — it relays opaque SDP/ICE blobs and owns the session
//! lifecycle.
//!
//! ## A cast is an override
//!
//! Rather than adding a second way to drive the screen, starting a cast pins the
//! existing override to `cast_display.html` and ending it puts back whatever was
//! there before. The browser loop needs no changes at all: it already interrupts
//! the current item on `override_signal`, and already resumes the playlist from
//! `resume_after_order` once the override clears.
//!
//! Two existing details in `browser.rs` are load-bearing here:
//!
//! * `run_override_loop` skips a notification whose override is unchanged, so a
//!   redundant `notify_one()` will not re-navigate the page and tear down the
//!   live `RTCPeerConnection` mid-cast.
//! * The per-item `tokio::select!` watches `override_signal`, so a cast starts
//!   immediately instead of waiting out the current item's duration.
//!
//! ## Differences from picklecast, deliberately
//!
//! There are exactly two peers, so the protocol carries no room names and no
//! peer addressing — picklecast needed both because public WebTorrent trackers
//! could deliver duplicate peers, which is also why its display had to ignore a
//! second offer. With one relay and a server-enforced single sender, none of
//! that applies.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::models::{AppState, CastAuth, OverrideItem, ScrollMode};

/// Code alphabet without I/O/0/1, so a code read off a screen cannot be mistyped.
const CODE_CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const CODE_LEN: usize = 4;

/// How long a pairing code stays valid once it is on screen.
const PAIRING_TTL: Duration = Duration::from_secs(30);
/// Grace period after the sender's socket drops, so a page reload does not end
/// the cast and bounce the display back to the playlist for two seconds.
const SENDER_GRACE: Duration = Duration::from_secs(5);
/// How long a claimed session is held before streaming starts. Has to cover the
/// guest reading the code, clicking share, and picking a window in the browser's
/// own dialog -- all of which is unhurried human time.
const RESERVATION_TTL: Duration = Duration::from_secs(120);
/// The display browser has this long to load the cast page and connect back.
const DISPLAY_TIMEOUT: Duration = Duration::from_secs(30);
/// Wrong codes tolerated per address before that address is locked out.
const MAX_CODE_ATTEMPTS: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);

/// Ping interval. A laptop whose lid closes stops answering without ever sending
/// a TCP FIN, so the socket looks healthy until something probes it.
const PING_INTERVAL: Duration = Duration::from_secs(15);
/// No frame at all (not even a pong) for this long means the peer is gone.
const PEER_IDLE_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Sender,
    Display,
}

struct Peer {
    tx: mpsc::UnboundedSender<Message>,
}

struct Pairing {
    code: String,
    expires_at: Instant,
}

/// A session claimed by a sender that has not started streaming yet.
///
/// This exists so the code is checked, and the slot taken, *before* the browser
/// opens its screen picker. Without it a guest picks a window and only then
/// learns the code was wrong, and two guests can be in the picker at once with
/// one of them guaranteed to lose.
struct Reservation {
    ticket: String,
    addr: IpAddr,
    expires_at: Instant,
}

struct Attempts {
    failures: u32,
    locked_until: Option<Instant>,
    last_seen: Instant,
}

/// The largest frame the display can actually put on screen, announced by the
/// display page itself.
///
/// This exists because a frame can decode perfectly and still show as a black
/// rectangle: the kiosk Pi's Broadcom VC4 has `MAX_TEXTURE_SIZE` 2048, and a
/// 2880-wide share never reaches the compositor. Only the display knows its own
/// GPU and panel, so it does the measuring; the server just carries the number
/// to the sender, which is the side that can do something about it.
#[derive(Clone, Copy, Serialize, Deserialize, Debug)]
pub struct DisplayLimits {
    /// Longest edge, in device pixels.
    max_edge: u32,
}

impl DisplayLimits {
    /// Below this a "limit" is more likely a bug on the display side than a real
    /// constraint, and honouring it would shrink the cast to something useless.
    const MIN_EDGE: u32 = 320;
    /// Above this there is nothing to constrain, and a number this large is
    /// noise. Every GPU that reports more can take whatever a screen produces.
    const MAX_EDGE: u32 = 16384;

    fn sane(self) -> Option<Self> {
        (Self::MIN_EDGE..=Self::MAX_EDGE)
            .contains(&self.max_edge)
            .then_some(self)
    }
}

#[derive(Default)]
pub struct CastSession {
    sender: Option<Peer>,
    display: Option<Peer>,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    sender_addr: Option<IpAddr>,
    /// Whatever the override held before the cast took it over.
    previous_override: Option<OverrideItem>,
    /// True while the override on screen is the one we installed.
    holding_override: bool,
    pairing: Option<Pairing>,
    reservation: Option<Reservation>,
    attempts: HashMap<IpAddr, Attempts>,
    /// Last limit a display announced. Deliberately kept when a session ends: it
    /// is a property of the hardware, not of the cast, and remembering it is what
    /// lets the *next* sender constrain its capture before the first frame
    /// instead of showing a black rectangle until the display checks in.
    display_limits: Option<DisplayLimits>,
    /// Bumped on every activate/deactivate so a delayed watchdog task can tell
    /// whether the session it was launched for is still the current one.
    epoch: u64,
}

impl CastSession {
    fn is_active(&self) -> bool {
        self.holding_override || self.sender.is_some()
    }

    /// Someone is streaming, or has claimed the slot and is still within the
    /// window to start. Expired reservations do not count, so an abandoned tab
    /// cannot block the display forever.
    fn is_taken(&self) -> bool {
        self.sender.is_some() || self.live_reservation().is_some()
    }

    /// Taken by somebody *other* than this address.
    ///
    /// The distinction matters for what a guest is told. Holding a reservation
    /// and then being informed that "someone else is casting" is a dead end the
    /// guest cannot act on -- and it is their own reservation. The claim endpoint
    /// already lets the same address re-claim, so the answer here has to agree
    /// with that.
    fn taken_by_other(&self, addr: IpAddr) -> bool {
        let sender_elsewhere = self.sender.is_some() && self.sender_addr != Some(addr);
        let reserved_elsewhere = self
            .live_reservation()
            .is_some_and(|held| held.addr != addr);
        sender_elsewhere || reserved_elsewhere
    }

    fn live_reservation(&self) -> Option<&Reservation> {
        self.reservation
            .as_ref()
            .filter(|held| Instant::now() < held.expires_at)
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/cast/ws", get(cast_ws))
        .route("/api/cast/state", get(cast_state))
        .route("/api/cast/session", axum::routing::delete(stop_cast))
        .route("/api/cast/pair", post(start_pairing))
        .route("/api/cast/info", get(cast_info))
        .route("/api/cast/qr.svg", get(cast_qr))
        .route("/api/cast/audio", get(read_audio).post(control_audio))
        .route(
            "/api/cast/claim",
            post(claim_session).delete(release_session),
        )
}

/// Paths a cast sender must reach *without* credentials.
///
/// Distinct from `is_display_path` in main.rs, which is loopback-only: the sender
/// is a guest's laptop somewhere on the LAN, and gating it behind basic auth would
/// mean handing out the operator password to everyone who wants to share a screen.
/// The cast's own auth (`--cast-auth`) is what guards it instead.
pub fn is_cast_public_path(path: &str) -> bool {
    matches!(
        path,
        // The sender page is the site root: a guest gets handed
        // `https://<device>:3443` and nothing more.
        "/"
            | "/index.html"
            | "/cast.html"
            | "/cast.js"
            | "/cast_display.html"
            | "/api/cast/ws"
            | "/api/cast/pair"
            | "/api/cast/info"
            | "/api/cast/qr.svg"
            | "/api/cast/audio"
            | "/api/cast/claim"
    )
}

/// The address a guest is handed. Deliberately the bare root: short enough to
/// read off a screen and type by hand. `--public-url` decides whether that is the
/// LAN address, an mDNS name or something the operator supplied.
/// The URL a guest is told to open. Resolved every time rather than stored: it
/// depends on `--public-url`, on the port actually bound, and on the machine's
/// current address, all of which can change without anybody editing anything.
pub fn sender_url(state: &AppState) -> String {
    crate::tls::public_base_url(&state.args.public_url, state.cast_tls_port)
}

/// QR code for the guest URL, as SVG.
///
/// Rendered here rather than in the page: the device is often offline, so a
/// client-side library would have to be vendored, and an SVG the display can
/// scale is a few hundred bytes.
pub async fn cast_qr(State(state): State<AppState>) -> Response {
    qr_svg(&sender_url(&state))
}

/// The QR code as a module matrix, one string of `0`/`1` per row.
///
/// This exists instead of a URL because the overlay lives in *someone else's*
/// document: Chromium's Local Network Access blocks a page on a public origin
/// from loading anything off `127.0.0.1` without a permission click, and a kiosk
/// has nobody to click it. Handing over the modules and drawing them as inline
/// SVG needs no request at all -- which also sidesteps an `img-src` CSP, where
/// even a `data:` URL would be refused.
pub fn qr_matrix(text: &str) -> Option<Vec<String>> {
    let code = qrcode::QrCode::new(text.as_bytes())
        .inspect_err(|e| warn!("Could not encode '{}' as a QR code: {}", text, e))
        .ok()?;
    let width = code.width();
    let modules = code.to_colors();
    Some(
        modules
            .chunks(width)
            .map(|row| {
                row.iter()
                    .map(|color| match color {
                        qrcode::Color::Dark => '1',
                        qrcode::Color::Light => '0',
                    })
                    .collect()
            })
            .collect(),
    )
}

/// Render any text as a QR code SVG.
///
/// Server-side because the device is often offline, so a client-side library
/// would have to be vendored, and because an SVG scales to whatever the panel
/// is. Shared with the overlay, which needs the same thing for arbitrary text.
pub fn qr_svg(text: &str) -> Response {
    let code = match qrcode::QrCode::new(text.as_bytes()) {
        Ok(code) => code,
        Err(e) => {
            warn!("Could not encode '{}' as a QR code: {}", text, e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "qr encoding failed").into_response();
        }
    };
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(240, 240)
        // white margin, so it still scans against a dark page background
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .dark_color(qrcode::render::svg::Color("#000000"))
        .build();

    (
        [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
        svg,
    )
        .into_response()
}

fn cast_display_url(port: u16) -> String {
    format!("http://127.0.0.1:{}/cast_display.html", port)
}

fn generate_code() -> String {
    let mut rng = rand::rng();
    (0..CODE_LEN)
        .map(|_| CODE_CHARS[rng.random_range(0..CODE_CHARS.len())] as char)
        .collect()
}

fn generate_ticket() -> String {
    let mut rng = rand::rng();
    (0..32)
        .map(|_| char::from_digit(rng.random_range(0..16), 16).unwrap_or('0'))
        .collect()
}

/// Length-independent, branch-free comparison, so a wrong code cannot be found
/// one character at a time by timing the response.
fn codes_match(expected: &str, provided: &str) -> bool {
    let a = expected.as_bytes();
    let b = provided.as_bytes();
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

// --------------------------------------------------------------- override

/// Pin the display to the cast page, remembering whatever was there before.
async fn activate_display(state: &AppState) {
    let mut session = state.cast.lock().await;
    if session.holding_override {
        return;
    }

    let url = cast_display_url(state.args.port);
    {
        let mut current = state.override_item.lock().await;
        session.previous_override = current.clone();
        *current = Some(OverrideItem {
            asset_id: None,
            url: Some(url),
            local_path: None,
            mimetype: None,
            scroll_config: ScrollMode::None,
        });
    }
    session.holding_override = true;
    session.started_at = Some(chrono::Utc::now());
    session.epoch += 1;
    let epoch = session.epoch;
    drop(session);

    state.override_signal.notify_one();
    info!("Cast: display pinned to the cast page");
    watch_display_arrival(state.clone(), epoch);
}

/// Release the display and let the playlist pick up where it left off.
async fn deactivate_display(state: &AppState) {
    let mut session = state.cast.lock().await;
    if !session.holding_override {
        return;
    }

    let ours = cast_display_url(state.args.port);
    {
        let mut current = state.override_item.lock().await;
        // Only restore if what is on screen is still the override we installed.
        // An operator who set a different one mid-cast made a newer decision, and
        // silently reverting it would look like the UI ignoring them.
        let still_ours = current.as_ref().and_then(|item| item.url.as_deref()) == Some(ours.as_str());
        if still_ours {
            *current = session.previous_override.take();
        } else {
            debug!("Cast: override changed during the cast, leaving it alone");
        }
    }
    session.previous_override = None;
    session.holding_override = false;
    session.started_at = None;
    session.sender_addr = None;
    session.pairing = None;
    session.reservation = None;
    session.epoch += 1;
    drop(session);

    state.override_signal.notify_one();
    info!("Cast: display released, playlist resumes");
}

/// Close both sockets and hand the screen back.
pub async fn end_session(state: &AppState) {
    let (sender, display) = {
        let mut session = state.cast.lock().await;
        // Cleared here rather than only in `deactivate_display`, which returns
        // early when no override is held -- precisely the case where someone has
        // reserved the session but not started streaming.
        session.reservation = None;
        (session.sender.take(), session.display.take())
    };
    for peer in [sender, display].into_iter().flatten() {
        let _ = peer.tx.send(Message::Close(None));
    }
    deactivate_display(state).await;
}

// -------------------------------------------------------------- watchdogs

/// The sender's socket went away. Wait out a short grace period before ending the
/// cast, so a page reload does not bounce the display back to the playlist.
fn watch_sender_grace(state: AppState, epoch: u64) {
    tokio::spawn(async move {
        tokio::time::sleep(SENDER_GRACE).await;
        let stale = {
            let session = state.cast.lock().await;
            session.epoch == epoch && session.sender.is_none() && session.holding_override
        };
        if stale {
            info!("Cast: sender did not return, ending session");
            end_session(&state).await;
        }
    });
}

/// The display browser is being navigated to the cast page. If it never connects
/// back, the sender would sit forever on "waiting for display".
fn watch_display_arrival(state: AppState, epoch: u64) {
    tokio::spawn(async move {
        tokio::time::sleep(DISPLAY_TIMEOUT).await;
        let missing = {
            let session = state.cast.lock().await;
            session.epoch == epoch && session.display.is_none() && session.holding_override
        };
        if missing {
            warn!("Cast: display never connected back within {:?}", DISPLAY_TIMEOUT);
            let sender_tx = {
                let session = state.cast.lock().await;
                session.sender.as_ref().map(|peer| peer.tx.clone())
            };
            if let Some(tx) = sender_tx {
                let _ = tx.send(error_frame("display", "Das Display hat sich nicht gemeldet."));
            }
            end_session(&state).await;
        }
    });
}

/// A pairing code nobody used must not hold the screen hostage.
fn watch_pairing_expiry(state: AppState, epoch: u64) {
    tokio::spawn(async move {
        tokio::time::sleep(PAIRING_TTL).await;
        let unused = {
            let session = state.cast.lock().await;
            session.epoch == epoch && session.sender.is_none() && session.holding_override
        };
        if unused {
            info!("Cast: pairing code expired unused");
            end_session(&state).await;
        }
    });
}

// ------------------------------------------------------------------- auth

/// Checks a sender's code against the configured policy, with a per-address
/// lockout so a four-character code cannot simply be enumerated.
async fn authorize_sender(
    state: &AppState,
    addr: IpAddr,
    provided: Option<&str>,
) -> Result<(), String> {
    let settings = {
        let settings = state.settings.read().await;
        (settings.cast_enabled, settings.cast_auth, settings.cast_code.clone())
    };
    let (enabled, auth_mode, configured_code) = settings;

    let mut session = state.cast.lock().await;

    if let Some(entry) = session.attempts.get(&addr) {
        if let Some(until) = entry.locked_until {
            if Instant::now() < until {
                return Err("Zu viele Fehlversuche. Bitte kurz warten.".to_string());
            }
        }
    }

    if !enabled {
        return Err("Übertragung ist derzeit deaktiviert.".to_string());
    }

    let outcome = match auth_mode {
        CastAuth::None => Ok(()),
        CastAuth::Code => {
            // Failing closed matters: falling back to "no code" here would turn a
            // misconfiguration into an open cast endpoint.
            if configured_code.is_empty() {
                Err("Cast-Code ist nicht konfiguriert.".to_string())
            } else if codes_match(&configured_code, provided.unwrap_or("")) {
                Ok(())
            } else {
                Err("Falscher Code.".to_string())
            }
        }
        CastAuth::Pairing => match session.pairing.as_ref() {
            None => Err("Kein Pairing angefordert.".to_string()),
            Some(pairing) if Instant::now() >= pairing.expires_at => {
                Err("Der Code ist abgelaufen.".to_string())
            }
            Some(pairing) => {
                if codes_match(&pairing.code, provided.unwrap_or("")) {
                    Ok(())
                } else {
                    Err("Falscher Code.".to_string())
                }
            }
        },
    };

    match outcome {
        Ok(()) => {
            session.attempts.remove(&addr);
            // A pairing code is single-use; leaving it valid would let a second
            // guest reuse a code they saw on screen minutes ago.
            session.pairing = None;
            Ok(())
        }
        Err(message) => {
            let now = Instant::now();
            // Forget addresses that are neither locked out nor still actively
            // guessing, so the table does not grow for the lifetime of a device
            // that runs for months. The window must outlast a burst of wrong
            // codes, or a slow attacker's counter would reset before it trips.
            session.attempts.retain(|_, entry| {
                entry.locked_until.is_some_and(|until| until > now)
                    || now.duration_since(entry.last_seen) < LOCKOUT
            });

            let entry = session.attempts.entry(addr).or_insert(Attempts {
                failures: 0,
                locked_until: None,
                last_seen: now,
            });
            entry.last_seen = now;
            entry.failures += 1;
            if entry.failures >= MAX_CODE_ATTEMPTS {
                entry.failures = 0;
                entry.locked_until = Some(Instant::now() + LOCKOUT);
                warn!("Cast: locking out {} after repeated wrong codes", addr);
            }
            Err(message)
        }
    }
}

// -------------------------------------------------------------- signaling

fn error_frame(code: &str, message: &str) -> Message {
    Message::Text(json!({"type": "error", "code": code, "message": message}).to_string().into())
}

fn ice_servers(state: &AppState) -> serde_json::Value {
    // Empty by default: on a LAN both peers see each other's host candidates, and
    // a STUN server that is unreachable offline only adds a connection delay.
    match state.args.cast_stun_url.as_deref() {
        Some(url) if !url.is_empty() => json!([{ "urls": url }]),
        _ => json!([]),
    }
}

#[derive(Deserialize)]
pub struct CastWsQuery {
    role: Role,
    /// Handed out by `POST /api/cast/claim`. The socket carries no code: auth
    /// happens once, at claim time, so there is a single place to get it wrong.
    #[serde(default)]
    ticket: Option<String>,
}

pub async fn cast_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<CastWsQuery>,
) -> Response {
    if state.args.disable_cast {
        return (StatusCode::NOT_FOUND, "casting is disabled").into_response();
    }

    let addr = peer.ip();

    // The runtime switch is enforced in `authorize_sender` for senders, so they
    // get a readable reason on the socket; the display has nobody to tell.
    if query.role == Role::Display && !state.settings.read().await.cast_enabled {
        return (StatusCode::NOT_FOUND, "casting is disabled").into_response();
    }

    // The cast page is only ever opened by our own Chromium over loopback, so a
    // plain status code is the right answer -- no browser reads this one.
    if query.role == Role::Display && !addr.is_loopback() {
        return (StatusCode::FORBIDDEN, "display role is loopback only").into_response();
    }

    // Sender admission deliberately happens *after* the upgrade. A WebSocket
    // rejected at the HTTP layer gives the page neither status nor body, so the
    // reason would surface as nothing but "connection failed"; refusing over the
    // open socket lets the sender show what actually went wrong.
    let role = query.role;
    let ticket = query.ticket.clone();
    ws.on_upgrade(move |socket| handle_socket(state, role, addr, ticket, socket))
}

async fn handle_socket(
    state: AppState,
    role: Role,
    addr: IpAddr,
    ticket: Option<String>,
    socket: WebSocket,
) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    let writer = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            let closing = matches!(message, Message::Close(_));
            if sink.send(message).await.is_err() {
                break;
            }
            if closing {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let admission: Result<(), Message> = async {
        if role == Role::Sender {
            if let Err(message) = consume_reservation(&state, addr, ticket.as_deref()).await {
                return Err(error_frame("claim", &message));
            }
        }
        if !register_peer(&state, role, addr, tx.clone()).await {
            return Err(error_frame("busy", "Es überträgt bereits jemand."));
        }
        Ok(())
    }
    .await;

    if let Err(frame) = admission {
        let _ = tx.send(frame);
        let _ = tx.send(Message::Close(None));
        drop(tx);
        let _ = writer.await;
        return;
    }

    // Keepalive. Without it a laptop that suspends mid-cast leaves a socket that
    // looks open indefinitely, and the display stays stuck on a frozen frame.
    let ping_tx = tx.clone();
    let pinger = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PING_INTERVAL);
        ticker.tick().await; // the first tick fires immediately
        loop {
            ticker.tick().await;
            if ping_tx.send(Message::Ping(Default::default())).is_err() {
                break;
            }
        }
    });

    loop {
        // Any inbound frame counts as liveness, pongs included.
        match tokio::time::timeout(PEER_IDLE_TIMEOUT, stream.next()).await {
            Err(_) => {
                warn!("Cast: {:?} at {} went silent, dropping", role, addr);
                break;
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => {
                debug!("Cast: socket error from {:?}: {}", role, e);
                break;
            }
            Ok(Some(Ok(Message::Text(text)))) => {
                if !handle_frame(&state, role, text.as_str()).await {
                    break;
                }
            }
            Ok(Some(Ok(Message::Close(_)))) => break,
            Ok(Some(Ok(_))) => {}
        }
    }

    pinger.abort();
    let _ = tx.send(Message::Close(None));
    drop(tx);
    let _ = writer.await;
    unregister_peer(&state, role, addr).await;
}

/// Returns false when the socket should be closed.
async fn handle_frame(state: &AppState, role: Role, text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        debug!("Cast: ignoring non-JSON frame from {:?}", role);
        return true;
    };

    match value.get("type").and_then(|t| t.as_str()) {
        // SDP and ICE are relayed verbatim. The server has no reason to parse
        // WebRTC payloads, and not parsing them means it cannot break them.
        Some("signal") => {
            let session = state.cast.lock().await;
            let target = match role {
                Role::Sender => session.display.as_ref(),
                Role::Display => session.sender.as_ref(),
            };
            match target {
                Some(peer) => {
                    let _ = peer.tx.send(Message::Text(text.to_string().into()));
                }
                None => debug!("Cast: dropping signal from {:?}, no counterpart", role),
            }
            true
        }
        // Only the display may say this: it is a statement about the hardware in
        // front of the guest, and a sender that could set it would be capping (or
        // uncapping) its own stream on the display's behalf.
        Some("limits") if role == Role::Display => {
            let Some(limits) = serde_json::from_value::<DisplayLimits>(value.clone())
                .ok()
                .and_then(DisplayLimits::sane)
            else {
                debug!("Cast: ignoring implausible display limits {}", text);
                return true;
            };

            // The operator's cap wins over what the display reports about itself.
            // Applied here, in the one place the limit passes through, so the
            // relay to the sender and /api/cast/info cannot disagree.
            let limits = match state.args.cast_max_edge {
                Some(cap) if cap < limits.max_edge => {
                    info!(
                        "Cast: display offers {}px, capped to {}px by --cast-max-edge",
                        limits.max_edge, cap
                    );
                    DisplayLimits { max_edge: cap }
                }
                _ => {
                    info!("Cast: display can show frames up to {}px", limits.max_edge);
                    limits
                }
            };

            let sender = {
                let mut session = state.cast.lock().await;
                session.display_limits = Some(limits);
                session.sender.as_ref().map(|peer| peer.tx.clone())
            };
            if let Some(sender) = sender {
                let _ = sender.send(Message::Text(
                    json!({"type": "display_limits", "max_edge": limits.max_edge})
                        .to_string()
                        .into(),
                ));
            }
            true
        }
        Some("stop") => {
            info!("Cast: {:?} asked to stop", role);
            let state = state.clone();
            tokio::spawn(async move { end_session(&state).await });
            false
        }
        other => {
            debug!("Cast: unknown frame type {:?} from {:?}", other, role);
            true
        }
    }
}

/// Trade the claim ticket for the sender slot. Anything unexpected here means
/// the reservation lapsed or belongs to someone else, so the page should send
/// the guest back to the code step.
async fn consume_reservation(
    state: &AppState,
    addr: IpAddr,
    ticket: Option<&str>,
) -> Result<(), String> {
    let mut session = state.cast.lock().await;

    if session.sender.is_some() {
        return Err("Es überträgt bereits jemand.".to_string());
    }
    let Some(provided) = ticket else {
        return Err("Sitzung nicht reserviert.".to_string());
    };
    let matches = session
        .live_reservation()
        .is_some_and(|held| held.addr == addr && codes_match(&held.ticket, provided));
    if !matches {
        // Not cleared on a mismatch: a stray socket must not drop a reservation
        // that legitimately belongs to somebody else.
        return Err("Die Reservierung ist abgelaufen. Bitte neu beginnen.".to_string());
    }

    session.reservation = None;
    Ok(())
}

async fn register_peer(
    state: &AppState,
    role: Role,
    addr: IpAddr,
    tx: mpsc::UnboundedSender<Message>,
) -> bool {
    let (counterpart, welcome) = {
        let mut session = state.cast.lock().await;

        let slot = match role {
            Role::Sender => &mut session.sender,
            Role::Display => &mut session.display,
        };
        if slot.is_some() {
            return false;
        }
        *slot = Some(Peer { tx: tx.clone() });

        if role == Role::Sender {
            session.sender_addr = Some(addr);
        }

        let counterpart = match role {
            Role::Sender => session.display.as_ref().map(|peer| peer.tx.clone()),
            Role::Display => session.sender.as_ref().map(|peer| peer.tx.clone()),
        };

        // The display shows a pairing code that was requested before it finished
        // loading, so it has to learn about it here rather than only on the push.
        let pairing = session.pairing.as_ref().and_then(|pairing| {
            let remaining = pairing.expires_at.saturating_duration_since(Instant::now());
            (!remaining.is_zero()).then(|| json!({
                "code": pairing.code,
                "expires_in": remaining.as_secs(),
            }))
        });

        let welcome = json!({
            "type": "welcome",
            "role": match role { Role::Sender => "sender", Role::Display => "display" },
            "peer": counterpart.is_some(),
            "ice_servers": ice_servers(state),
            "pairing": pairing,
            // Only useful to the sender, and only if a display has ever said so.
            // The sender captures before its socket exists, so this is what makes
            // the *first* frame the right size on every cast after the first.
            "display_limits": match role {
                Role::Sender => session.display_limits,
                Role::Display => None,
            },
        });

        (counterpart, welcome)
    };

    let _ = tx.send(Message::Text(welcome.to_string().into()));
    if let Some(other) = counterpart {
        let _ = other.send(Message::Text(
            json!({"type": "peer", "connected": true}).to_string().into(),
        ));
    }

    info!("Cast: {:?} connected from {}", role, addr);

    // The sender arriving is what puts the cast page on screen. Doing it here
    // rather than at the HTTP upgrade means a sender that fails to establish its
    // socket never interrupts the playlist.
    if role == Role::Sender {
        activate_display(state).await;
    }

    true
}

async fn unregister_peer(state: &AppState, role: Role, addr: IpAddr) {
    let (counterpart, epoch, holding) = {
        let mut session = state.cast.lock().await;
        match role {
            Role::Sender => session.sender = None,
            Role::Display => session.display = None,
        }
        let counterpart = match role {
            Role::Sender => session.display.as_ref().map(|peer| peer.tx.clone()),
            Role::Display => session.sender.as_ref().map(|peer| peer.tx.clone()),
        };
        (counterpart, session.epoch, session.holding_override)
    };

    if let Some(other) = counterpart {
        let _ = other.send(Message::Text(
            json!({"type": "peer", "connected": false}).to_string().into(),
        ));
    }

    info!("Cast: {:?} at {} disconnected", role, addr);

    if role == Role::Sender && holding {
        watch_sender_grace(state.clone(), epoch);
    }
}

// ------------------------------------------------------------------- http

#[derive(Serialize)]
pub struct CastStateResponse {
    /// Effective switch: the stored one, forced off by `--disable-cast`.
    enabled: bool,
    /// `--disable-cast`: off at deployment level, not changeable from the UI.
    hard_disabled: bool,
    auth: CastAuth,
    /// Only meaningful in `code` mode. Reachable from loopback and the operator,
    /// never from `/api/cast/info`.
    code: String,
    active: bool,
    /// Somebody passed the code and is in their browser's screen picker.
    reserved: bool,
    sender: Option<String>,
    display_connected: bool,
    started_at: Option<String>,
    tls_port: u16,
    sender_url: String,
    /// What the display said it can show. Visible here because "the cast is
    /// black" is otherwise very hard to tell from "the cast is not running".
    display_limits: Option<DisplayLimits>,
}

pub async fn cast_state(State(state): State<AppState>) -> impl IntoResponse {
    let sender_url = sender_url(&state);
    let settings = state.settings.read().await;
    let session = state.cast.lock().await;
    Json(CastStateResponse {
        enabled: settings.cast_enabled,
        hard_disabled: state.args.disable_cast,
        auth: settings.cast_auth,
        code: settings.cast_code.clone(),
        active: session.is_active(),
        reserved: session.live_reservation().is_some(),
        sender: session.sender_addr.map(|addr| addr.to_string()),
        display_connected: session.display.is_some(),
        started_at: session.started_at.map(|at| at.to_rfc3339()),
        tls_port: state.cast_tls_port,
        sender_url,
        display_limits: session.display_limits,
    })
}

/// What a sender needs to render its own page, and nothing more.
///
/// Kept separate from `/api/cast/state`, which stays behind operator auth: the
/// sender has no business learning who else is casting or from which address.
pub async fn cast_info(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    let sender_url = sender_url(&state);
    let settings = state.settings.read().await;
    let session = state.cast.lock().await;
    // Never the code itself: this endpoint is reachable without credentials.
    Json(json!({
        "enabled": settings.cast_enabled,
        "auth": settings.cast_auth,
        "busy": session.taken_by_other(peer.ip()),
        // What the display can show. The sender needs this *before* it calls
        // getDisplayMedia, and at that moment it has no socket yet.
        "display_limits": session.display_limits,
        // so a page reached over plain HTTP can send itself to the TLS origin,
        // where getDisplayMedia actually exists
        "sender_url": sender_url,
    }))
}

#[derive(Deserialize)]
pub struct ClaimRequest {
    #[serde(default)]
    code: Option<String>,
}

/// Validate the code and hold the session for this guest.
///
/// Deliberately a separate step from opening the socket: this is what lets the
/// page tell someone their code is wrong *before* the screen picker appears, and
/// what stops two guests from both getting that far.
pub async fn claim_session(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(payload): Json<ClaimRequest>,
) -> Response {
    if state.args.disable_cast {
        return (StatusCode::NOT_FOUND, "casting is disabled").into_response();
    }

    let addr = peer.ip();

    {
        let session = state.cast.lock().await;
        if session.sender.is_some() {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "Es überträgt bereits jemand."})),
            )
                .into_response();
        }
        // Someone else is already in the picker. Let the same address re-claim,
        // so a reload or a second click does not lock a guest out of their own
        // reservation.
        if let Some(held) = session.live_reservation() {
            if held.addr != addr {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({"error": "Jemand anderes bereitet gerade eine Übertragung vor."})),
                )
                    .into_response();
            }
        }
    }

    if let Err(message) = authorize_sender(&state, addr, payload.code.as_deref()).await {
        return (StatusCode::FORBIDDEN, Json(json!({"error": message}))).into_response();
    }

    let ticket = generate_ticket();
    {
        let mut session = state.cast.lock().await;
        session.reservation = Some(Reservation {
            ticket: ticket.clone(),
            addr,
            expires_at: Instant::now() + RESERVATION_TTL,
        });
    }
    info!("Cast: session reserved by {}", addr);

    Json(json!({
        "ticket": ticket,
        "expires_in": RESERVATION_TTL.as_secs(),
    }))
    .into_response()
}

/// Give the slot back without having streamed -- the guest cancelled the picker
/// or closed the tab. Without this the next person waits out the full TTL.
pub async fn release_session(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    let mut session = state.cast.lock().await;
    let mine = session
        .reservation
        .as_ref()
        .is_some_and(|held| held.addr == peer.ip());
    if mine {
        session.reservation = None;
        info!("Cast: reservation released by {}", peer.ip());
    }
    StatusCode::NO_CONTENT
}

/// Only the person currently casting may touch the venue's audio.
///
/// Stricter than the rest of the cast-public routes on purpose: turning the
/// speakers down is a physical act in a shared room, and "anyone who can reach
/// the page" is too wide for it. The address has to match the sender that is
/// actually connected, so the permission ends when the cast does.
async fn caster_only(state: &AppState, peer: IpAddr) -> bool {
    let session = state.cast.lock().await;
    session.sender.is_some() && session.sender_addr == Some(peer)
}

/// Processes whose audio counts as "the cast's own".
async fn cast_process_ids(state: &AppState) -> Vec<u32> {
    let Some(pid) = *state.browser_pid.lock().await else {
        // Someone else started the browser, so we cannot claim a subtree. The
        // panel still works; it just cannot mark one stream as the caster's.
        return Vec::new();
    };
    tokio::task::spawn_blocking(move || crate::audio::descendants(pid))
        .await
        .unwrap_or_default()
}

pub async fn read_audio(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if !caster_only(&state, peer.ip()).await {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "Kein aktiver Cast."}))).into_response();
    }
    let pids = cast_process_ids(&state).await;
    Json(state.audio.state(&pids).await).into_response()
}

pub async fn control_audio(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(command): Json<crate::audio::AudioCommand>,
) -> Response {
    if !caster_only(&state, peer.ip()).await {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "Kein aktiver Cast."}))).into_response();
    }

    let pids = cast_process_ids(&state).await;
    // Needed for a device switch, which has to drag the cast's own stream along
    // or the audio keeps coming out of the old output.
    let cast_stream = state
        .audio
        .state(&pids)
        .await
        .streams
        .iter()
        .find(|stream| stream.is_cast)
        .map(|stream| stream.index);

    if !state.audio.apply(&command, cast_stream).await {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": "Audiosteuerung nicht verfügbar."})),
        )
            .into_response();
    }

    let pids = cast_process_ids(&state).await;
    Json(state.audio.state(&pids).await).into_response()
}

/// Operator override: cut the cast short and put the playlist back.
pub async fn stop_cast(State(state): State<AppState>) -> impl IntoResponse {
    end_session(&state).await;
    StatusCode::NO_CONTENT
}

/// Ask the display to show a fresh pairing code (`--cast-auth=pairing` only).
///
/// The code is deliberately not in the response: proving you can see the screen
/// is the entire point, and returning it would reduce this to "no auth".
pub async fn start_pairing(State(state): State<AppState>) -> Response {
    if state.args.disable_cast {
        return (StatusCode::NOT_FOUND, "casting is disabled").into_response();
    }
    if state.settings.read().await.cast_auth != CastAuth::Pairing {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Pairing ist nicht aktiv."})),
        )
            .into_response();
    }
    if state.cast.lock().await.is_taken() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "Es überträgt bereits jemand."})),
        )
            .into_response();
    }

    let code = generate_code();
    let display_tx = {
        let mut session = state.cast.lock().await;
        session.pairing = Some(Pairing {
            code: code.clone(),
            expires_at: Instant::now() + PAIRING_TTL,
        });
        session.display.as_ref().map(|peer| peer.tx.clone())
    };

    activate_display(&state).await;

    if let Some(tx) = display_tx {
        let _ = tx.send(Message::Text(
            json!({
                "type": "pairing",
                "code": code,
                "expires_in": PAIRING_TTL.as_secs(),
            })
            .to_string()
            .into(),
        ));
    }

    let epoch = state.cast.lock().await.epoch;
    watch_pairing_expiry(state.clone(), epoch);

    Json(json!({"expires_in": PAIRING_TTL.as_secs()})).into_response()
}

pub type SharedCastSession = Arc<tokio::sync::Mutex<CastSession>>;
