use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::guest_page::redact;
use crate::models::{AppState, Display, ScrollMode, ScrollOptions};

use super::{
    activate_display, codes_match, deactivate_display, end_session, error_frame,
    watch_sender_grace, ClaimMode, DisplayLimits, Peer, Role, Showing, PAGE_GRACE,
    PEER_IDLE_TIMEOUT, PING_INTERVAL, SENDER_GRACE,
};

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
    /// Which screen this connection is for. Only ever set by the display role:
    /// the display role is loopback-only, so this is our own page saying which
    /// screen it is. A sender never sends this -- its ticket already names the
    /// screen it was issued for, and trusting a query string instead would let
    /// a sender claim a screen its ticket was never issued for.
    #[serde(default)]
    screen: Option<String>,
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
    // Resolved once, here, and carried for the life of the socket: a socket may
    // only ever act on the session it was admitted to, so nothing below this
    // line resolves a screen a second time.
    let display = match role {
        Role::Display => match crate::display::resolve(&state, query.screen.as_deref()) {
            Ok(display) => display,
            Err(response) => return response,
        },
        // No query parameter to trust here -- find whichever display's session
        // is currently holding a live reservation for this exact ticket.
        // `consume_reservation` still re-checks address and liveness before
        // admitting it; this only decides which session to check them against.
        //
        // A missing ticket names no screen at all, so it falls back to the
        // primary display purely to have a session to open against --
        // `consume_reservation` refuses it there on the ticket being absent, a
        // refusal that says the same thing regardless of which display answered.
        //
        // A ticket that *was* provided but is held by no session (expired,
        // guessed, or for a screen since removed) is refused right here, before
        // any session is touched. Falling back to the primary display for that
        // case, as this used to, would ask *its* session for a reason, and its
        // busy/free state has nothing to do with this ticket -- an unrelated
        // cast already running on the primary display would then read as "your
        // reservation is fine, someone else is casting" when the truth is the
        // guest's own reservation lapsed.
        Role::Sender => match ticket.as_deref() {
            None => state.primary(),
            Some(_) => match display_for_ticket(&state, ticket.as_deref()).await {
                Some(display) => display,
                None => return ws.on_upgrade(refuse_unknown_ticket),
            },
        },
    };
    ws.on_upgrade(move |socket| handle_socket(state, display, role, addr, ticket, socket))
}

/// The display whose session holds a live reservation for this ticket, if any.
///
/// `codes_match`, not `==`: this scan runs first, over the same secret
/// `consume_reservation` compares with `codes_match` a moment later, so a plain
/// `==` here would let a wrong ticket be found one character at a time by
/// timing this call alone -- the later, constant-time comparison would never
/// even run.
async fn display_for_ticket(state: &AppState, ticket: Option<&str>) -> Option<Arc<Display>> {
    let ticket = ticket?;
    for display in state.displays.iter() {
        let session = display.cast.lock().await;
        if session.live_reservation().is_some_and(|held| codes_match(&held.ticket, ticket)) {
            return Some(display.clone());
        }
    }
    None
}

/// Refuse a sender whose ticket is held by no session at all, over the socket
/// and without ever locking a display's session -- see the comment in
/// `cast_ws` for why picking one to check against would be misleading.
async fn refuse_unknown_ticket(socket: WebSocket) {
    let (mut sink, _stream) = socket.split();
    let frame = error_frame("claim", "Die Reservierung ist abgelaufen. Bitte neu beginnen.");
    let _ = sink.send(frame).await;
    let _ = sink.send(Message::Close(None)).await;
    let _ = sink.close().await;
}

async fn handle_socket(
    state: AppState,
    display: Arc<Display>,
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
            if let Err(message) = consume_reservation(&display, addr, ticket.as_deref()).await {
                return Err(error_frame("claim", &message));
            }
        }
        if !register_peer(&state, &display, role, addr, tx.clone()).await {
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
                if !handle_frame(&state, &display, role, text.as_str()).await {
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
    unregister_peer(&state, &display, role, addr).await;
}

/// Returns false when the socket should be closed.
async fn handle_frame(state: &AppState, display: &Arc<Display>, role: Role, text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        debug!("Cast: ignoring non-JSON frame from {:?}", role);
        return true;
    };

    match value.get("type").and_then(|t| t.as_str()) {
        // SDP and ICE are relayed verbatim. The server has no reason to parse
        // WebRTC payloads, and not parsing them means it cannot break them.
        Some("signal") => {
            let session = display.cast.lock().await;
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
                let mut session = display.cast.lock().await;
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
                let tx = display.cast.lock().await.sender.as_ref().map(|p| p.tx.clone());
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
            let sender_ip = display.cast.lock().await.sender_addr;
            deactivate_display(state, display, "replaced").await;
            activate_display(
                state,
                display,
                Showing::Page { url: parsed.clone(), scroll },
                sender_ip,
            )
            .await;

            let tx = display.cast.lock().await.sender.as_ref().map(|p| p.tx.clone());
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
            let display = display.clone();
            // The arm is role-agnostic on purpose -- either end may hang up --
            // so the reason has to come from `role`. Hard-coding "sender" put
            // the display's own stop in the log and in `cast.ended` under the
            // sender's name, which is a false trail for whoever reads it back.
            let reason = match role {
                Role::Sender => "sender",
                Role::Display => "display",
            };
            tokio::spawn(async move { end_session(&state, &display, reason).await });
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
    display: &Arc<Display>,
    addr: IpAddr,
    ticket: Option<&str>,
) -> Result<(), String> {
    let mut session = display.cast.lock().await;

    if session.sender.is_some() {
        return Err("Es überträgt bereits jemand.".to_string());
    }
    let Some(provided) = ticket else {
        return Err("Sitzung nicht reserviert.".to_string());
    };
    let held = session
        .live_reservation()
        .filter(|held| held.addr == addr && codes_match(&held.ticket, provided));
    let Some(held) = held else {
        // Not cleared on a mismatch: a stray socket must not drop a reservation
        // that legitimately belongs to somebody else.
        return Err("Die Reservierung ist abgelaufen. Bitte neu beginnen.".to_string());
    };
    // Not belt-and-braces in the usual sense -- there is no second, independent
    // fact this checks against. `cast_ws` already resolved `display` to be the
    // exact session holding this ticket (or refused before the socket ever
    // reached here), and `Reservation::display` is stamped, once, from the name
    // of the very session it is stored into (`claim_session`, the only
    // construction site) -- so `held.display == display.name` on every path
    // that gets this far. The guard is here so that if a future change ever let
    // a `Reservation` move between sessions, that bug would fail loudly here
    // rather than quietly admitting a sender to a screen its ticket was never
    // issued for.
    if held.display != display.name {
        return Err("Dieses Ticket gehört zu einem anderen Bildschirm.".to_string());
    }

    // Taken before `session` is borrowed mutably below -- `held` already proved
    // `session.reservation` is `Some`, so re-deriving the mode from it again via
    // `map_or` (as this used to) was reaching for a fallback that could never be
    // taken.
    let mode = held.mode;

    // Carried onto the session so `register_peer` knows, before the guest has
    // said anything on the socket, whether to pin the cast page.
    session.pending_mode = mode;
    session.reservation = None;
    Ok(())
}

pub(super) async fn register_peer(
    state: &AppState,
    display: &Arc<Display>,
    role: Role,
    addr: IpAddr,
    tx: mpsc::UnboundedSender<Message>,
) -> bool {
    let (counterpart, welcome) = {
        let mut session = display.cast.lock().await;

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
            let mut session = display.cast.lock().await;
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
            // The screen this sender was admitted to, which is also the one
            // `activate_display` pins below -- they are the same `Display`, so a
            // receiver cannot be told about a screen the cast is not on.
            state.webhooks.fire(&display.name, crate::webhook::Event::CastStarted {
                sender_ip: addr.to_string(),
                mode: "cast".to_string(),
            });
        }
        if mode == ClaimMode::Cast {
            activate_display(state, display, Showing::Cast, Some(addr)).await;
        }
    }

    true
}

pub(super) async fn unregister_peer(
    state: &AppState,
    display: &Arc<Display>,
    role: Role,
    addr: IpAddr,
) {
    let (counterpart, epoch, holding, grace) = {
        let mut session = display.cast.lock().await;
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
        watch_sender_grace(state.clone(), display.clone(), epoch, grace);
    }
}
