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

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::Message;
use axum::routing::{get, post};
use axum::Router;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

// The crate name is shadowed by the `url` submodule declared below, so the
// `url` crate's own `Url` has to be named absolutely.
use ::url::Url;

use crate::guest_page::redact;
use crate::models::{AppState, Display, OverrideItem, ScrollMode};

mod api;
mod room_audio;
mod signaling;
mod url;

pub use api::{cast_info, cast_state, claim_session, release_session, start_pairing, stop_cast};
pub use room_audio::{apply_audio, cast_process_ids, control_audio, read_audio};
pub use signaling::cast_ws;
pub use url::{cast_qr, qr_matrix, sender_url};
use url::cast_display_url;

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

/// Wrong pairing codes seen from one address, and the lockout they earned.
///
/// The one `pub` item in this file that is not part of the session's own
/// surface, and only because `AppState::cast_attempts` names it: a public field
/// cannot have a private type. The lockout counter is controller-wide, not per
/// screen -- per screen it would multiply by the number of displays and hand an
/// attacker N tries at a four-character code instead of one. The fields stay
/// private, which still reaches every submodule of `cast` and nothing else.
pub struct Attempts {
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

/// One screen's cast session. It lives on the `Display` it belongs to, so two
/// guests casting to two screens share no peers, no reservation and no timers.
///
/// `Attempts` is the deliberate exception and sits on `AppState` instead: a
/// per-screen lockout counter would give a guesser one budget per display.
///
/// Every field is private, not `pub(super)`: a `pub(super)` item defined
/// *directly in this file* is scoped to this module's parent, which is the
/// crate root, and `pub(super)` extends to every descendant of that -- the
/// whole binary, not just `src/cast/`. Plain privacy is what actually stays
/// confined here, because a private item defined in `cast`'s own file is
/// visible to `cast` and to its descendant submodules (`api`, `signaling`,
/// `room_audio`, `url`) alike -- the same reach the struct had as a single
/// file, before the split. `is_active` and `showing_json` are the whole
/// outside surface.
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
    /// Last limit a display announced. Deliberately kept when a session ends: it
    /// is a property of the hardware, not of the cast, and remembering it is what
    /// lets the *next* sender constrain its capture before the first frame
    /// instead of showing a black rectangle until the display checks in. Now
    /// that the session hangs off the `Display`, it also cannot be read for the
    /// wrong screen: each panel's GPU limit is stored beside that panel, and the
    /// session outliving it is the `Display` itself.
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

/// Pin the display to whatever this session is showing, remembering what was
/// there before.
///
/// `sender_ip` is a parameter rather than a read of `session.sender_addr`
/// because a guest replacing their page releases the old one first, and
/// `deactivate_display` clears that field on its way out -- reading it here
/// would announce the second and every later page with no address at all.
///
/// `display` is the screen this session belongs to, passed in rather than
/// resolved: the session *is* one of its fields, so resolving a second time here
/// is the one way the pinned override could end up on a different screen than the
/// session that thinks it holds it.
async fn activate_display(
    state: &AppState,
    display: &Arc<Display>,
    showing: Showing,
    sender_ip: Option<IpAddr>,
) {
    // Lock order is settings -> cast -> `Display::override_item`, and the guard
    // below is held across the `override_item` await. Nothing in the tree takes
    // these the other way round.
    let mut session = display.cast.lock().await;
    if session.holding_override {
        return;
    }

    let (url, scroll) = match &showing {
        Showing::Cast => (cast_display_url(state.args.port), ScrollMode::None),
        Showing::Page { url, scroll } => (url.to_string(), scroll.clone()),
        Showing::Nothing => return,
    };
    {
        let mut current = display.override_item.lock().await;
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

    display.override_signal.notify_one();

    match &showing {
        Showing::Cast => {
            info!("Cast: display pinned to the cast page");
            // Deliberately no `cast.started` here. This runs when the pairing
            // code appears, which is a display being pinned and not a cast
            // beginning, and by the time a guest types the code the early return
            // above means it never runs a second time. `register_peer` announces
            // the cast instead. `override.set` stays, because pinning the
            // display is exactly what did happen.
            state.webhooks.fire(&display.name, crate::webhook::Event::OverrideSet {
                url: cast_display_url(state.args.port),
                source: "cast",
            });
            // Only a cast has a display peer to wait for. A page has none, and
            // this watchdog would tear it down after its deadline for a peer
            // that was never coming.
            watch_display_arrival(state.clone(), display.clone(), epoch);
        }
        Showing::Page { url, .. } => {
            info!("Cast: guest page pinned to {}", redact(url));
            state.webhooks.fire(&display.name, crate::webhook::Event::GuestPageShown {
                url: redact(url),
                sender_ip: sender_ip.map(|addr| addr.to_string()).unwrap_or_default(),
            });
            state.webhooks.fire(&display.name, crate::webhook::Event::OverrideSet {
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
///
/// `display` is the same screen `activate_display` was given -- it is the one
/// whose session this is, so the teardown cannot release a different panel than
/// the one that was pinned.
async fn deactivate_display(state: &AppState, display: &Arc<Display>, reason: &'static str) {
    // Same order as `activate_display`: cast, then `override_item`.
    let mut session = display.cast.lock().await;
    if !session.holding_override {
        return;
    }

    let ours = match &session.showing {
        Showing::Cast => Some(cast_display_url(state.args.port)),
        Showing::Page { url, .. } => Some(url.to_string()),
        Showing::Nothing => None,
    };
    {
        let mut current = display.override_item.lock().await;
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

    display.override_signal.notify_one();
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
                    .fire(&display.name, crate::webhook::Event::CastEnded { reason, duration_secs });
            }
            state
                .webhooks
                .fire(&display.name, crate::webhook::Event::OverrideCleared { source: "cast" });
        }
        Showing::Page { .. } => {
            state
                .webhooks
                .fire(&display.name, crate::webhook::Event::GuestPageEnded { reason, duration_secs });
            state
                .webhooks
                .fire(&display.name, crate::webhook::Event::OverrideCleared { source: "guest_page" });
        }
        Showing::Nothing => {}
    }
}

/// Close both sockets and hand the screen back.
///
/// The reason is threaded through rather than decided here, because the callers
/// disagree about it: an operator's stop, a guest's own stop frame, three
/// watchdogs giving up and the cast switch being turned off all end up here.
pub async fn end_session(state: &AppState, display: &Arc<Display>, reason: &'static str) {
    let (sender_peer, display_peer) = {
        let mut session = display.cast.lock().await;
        // Cleared here rather than only in `deactivate_display`, which returns
        // early when no override is held -- precisely the case where someone has
        // reserved the session but not started streaming.
        session.reservation = None;
        (session.sender.take(), session.display.take())
    };
    for peer in [sender_peer, display_peer].into_iter().flatten() {
        let _ = peer.tx.send(Message::Close(None));
    }
    deactivate_display(state, display, reason).await;
}

/// The sender's socket went away. Wait out a short grace period before ending the
/// cast, so a page reload does not bounce the display back to the playlist.
fn watch_sender_grace(state: AppState, display: Arc<Display>, epoch: u64, grace: Duration) {
    // Bound out of the macro's reach: `tracing`'s own `display()` field helper
    // is in scope inside `info!`, so `display.name` there resolves to that
    // function rather than to this screen.
    let name = display.name.clone();
    tokio::spawn(async move {
        tokio::time::sleep(grace).await;
        let stale = {
            let session = display.cast.lock().await;
            session.epoch == epoch && session.sender.is_none() && session.holding_override
        };
        if stale {
            info!("Cast: sender did not return, ending session on '{}'", name);
            end_session(&state, &display, "grace").await;
        }
    });
}

/// The display browser is being navigated to the cast page. If it never connects
/// back, the sender would sit forever on "waiting for display".
fn watch_display_arrival(state: AppState, display: Arc<Display>, epoch: u64) {
    // See `watch_sender_grace`: `display` is shadowed inside the log macros.
    let name = display.name.clone();
    tokio::spawn(async move {
        tokio::time::sleep(DISPLAY_TIMEOUT).await;
        let missing = {
            let session = display.cast.lock().await;
            session.epoch == epoch && session.display.is_none() && session.holding_override
        };
        if missing {
            warn!(
                "Cast: display '{}' never connected back within {:?}",
                name, DISPLAY_TIMEOUT
            );
            let sender_tx = {
                let session = display.cast.lock().await;
                session.sender.as_ref().map(|peer| peer.tx.clone())
            };
            if let Some(tx) = sender_tx {
                let _ = tx.send(error_frame("display", "Das Display hat sich nicht gemeldet."));
            }
            end_session(&state, &display, "grace").await;
        }
    });
}

/// A pairing code nobody used must not hold the screen hostage.
fn watch_pairing_expiry(state: AppState, display: Arc<Display>, epoch: u64) {
    // See `watch_sender_grace`: `display` is shadowed inside the log macros.
    let name = display.name.clone();
    tokio::spawn(async move {
        tokio::time::sleep(PAIRING_TTL).await;
        let unused = {
            let session = display.cast.lock().await;
            session.epoch == epoch && session.sender.is_none() && session.holding_override
        };
        if unused {
            info!("Cast: pairing code on '{}' expired unused", name);
            end_session(&state, &display, "grace").await;
        }
    });
}

fn error_frame(code: &str, message: &str) -> Message {
    Message::Text(json!({"type": "error", "code": code, "message": message}).to_string().into())
}

pub type SharedCastSession = Arc<tokio::sync::Mutex<CastSession>>;

#[cfg(test)]
mod tests {
    use super::*;
    use super::api::authorize_sender;
    use super::signaling::{register_peer, unregister_peer};
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
        state_for_displays(receiver, &["default"]).await
    }

    /// The same state, driving the named screens. Each gets its own session,
    /// because the session is a field of the `Display`.
    async fn state_for_displays(receiver: &Receiver, names: &[&str]) -> AppState {
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

        let mut args = crate::models::Args::parse_from(["miniclientcontrol"]);
        args.display = names.iter().map(|n| n.to_string()).collect();
        let settings = crate::settings::load(&pool, &args).await;
        let displays = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                Arc::new(crate::models::Display::new(
                    name,
                    &format!("http://127.0.0.1:{}", 9222 + index),
                ))
            })
            .collect();
        AppState {
            pool: pool.clone(),
            args: Arc::new(args),
            displays: Arc::new(displays),
            cast_tls_port: 0,
            managed_cert: false,
            settings: Arc::new(tokio::sync::RwLock::new(settings)),
            locks: Default::default(),
            auth_cache: Default::default(),
            audio: Arc::new(crate::audio::Backend::Unavailable),
            cast_attempts: Default::default(),
            webhooks: Arc::new(crate::webhook::Dispatcher::new(pool)),
        }
    }

    /// Stands in for the pairing request: `start_pairing` mints a code and pins
    /// the display so the cast page can draw it. Everything else it does needs
    /// an HTTP extractor and none of it touches what is under test here.
    async fn pin_for_pairing(state: &AppState, display: &Arc<Display>) {
        display.cast.lock().await.pairing = Some(Pairing {
            code: "ABCD".to_string(),
            expires_at: Instant::now() + PAIRING_TTL,
        });
        activate_display(state, display, Showing::Cast, None).await;
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
        let display = state.primary();

        pin_for_pairing(&state, &display).await;

        {
            let session = display.cast.lock().await;
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

        let display = state.primary();
        pin_for_pairing(&state, &display).await;
        let pinned_at = display.cast.lock().await.started_at.expect("no start time");

        let addr: IpAddr = "192.168.1.44".parse().unwrap();
        assert!(register_peer(&state, &display, Role::Sender, addr, sender_socket()).await);

        {
            let session = display.cast.lock().await;
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

    /// Two declared screens, one activated. The other must be untouched -- the
    /// session is a field of the `Display`, so there is nothing for the two to
    /// share and nothing for one to clobber in the other.
    #[tokio::test]
    async fn two_screens_hold_independent_sessions() {
        let receiver = receiver().await;
        let state = state_for_displays(&receiver, &["foyer", "werkstatt"]).await;
        let foyer = state.display("foyer").unwrap();
        let werkstatt = state.display("werkstatt").unwrap();

        // The *second* declared screen on purpose. Activating the first would
        // pass just as well against an `activate_display` that ignored its
        // argument and resolved `displays[0]` itself, which is exactly the
        // regression this test is here to catch.
        activate_display(
            &state,
            &werkstatt,
            Showing::Cast,
            Some("10.0.0.5".parse().unwrap()),
        )
        .await;

        assert!(
            werkstatt.cast.lock().await.is_active(),
            "the screen that was activated"
        );
        assert!(
            !foyer.cast.lock().await.is_active(),
            "the other screen must be untouched -- one session per display is the whole feature"
        );
        assert!(werkstatt.override_item.lock().await.is_some());
        assert!(
            foyer.override_item.lock().await.is_none(),
            "activating one screen must not pin an override on another"
        );
    }

    #[tokio::test]
    async fn a_sender_reconnecting_inside_the_grace_period_announces_nothing_new() {
        let receiver = receiver().await;
        let state = state_for(&receiver).await;

        let display = state.primary();
        pin_for_pairing(&state, &display).await;
        let addr: IpAddr = "192.168.1.44".parse().unwrap();
        assert!(register_peer(&state, &display, Role::Sender, addr, sender_socket()).await);
        assert_eq!(receiver.wait_for("cast.started", 1).await, 1);
        let started_at = display.cast.lock().await.started_at.expect("no start time");
        // `activate_display` early-returns once the display is already pinned,
        // so this session's only `override.set` came from `pin_for_pairing`;
        // nothing between here and the bounce below fires another one.
        let overrides_before = receiver.count("override.set").await;

        // The socket bounces. The session survives, because `unregister_peer`
        // hands it to the grace watchdog rather than ending it.
        unregister_peer(&state, &display, Role::Sender, addr).await;
        assert!(
            display.cast.lock().await.holding_override,
            "the grace period should still be holding the display"
        );
        assert!(register_peer(&state, &display, Role::Sender, addr, sender_socket()).await);

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
            display.cast.lock().await.started_at,
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

        let display = state.primary();
        pin_for_pairing(&state, &display).await;
        assert_eq!(receiver.wait_for("override.set", 1).await, 1);

        // What `watch_pairing_expiry` does when nobody types the code.
        end_session(&state, &display, "grace").await;

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

        let display = state.primary();
        pin_for_pairing(&state, &display).await;
        let addr: IpAddr = "192.168.1.44".parse().unwrap();
        assert!(register_peer(&state, &display, Role::Sender, addr, sender_socket()).await);
        assert_eq!(receiver.wait_for("cast.started", 1).await, 1);

        end_session(&state, &display, "operator").await;

        assert_eq!(receiver.wait_for("cast.ended", 1).await, 1);
        assert_eq!(receiver.wait_for("override.cleared", 1).await, 1);
        // Cleared on teardown, or the next session on this process would think it
        // had already announced itself and stay silent.
        assert!(!display.cast.lock().await.cast_announced);
    }

    #[tokio::test]
    async fn a_replacing_page_still_carries_the_guests_address() {
        let receiver = receiver().await;
        let state = state_for(&receiver).await;
        let display = state.primary();
        let addr: IpAddr = "192.168.1.44".parse().unwrap();
        let first: Url = "https://dash.example.test/a".parse().unwrap();
        let second: Url = "https://dash.example.test/b".parse().unwrap();

        activate_display(
            &state,
            &display,
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
        deactivate_display(&state, &display, "replaced").await;
        activate_display(
            &state,
            &display,
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

    /// A concurrent burst of wrong codes from one address must not evaluate
    /// more of them than the lockout is supposed to tolerate.
    ///
    /// `authorize_sender` used to read `cast_attempts`, drop that guard, and
    /// only then acquire `display.cast` -- so a task that read a clean counter
    /// could be queued behind another that had done the same, and both would go
    /// on to be evaluated before either recorded a failure. This needs a real
    /// multi-worker runtime: on a single-threaded one every `.await` in
    /// `authorize_sender` is a cooperative yield point at a *fixed* place, and
    /// this interleaving depends on genuinely concurrent lock acquisition.
    ///
    /// Parameterised over both modes that can produce "Falscher Code.":
    /// `Code` is checked against `configured_code` alone and never touches
    /// `display.cast`, while `Pairing` is the only arm that acquires it on top
    /// of `cast_attempts` -- the nested acquisition the fix was actually
    /// about. Covering only `Code` would pass even if that nesting still
    /// raced.
    async fn assert_concurrent_wrong_codes_are_bounded(mode: crate::models::CastAuth) {
        let receiver = receiver().await;
        let state = state_for(&receiver).await;
        let display = state.primary();

        match mode {
            crate::models::CastAuth::Code => {
                let mut settings = state.settings.write().await;
                settings.cast_auth = crate::models::CastAuth::Code;
                settings.cast_code = "ABCD".to_string();
            }
            crate::models::CastAuth::Pairing => {
                {
                    let mut settings = state.settings.write().await;
                    settings.cast_auth = crate::models::CastAuth::Pairing;
                }
                // Mints a real pairing code and pins the display, exactly as a
                // pairing request would -- the wrong guesses below must land on
                // the `Pairing` arm's `display.cast.lock()`, not on a session
                // with nothing to check against.
                pin_for_pairing(&state, &display).await;
            }
            crate::models::CastAuth::None => unreachable!("not exercised here"),
        }

        let addr: IpAddr = "192.168.1.99".parse().unwrap();

        let mut handles = Vec::new();
        for _ in 0..200 {
            let state = state.clone();
            let display = display.clone();
            handles.push(tokio::spawn(async move {
                authorize_sender(&state, &display, addr, Some("ZZZZ"), ClaimMode::Cast).await
            }));
        }

        let mut evaluated = 0;
        for handle in handles {
            if let Err(message) = handle.await.unwrap() {
                if message == "Falscher Code." {
                    evaluated += 1;
                }
            }
        }

        assert!(
            evaluated <= MAX_CODE_ATTEMPTS as usize,
            "a concurrent burst of 200 wrong codes evaluated {evaluated}, more than \
             MAX_CODE_ATTEMPTS ({MAX_CODE_ATTEMPTS}) -- the lockout no longer bounds a \
             concurrent guesser"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_wrong_codes_are_bounded_by_the_lockout_code() {
        assert_concurrent_wrong_codes_are_bounded(crate::models::CastAuth::Code).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_wrong_codes_are_bounded_by_the_lockout_pairing() {
        assert_concurrent_wrong_codes_are_bounded(crate::models::CastAuth::Pairing).await;
    }
}
