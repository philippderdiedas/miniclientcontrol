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
//! ## The protocol carries no addressing
//!
//! There are exactly two peers, so a frame needs neither a room name nor a peer
//! id, and the relay forwards SDP and ICE without interpreting them. Not parsing
//! them is also what keeps the server from being able to break them.

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

use url::Url;

use crate::guest_page::redact;
use crate::models::{AppState, CastAuth, OverrideItem, ScrollMode, ScrollOptions};

/// Code alphabet without I/O/0/1, so a code read off a screen cannot be mistyped.
const CODE_CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const CODE_LEN: usize = 4;

/// How long a pairing code stays valid once it is on screen.
const PAIRING_TTL: Duration = Duration::from_secs(30);
/// Grace period after the sender's socket drops, so a page reload does not end
/// the cast and bounce the display back to the playlist for two seconds.
const SENDER_GRACE: Duration = Duration::from_secs(5);
/// Grace for a guest page, rather than the cast's five seconds.
///
/// The expected case here is a phone whose tab was backgrounded, not a page
/// reload. The keepalive itself survives that -- it is a protocol-level ping the
/// browser's network stack answers without waking any JavaScript -- so what this
/// covers is a tab the OS *discarded*, which takes longer to come back.
const PAGE_GRACE: Duration = Duration::from_secs(30);
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
    mode: ClaimMode,
}

/// What a guest said they were going to do, decided at claim time.
///
/// It has to be known this early. `register_peer` activates the display the
/// moment a sender's socket arrives -- deliberately there, so a sender whose
/// socket fails never interrupts the playlist -- and a page-mode sender must not
/// pin `cast_display.html` on its way to the guest's URL. Nor may
/// `watch_display_arrival` start, since a page has no display peer coming.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaimMode {
    #[default]
    Cast,
    Page,
}

/// What the session has put on the display.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Showing {
    #[default]
    Nothing,
    Cast,
    Page {
        /// Full, including any credentials -- the browser needs them. Anything
        /// that logs or displays this goes through `guest_page::redact`.
        url: Url,
        scroll: ScrollMode,
    },
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
    /// What we put there, which decides how teardown checks "is it still ours"
    /// and which grace period applies.
    showing: Showing,
    /// The mode of the reservation the current sender consumed, read by
    /// `register_peer` before the guest has said anything on the socket.
    pending_mode: ClaimMode,
    /// True once this session has told the world a cast started.
    ///
    /// `cast.started` names the moment a sender registers, not the moment the
    /// display is pinned: in the pairing flow the display is pinned first, so
    /// it can draw the code, and nobody is casting yet. Remembering the
    /// announcement is what keeps two other cases honest. A sender whose socket
    /// bounces and returns inside the grace period registers again, and must not
    /// announce a second cast for one session; and a pairing code that expires
    /// unused must not emit a `cast.ended` for a cast that never started.
    cast_announced: bool,
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
    /// What is on screen, for the operator's view.
    ///
    /// The URL is redacted: this is rendered into the admin page and can end up
    /// in a log, and a guest may have typed credentials into it.
    pub fn showing_json(&self) -> serde_json::Value {
        match &self.showing {
            Showing::Nothing => serde_json::Value::Null,
            Showing::Cast => json!("cast"),
            Showing::Page { url, .. } => json!({ "page": redact(url) }),
        }
    }

    pub fn is_active(&self) -> bool {
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
            // The guest page's audio panel: shared with the admin page, so the
            // file has to be reachable without credentials too.
            | "/audio.js"
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
    if state.managed_cert {
        if let Some(url) = crate::tls::managed_base_url(state.cast_tls_port) {
            return url;
        }
    }
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

/// Pin the display to whatever this session is showing, remembering what was
/// there before.
///
/// `sender_ip` is a parameter rather than a read of `session.sender_addr`
/// because a guest replacing their page releases the old one first, and
/// `deactivate_display` clears that field on its way out -- reading it here
/// would announce the second and every later page with no address at all.
async fn activate_display(state: &AppState, showing: Showing, sender_ip: Option<IpAddr>) {
    let mut session = state.cast.lock().await;
    if session.holding_override {
        return;
    }

    let (url, scroll) = match &showing {
        Showing::Cast => (cast_display_url(state.args.port), ScrollMode::None),
        Showing::Page { url, scroll } => (url.to_string(), scroll.clone()),
        Showing::Nothing => return,
    };
    {
        let mut current = state.override_item.lock().await;
        session.previous_override = current.clone();
        *current = Some(OverrideItem {
            asset_id: None,
            url: Some(url),
            local_path: None,
            mimetype: None,
            scroll_config: scroll,
        });
    }
    session.showing = showing.clone();
    session.holding_override = true;
    session.started_at = Some(chrono::Utc::now());
    session.epoch += 1;
    let epoch = session.epoch;
    drop(session);

    state.override_signal.notify_one();

    match &showing {
        Showing::Cast => {
            info!("Cast: display pinned to the cast page");
            // Deliberately no `cast.started` here. This runs when the pairing
            // code appears, which is a display being pinned and not a cast
            // beginning, and by the time a guest types the code the early return
            // above means it never runs a second time. `register_peer` announces
            // the cast instead. `override.set` stays, because pinning the
            // display is exactly what did happen.
            state.webhooks.fire(crate::webhook::Event::OverrideSet {
                url: cast_display_url(state.args.port),
                source: "cast",
            });
            // Only a cast has a display peer to wait for. A page has none, and
            // this watchdog would tear it down after its deadline for a peer
            // that was never coming.
            watch_display_arrival(state.clone(), epoch);
        }
        Showing::Page { url, .. } => {
            info!("Cast: guest page pinned to {}", redact(url));
            state.webhooks.fire(crate::webhook::Event::GuestPageShown {
                url: redact(url),
                sender_ip: sender_ip.map(|addr| addr.to_string()).unwrap_or_default(),
            });
            state.webhooks.fire(crate::webhook::Event::OverrideSet {
                url: redact(url),
                source: "guest_page",
            });
        }
        Showing::Nothing => {}
    }
}

/// Release the display and let the playlist pick up where it left off.
///
/// `reason` is what a webhook receiver is told about why the session ended, so
/// it has to come from the caller: this function cannot tell an operator's stop
/// from a watchdog's timeout from one guest page replacing another.
async fn deactivate_display(state: &AppState, reason: &'static str) {
    let mut session = state.cast.lock().await;
    if !session.holding_override {
        return;
    }

    let ours = match &session.showing {
        Showing::Cast => Some(cast_display_url(state.args.port)),
        Showing::Page { url, .. } => Some(url.to_string()),
        Showing::Nothing => None,
    };
    {
        let mut current = state.override_item.lock().await;
        // Only restore if what is on screen is still the override we installed.
        // An operator who set a different one mid-session made a newer decision,
        // and silently reverting it would look like the UI ignoring them.
        let still_ours = ours.is_some()
            && current.as_ref().and_then(|item| item.url.as_deref()) == ours.as_deref();
        if still_ours {
            *current = session.previous_override.take();
        } else {
            debug!("Cast: override changed during the session, leaving it alone");
        }
    }
    // Captured before the fields are cleared: afterwards this is
    // `Showing::Nothing` with no start time, and the webhook would say nothing.
    let was = session.showing.clone();
    let announced = session.cast_announced;
    let duration_secs = session
        .started_at
        .map(|started| (chrono::Utc::now() - started).num_seconds())
        .unwrap_or(0);

    session.previous_override = None;
    session.holding_override = false;
    session.showing = Showing::Nothing;
    session.started_at = None;
    session.sender_addr = None;
    session.pairing = None;
    session.reservation = None;
    session.cast_announced = false;
    session.epoch += 1;
    drop(session);

    state.override_signal.notify_one();
    info!("Cast: display released, playlist resumes");

    // Both `override.cleared` fires sit outside the `still_ours` branch above,
    // so the event goes out even when the override was left alone because an
    // operator had replaced it mid-session. That is deliberate: the event says
    // this session stopped holding the display, which is true either way, and it
    // is what keeps a session's own `override.set` paired with an
    // `override.cleared` carrying the same `source`. The pairing holds per
    // source and not globally, and a receiver must not assume otherwise:
    // `handlers::set_override` fires a second `override.set{operator}` with no
    // clear in between when an operator replaces one override with another, and
    // `handlers::clear_override` fires `override.cleared{operator}`
    // unconditionally, even with nothing set. An operator clearing a cast's
    // override mid-session therefore produces `set{cast}`,
    // `cleared{operator}`, `cleared{cast}` -- two clears for one set. The
    // asymmetry with `cast.ended` is also deliberate -- that one describes a
    // cast, so it is gated on one having been announced, while
    // `override.cleared` describes the display and is not.
    match was {
        Showing::Cast => {
            if announced {
                state
                    .webhooks
                    .fire(crate::webhook::Event::CastEnded { reason, duration_secs });
            }
            state
                .webhooks
                .fire(crate::webhook::Event::OverrideCleared { source: "cast" });
        }
        Showing::Page { .. } => {
            state
                .webhooks
                .fire(crate::webhook::Event::GuestPageEnded { reason, duration_secs });
            state
                .webhooks
                .fire(crate::webhook::Event::OverrideCleared { source: "guest_page" });
        }
        Showing::Nothing => {}
    }
}

/// Close both sockets and hand the screen back.
///
/// The reason is threaded through rather than decided here, because the callers
/// disagree about it: an operator's stop, a guest's own stop frame, three
/// watchdogs giving up and the cast switch being turned off all end up here.
pub async fn end_session(state: &AppState, reason: &'static str) {
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
    deactivate_display(state, reason).await;
}

// -------------------------------------------------------------- watchdogs

/// The sender's socket went away. Wait out a short grace period before ending the
/// cast, so a page reload does not bounce the display back to the playlist.
fn watch_sender_grace(state: AppState, epoch: u64, grace: Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(grace).await;
        let stale = {
            let session = state.cast.lock().await;
            session.epoch == epoch && session.sender.is_none() && session.holding_override
        };
        if stale {
            info!("Cast: sender did not return, ending session");
            end_session(&state, "grace").await;
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
            end_session(&state, "grace").await;
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
            end_session(&state, "grace").await;
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
    mode: ClaimMode,
) -> Result<(), String> {
    let settings = {
        let settings = state.settings.read().await;
        (
            settings.cast_enabled,
            settings.guest_pages_enabled,
            settings.cast_auth,
            settings.cast_code.clone(),
        )
    };
    let (cast_enabled, pages_enabled, auth_mode, configured_code) = settings;
    // The two capabilities are independent: a device too weak for WebRTC can
    // still render a page, so refusing one must not refuse the other.
    let enabled = match mode {
        ClaimMode::Cast => cast_enabled,
        ClaimMode::Page => pages_enabled,
    };

    let mut session = state.cast.lock().await;

    if let Some(entry) = session.attempts.get(&addr) {
        if let Some(until) = entry.locked_until {
            if Instant::now() < until {
                return Err("Zu viele Fehlversuche. Bitte kurz warten.".to_string());
            }
        }
    }

    if !enabled {
        return Err(match mode {
            ClaimMode::Cast => "Übertragung ist derzeit deaktiviert.".to_string(),
            ClaimMode::Page => "Webseiten sind derzeit nicht erlaubt.".to_string(),
        });
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
        // Only a sender may say this, for the same reason only a display may
        // send `limits`: it is a statement about what the guest wants shown.
        Some("present") if role == Role::Sender => {
            let allowed = state.settings.read().await.guest_pages_enabled;
            let refuse = |reason: &'static str| async move {
                let tx = state.cast.lock().await.sender.as_ref().map(|p| p.tx.clone());
                if let Some(tx) = tx {
                    let _ = tx.send(error_frame("page", reason));
                }
            };
            if !allowed {
                refuse("Webseiten sind derzeit nicht erlaubt.").await;
                return true;
            }

            let raw = value.get("url").and_then(|u| u.as_str()).unwrap_or_default();
            let parsed = match crate::guest_page::parse_guest_url(raw) {
                Ok(parsed) => parsed,
                Err(reason) => {
                    refuse(reason).await;
                    return true;
                }
            };

            let scroll = match value.get("scroll").and_then(|s| s.as_str()) {
                // The operator UI's own defaults, so a guest page behaves like a
                // playlist item rather than like a separate feature.
                Some("slow") => ScrollMode::Continuous(ScrollOptions {
                    speed: 2.0,
                    top_delay: 2000,
                    return_delay: 2000,
                }),
                _ => ScrollMode::None,
            };

            // A second `present` replaces the first rather than being refused:
            // mistyping an address must not cost the guest their slot and a
            // fresh claim. Releasing first keeps `previous_override` pointing at
            // the playlist rather than at the guest's own previous page.
            //
            // The address is read before the release, not after:
            // `deactivate_display` clears `sender_addr`, so a `guest_page.shown`
            // built from the field afterwards would announce the guest's second
            // and every later page with no address at all.
            let sender_ip = state.cast.lock().await.sender_addr;
            deactivate_display(state, "replaced").await;
            activate_display(state, Showing::Page { url: parsed.clone(), scroll }, sender_ip)
                .await;

            let tx = state.cast.lock().await.sender.as_ref().map(|p| p.tx.clone());
            if let Some(tx) = tx {
                let _ = tx.send(Message::Text(
                    json!({"type": "presenting", "url": redact(&parsed)})
                        .to_string()
                        .into(),
                ));
            }
            true
        }
        Some("stop") => {
            info!("Cast: {:?} asked to stop", role);
            let state = state.clone();
            // The arm is role-agnostic on purpose -- either end may hang up --
            // so the reason has to come from `role`. Hard-coding "sender" put
            // the display's own stop in the log and in `cast.ended` under the
            // sender's name, which is a false trail for whoever reads it back.
            let reason = match role {
                Role::Sender => "sender",
                Role::Display => "display",
            };
            tokio::spawn(async move { end_session(&state, reason).await });
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

    // Carried onto the session so `register_peer` knows, before the guest has
    // said anything on the socket, whether to pin the cast page.
    session.pending_mode = session
        .reservation
        .as_ref()
        .map_or(ClaimMode::Cast, |held| held.mode);
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

    // The sender arriving is what puts something on screen. Doing it here rather
    // than at the HTTP upgrade means a sender that fails to establish its socket
    // never interrupts the playlist.
    //
    // A page-mode sender activates nothing yet: it has not said *what* to show.
    // Its `present` frame does that. Pinning the cast page here would make the
    // display visibly bounce through it on the way to the guest's URL.
    if role == Role::Sender {
        // One acquisition for the mode, the flag and the re-stamp together:
        // `activate_display` below takes the same lock, so the guard has to be
        // gone before anything is fired. The start time is re-stamped here
        // rather than left as `activate_display` set it, because in the pairing
        // flow it was stamped when the code appeared and
        // `cast.ended.duration_secs` would otherwise count however long the
        // guest took to type it as time spent casting. Only on the announcing
        // pass, so a socket that bounces mid-cast does not restart the clock.
        let (mode, announce) = {
            let mut session = state.cast.lock().await;
            let mode = session.pending_mode;
            let announce = mode == ClaimMode::Cast && !session.cast_announced;
            if announce {
                session.cast_announced = true;
                session.started_at = Some(chrono::Utc::now());
            }
            (mode, announce)
        };
        // Before `activate_display`, so a receiver reading its log sees the cast
        // start and then the display being pinned. Nothing depends on it --
        // `fire` spawns per event, so delivery order is unordered anyway -- but
        // the state the event describes is already settled here: the sender slot
        // is filled and its address recorded.
        if announce {
            state.webhooks.fire(crate::webhook::Event::CastStarted {
                sender_ip: addr.to_string(),
                mode: "cast".to_string(),
            });
        }
        if mode == ClaimMode::Cast {
            activate_display(state, Showing::Cast, Some(addr)).await;
        }
    }

    true
}

async fn unregister_peer(state: &AppState, role: Role, addr: IpAddr) {
    let (counterpart, epoch, holding, grace) = {
        let mut session = state.cast.lock().await;
        match role {
            Role::Sender => session.sender = None,
            Role::Display => session.display = None,
        }
        let counterpart = match role {
            Role::Sender => session.display.as_ref().map(|peer| peer.tx.clone()),
            Role::Display => session.sender.as_ref().map(|peer| peer.tx.clone()),
        };
        // A page gets a longer leash than a cast: the expected case is a phone
        // whose tab was discarded, not a page reload.
        let grace = match session.showing {
            Showing::Page { .. } => PAGE_GRACE,
            _ => SENDER_GRACE,
        };
        (counterpart, session.epoch, session.holding_override, grace)
    };

    if let Some(other) = counterpart {
        let _ = other.send(Message::Text(
            json!({"type": "peer", "connected": false}).to_string().into(),
        ));
    }

    info!("Cast: {:?} at {} disconnected", role, addr);

    if role == Role::Sender && holding {
        watch_sender_grace(state.clone(), epoch, grace);
    }
}

// ------------------------------------------------------------------- http

#[derive(Serialize)]
pub struct PairingView {
    code: String,
    expires_in: u64,
}

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
    /// What is on screen: `null`, `"cast"`, or `{ "page": "<redacted url>" }`.
    showing: serde_json::Value,
    /// Somebody passed the code and is in their browser's screen picker.
    reserved: bool,
    sender: Option<String>,
    display_connected: bool,
    started_at: Option<String>,
    tls_port: u16,
    sender_url: String,
    /// The pairing code currently on the display, with the seconds it has left.
    ///
    /// Only ever here, never on `/api/cast/info`: this is the operator's view. It
    /// exists because a pairing code is created on request and lives 30 seconds,
    /// so without it whoever is helping a guest by phone is the one person who
    /// cannot see it.
    pairing: Option<PairingView>,
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
        // Redacted inside `showing_json`: a guest may have typed credentials
        // into that address, and this is rendered into the admin page.
        showing: session.showing_json(),
        reserved: session.live_reservation().is_some(),
        sender: session.sender_addr.map(|addr| addr.to_string()),
        display_connected: session.display.is_some(),
        started_at: session.started_at.map(|at| at.to_rfc3339()),
        tls_port: state.cast_tls_port,
        sender_url,
        display_limits: session.display_limits,
        pairing: session.pairing.as_ref().and_then(|pairing| {
            let remaining = pairing.expires_at.saturating_duration_since(Instant::now());
            (!remaining.is_zero()).then(|| PairingView {
                code: pairing.code.clone(),
                expires_in: remaining.as_secs(),
            })
        }),
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
        // Its own switch, not a detail of `enabled`: a device too weak for
        // WebRTC can still render a page, so the guest page shows one control
        // and not the other.
        "page_enabled": settings.guest_pages_enabled,
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
    /// What the guest intends to do. Defaults to `cast`, so an older page that
    /// does not send it behaves exactly as before.
    #[serde(default)]
    mode: ClaimMode,
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

    if let Err(message) = authorize_sender(&state, addr, payload.code.as_deref(), payload.mode).await {
        return (StatusCode::FORBIDDEN, Json(json!({"error": message}))).into_response();
    }

    let ticket = generate_ticket();
    {
        let mut session = state.cast.lock().await;
        session.reservation = Some(Reservation {
            ticket: ticket.clone(),
            addr,
            expires_at: Instant::now() + RESERVATION_TTL,
            mode: payload.mode,
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
pub async fn cast_process_ids(state: &AppState) -> Vec<u32> {
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

/// The part both audio routes share, after each has decided who is allowed in.
///
/// One body on purpose: the guest's panel and the operator's are the same
/// controls, and two copies would drift the moment one gains a feature.
pub async fn apply_audio(state: &AppState, command: crate::audio::AudioCommand) -> Response {
    let pids = cast_process_ids(state).await;
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

    let pids = cast_process_ids(state).await;
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

    apply_audio(&state, command).await
}

/// Operator override: cut the cast short and put the playlist back.
pub async fn stop_cast(State(state): State<AppState>) -> impl IntoResponse {
    end_session(&state, "operator").await;
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

    // The pairing code is drawn by the cast page, so the display has to be
    // pinned to it before anybody is streaming. No sender exists yet, which is
    // also why no `cast.started` comes out of this -- see `register_peer`.
    activate_display(&state, Showing::Cast, None).await;

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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// A listener that answers every request and keeps the bodies it was sent.
    ///
    /// `Dispatcher::fire` spawns, so nothing about an emit site can be asserted
    /// synchronously. Counting deliveries on a real socket is also the only way
    /// "announced exactly once" means once rather than at least once: the
    /// admin-page record the dispatcher keeps holds one result per target and
    /// would look identical after a double fire.
    struct Receiver {
        url: String,
        bodies: Arc<tokio::sync::Mutex<Vec<String>>>,
    }

    impl Receiver {
        async fn count(&self, event: &str) -> usize {
            let needle = format!("\"event\":\"{event}\"");
            self.bodies.lock().await.iter().filter(|b| b.contains(&needle)).count()
        }

        /// Block until `event` has arrived at least `want` times, or give up.
        ///
        /// Used as a barrier before a negative assertion: an event that *should*
        /// come out of the step under test proves the dispatcher got that far,
        /// so a zero count for its sibling is a decision and not a race.
        async fn wait_for(&self, event: &str, want: usize) -> usize {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let seen = self.count(event).await;
                if seen >= want || std::time::Instant::now() >= deadline {
                    return seen;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        /// Long enough for a delivery already in flight to land. Only ever used
        /// after `wait_for` has proven the dispatcher is running.
        async fn settle(&self) {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    async fn receiver() -> Receiver {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let bodies = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let sink = bodies.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let sink = sink.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    // Read the header block, then whatever the declared length
                    // still owes. A single `read` happens to be enough today and
                    // nothing guarantees it stays that way.
                    let header_end = loop {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break pos + 4;
                        }
                    };
                    let length: usize = String::from_utf8_lossy(&buf[..header_end])
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().to_string())
                        })
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    while buf.len() < header_end + length {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let _ = socket
                        .write_all(b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n")
                        .await;
                    let _ = socket.flush().await;
                    sink.lock().await
                        .push(String::from_utf8_lossy(&buf[header_end..]).to_string());
                });
            }
        });
        Receiver { url: format!("http://127.0.0.1:{port}/hook"), bodies }
    }

    /// A state whose only webhook target is `receiver`, subscribed to every
    /// event this module can emit.
    async fn state_for(receiver: &Receiver) -> AppState {
        // `max_connections(1)` rather than a bare `sqlite::memory:`: every test
        // in this module fires webhooks, which deliver from a spawned task
        // holding a second pool connection, and a second connection to a fresh
        // anonymous in-memory database sees no `webhooks` table at all -- the
        // delivery is silently dropped and the failure surfaces as a `wait_for`
        // timeout, not as an error naming the real cause.
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        sqlx::query("INSERT INTO webhooks (name, url, events) VALUES (?, ?, ?)")
            .bind("Counter")
            .bind(&receiver.url)
            .bind(
                r#"["cast.started","cast.ended","override.set","override.cleared",
                    "guest_page.shown","guest_page.ended"]"#,
            )
            .execute(&pool)
            .await
            .unwrap();

        let args = crate::models::Args::parse_from(["miniclientcontrol"]);
        let settings = crate::settings::load(&pool, &args).await;
        AppState {
            pool: pool.clone(),
            args: Arc::new(args),
            skip_signal: Default::default(),
            playlist_signal: Default::default(),
            override_signal: Default::default(),
            overlay_signal: Default::default(),
            current_item_id: Default::default(),
            pending_jump: Default::default(),
            override_item: Default::default(),
            cast_tls_port: 0,
            managed_cert: false,
            settings: Arc::new(tokio::sync::RwLock::new(settings)),
            locks: Default::default(),
            auth_cache: Default::default(),
            audio: Arc::new(crate::audio::Backend::Unavailable),
            browser_pid: Default::default(),
            cast: Default::default(),
            webhooks: Arc::new(crate::webhook::Dispatcher::new(pool)),
        }
    }

    /// Stands in for the pairing request: `start_pairing` mints a code and pins
    /// the display so the cast page can draw it. Everything else it does needs
    /// an HTTP extractor and none of it touches what is under test here.
    async fn pin_for_pairing(state: &AppState) {
        state.cast.lock().await.pairing = Some(Pairing {
            code: "ABCD".to_string(),
            expires_at: Instant::now() + PAIRING_TTL,
        });
        activate_display(state, Showing::Cast, None).await;
    }

    fn sender_socket() -> mpsc::UnboundedSender<Message> {
        // The receiving half is dropped: `register_peer` only ever `let _ =`s its
        // sends, so a closed channel is indistinguishable from a guest who
        // stopped reading.
        mpsc::unbounded_channel().0
    }

    #[tokio::test]
    async fn pairing_pins_the_display_without_announcing_a_cast() {
        let receiver = receiver().await;
        let state = state_for(&receiver).await;

        pin_for_pairing(&state).await;

        {
            let session = state.cast.lock().await;
            assert!(session.holding_override, "the display was not pinned");
            assert!(
                !session.cast_announced,
                "a pairing code appearing is not a cast starting"
            );
        }
        // `override.set` is what this step legitimately emits, so its arrival is
        // the proof that a missing `cast.started` is a decision, not a race.
        assert_eq!(receiver.wait_for("override.set", 1).await, 1);
        assert_eq!(
            receiver.count("cast.started").await,
            0,
            "cast.started went out while the display was only showing a code"
        );
    }

    #[tokio::test]
    async fn the_sender_registering_announces_the_cast_exactly_once() {
        let receiver = receiver().await;
        let state = state_for(&receiver).await;

        pin_for_pairing(&state).await;
        let pinned_at = state.cast.lock().await.started_at.expect("no start time");

        let addr: IpAddr = "192.168.1.44".parse().unwrap();
        assert!(register_peer(&state, Role::Sender, addr, sender_socket()).await);

        {
            let session = state.cast.lock().await;
            assert!(session.cast_announced, "the sender's arrival was not announced");
            let started_at = session.started_at.expect("no start time");
            // Without the re-stamp this would still be the pairing moment, and
            // `cast.ended.duration_secs` would count however long the guest took
            // to type the code as time spent casting.
            assert!(
                started_at > pinned_at,
                "started_at still points at the pairing moment"
            );
        }

        assert_eq!(receiver.wait_for("cast.started", 1).await, 1);
        let body = {
            let bodies = receiver.bodies.lock().await;
            bodies
                .iter()
                .find(|b| b.contains("\"event\":\"cast.started\""))
                .cloned()
                .expect("no cast.started body")
        };
        // The whole point of moving the fire: in `activate_display` there was no
        // sender yet and this field was empty.
        assert!(body.contains("192.168.1.44"), "the sender's address is missing: {body}");
    }

    #[tokio::test]
    async fn a_sender_reconnecting_inside_the_grace_period_announces_nothing_new() {
        let receiver = receiver().await;
        let state = state_for(&receiver).await;

        pin_for_pairing(&state).await;
        let addr: IpAddr = "192.168.1.44".parse().unwrap();
        assert!(register_peer(&state, Role::Sender, addr, sender_socket()).await);
        assert_eq!(receiver.wait_for("cast.started", 1).await, 1);
        let started_at = state.cast.lock().await.started_at.expect("no start time");
        // `activate_display` early-returns once the display is already pinned,
        // so this session's only `override.set` came from `pin_for_pairing`;
        // nothing between here and the bounce below fires another one.
        let overrides_before = receiver.count("override.set").await;

        // The socket bounces. The session survives, because `unregister_peer`
        // hands it to the grace watchdog rather than ending it.
        unregister_peer(&state, Role::Sender, addr).await;
        assert!(
            state.cast.lock().await.holding_override,
            "the grace period should still be holding the display"
        );
        assert!(register_peer(&state, Role::Sender, addr, sender_socket()).await);

        receiver.settle().await;
        assert_eq!(
            receiver.count("cast.started").await,
            1,
            "one session announced two casts"
        );
        // Nothing reads `started_at` after a reconnect, but a stray re-stamp
        // here is exactly the "a sender reconnect resets the cast clock"
        // regression the `if announce` guard exists to prevent.
        assert_eq!(
            state.cast.lock().await.started_at,
            Some(started_at),
            "the reconnect re-stamped the cast's start time"
        );
        // Unlike `cast.started`, which is gated by `cast_announced`, this rests
        // on a completely different guard: `activate_display` early-returns
        // while `holding_override` is true. Checking it catches a regression in
        // that guard even if the `cast_announced` flag were somehow fine.
        assert_eq!(
            receiver.count("override.set").await,
            overrides_before,
            "the reconnect re-pinned the display"
        );
    }

    #[tokio::test]
    async fn an_unused_pairing_code_ends_without_a_cast_ended() {
        let receiver = receiver().await;
        let state = state_for(&receiver).await;

        pin_for_pairing(&state).await;
        assert_eq!(receiver.wait_for("override.set", 1).await, 1);

        // What `watch_pairing_expiry` does when nobody types the code.
        end_session(&state, "grace").await;

        // The display was pinned and is released, so this one is owed.
        assert_eq!(receiver.wait_for("override.cleared", 1).await, 1);
        assert_eq!(
            receiver.count("cast.started").await,
            0,
            "cast.started fired for a code nobody used"
        );
        assert_eq!(
            receiver.count("cast.ended").await,
            0,
            "cast.ended has no cast.started to pair with"
        );
    }

    #[tokio::test]
    async fn a_real_cast_ending_is_paired_and_the_flag_is_cleared() {
        let receiver = receiver().await;
        let state = state_for(&receiver).await;

        pin_for_pairing(&state).await;
        let addr: IpAddr = "192.168.1.44".parse().unwrap();
        assert!(register_peer(&state, Role::Sender, addr, sender_socket()).await);
        assert_eq!(receiver.wait_for("cast.started", 1).await, 1);

        end_session(&state, "operator").await;

        assert_eq!(receiver.wait_for("cast.ended", 1).await, 1);
        assert_eq!(receiver.wait_for("override.cleared", 1).await, 1);
        // Cleared on teardown, or the next session on this process would think it
        // had already announced itself and stay silent.
        assert!(!state.cast.lock().await.cast_announced);
    }

    #[tokio::test]
    async fn a_replacing_page_still_carries_the_guests_address() {
        let receiver = receiver().await;
        let state = state_for(&receiver).await;
        let addr: IpAddr = "192.168.1.44".parse().unwrap();
        let first: Url = "https://dash.example.test/a".parse().unwrap();
        let second: Url = "https://dash.example.test/b".parse().unwrap();

        activate_display(
            &state,
            Showing::Page { url: first, scroll: ScrollMode::None },
            Some(addr),
        )
        .await;
        assert_eq!(receiver.wait_for("guest_page.shown", 1).await, 1);

        // Mirrors `handle_frame`'s "present" branch on a second `present`: the
        // address is read into a local before `deactivate_display` runs, because
        // that call clears `session.sender_addr` in its reset block. Passing the
        // same captured address into the following `activate_display` is the fix
        // under test -- before it, `activate_display` re-read the now-cleared
        // field itself and every replacement page announced no address at all.
        let sender_ip = Some(addr);
        deactivate_display(&state, "replaced").await;
        activate_display(
            &state,
            Showing::Page { url: second, scroll: ScrollMode::None },
            sender_ip,
        )
        .await;

        assert_eq!(receiver.wait_for("guest_page.shown", 2).await, 2);
        let bodies = receiver.bodies.lock().await;
        let shown: Vec<&String> = bodies
            .iter()
            .filter(|b| b.contains("\"event\":\"guest_page.shown\""))
            .collect();
        assert_eq!(shown.len(), 2, "expected one guest_page.shown per present");
        assert!(
            shown.iter().all(|b| b.contains("192.168.1.44")),
            "a present announced the guest with no address: {shown:?}"
        );
    }
}
