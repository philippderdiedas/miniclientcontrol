use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::ws::Message;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};

use crate::models::{AppState, CastAuth, Display};

use super::{
    activate_display, codes_match, end_session, generate_code, generate_ticket,
    watch_pairing_expiry, Attempts, ClaimMode, DisplayLimits, Pairing, Reservation, Showing,
    LOCKOUT, MAX_CODE_ATTEMPTS, PAIRING_TTL, RESERVATION_TTL,
};
use super::url::sender_url;

/// Checks a sender's code against the configured policy, with a per-address
/// lockout so a four-character code cannot simply be enumerated.
pub(super) async fn authorize_sender(
    state: &AppState,
    display: &Arc<Display>,
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

    // Held across the whole check -- the lockout read, the code comparison and
    // the bookkeeping -- not just the read. Splitting those into separate
    // acquisitions (as this used to) lets two concurrent guessers each read a
    // clean counter before either has recorded a failure, so a burst of size N
    // gets every one of its N wrong codes evaluated instead of being bounded by
    // `MAX_CODE_ATTEMPTS`. Lock order: settings -> cast_attempts -> cast ->
    // override_item. `cast_attempts` is a leaf everywhere else in the tree (only
    // `authorize_sender` touches it), so taking `cast` while holding it here
    // adds one edge and closes no cycle.
    let mut attempts = state.cast_attempts.lock().await;

    if let Some(entry) = attempts.get(&addr) {
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

    // Only `CastAuth::Pairing` reads the session (the code lives on the
    // display, not the lockout table), so it is the only arm that takes
    // `display.cast` -- the busiest mutex in the subsystem, and the one
    // `activate_display`/`deactivate_display` hold across an `override_item`
    // await and `cast_state` takes on every 2-second admin poll. `None` and
    // `Code` are checked against `configured_code` alone and need no session.
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
        CastAuth::Pairing => {
            let mut session = display.cast.lock().await;
            match session.pairing.as_ref() {
                None => Err("Kein Pairing angefordert.".to_string()),
                Some(pairing) if Instant::now() >= pairing.expires_at => {
                    Err("Der Code ist abgelaufen.".to_string())
                }
                Some(pairing) => {
                    if codes_match(&pairing.code, provided.unwrap_or("")) {
                        // A pairing code is single-use; leaving it valid would
                        // let a second guest reuse a code they saw on screen
                        // minutes ago.
                        session.pairing = None;
                        Ok(())
                    } else {
                        Err("Falscher Code.".to_string())
                    }
                }
            }
        }
    };

    match outcome {
        Ok(()) => {
            attempts.remove(&addr);
            Ok(())
        }
        Err(message) => {
            let now = Instant::now();
            // Forget addresses that are neither locked out nor still actively
            // guessing, so the table does not grow for the lifetime of a device
            // that runs for months. The window must outlast a burst of wrong
            // codes, or a slow attacker's counter would reset before it trips.
            attempts.retain(|_, entry| {
                entry.locked_until.is_some_and(|until| until > now)
                    || now.duration_since(entry.last_seen) < LOCKOUT
            });

            let entry = attempts.entry(addr).or_insert(Attempts {
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

#[derive(Deserialize)]
pub struct ScreenQuery {
    /// Which screen to report on. Same rule as every other scoped cast route --
    /// `display::resolve`: a named screen resolves or 404s, an omitted one
    /// resolves while exactly one display is declared and 409s once several are.
    #[serde(default)]
    screen: Option<String>,
}

pub async fn cast_state(
    State(state): State<AppState>,
    Query(query): Query<ScreenQuery>,
) -> Response {
    let sender_url = sender_url(&state);
    // Resolved once per handler and passed on, never resolved again further
    // down: one request must not read one session and write another.
    let display = match crate::display::resolve(&state, query.screen.as_deref()) {
        Ok(display) => display,
        Err(response) => return response,
    };
    let settings = state.settings.read().await;
    let session = display.cast.lock().await;
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
    .into_response()
}

/// What a sender -- or the screen chooser in front of it -- needs, and nothing
/// more.
///
/// Kept separate from `/api/cast/state`, which stays behind operator auth: the
/// sender has no business learning *who* else is casting or from which address.
///
/// Exempt from authentication regardless of address
/// (`cast::is_cast_public_path`): the guest is by definition not loopback, so
/// the screen list below is readable by anyone on the LAN. That is why a
/// disabled feature reports `enabled: false` and carries no `screens` key at
/// all -- a switched-off feature must not enumerate the venue.
pub async fn cast_info(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    let sender_url = sender_url(&state);
    let (cast_enabled, page_enabled, auth) = {
        let settings = state.settings.read().await;
        (
            settings.cast_enabled,
            settings.guest_pages_enabled,
            settings.cast_auth,
        )
    };
    // `--disable-cast` is the deployment-level kill switch; `cast_enabled` is
    // the operator's. Either one hides the screen list.
    let enabled = cast_enabled && !state.args.disable_cast;

    if !enabled {
        return Json(json!({
            "enabled": false,
            "page_enabled": page_enabled,
            "auth": auth,
            "sender_url": sender_url,
        }));
    }

    // The operator's name for each screen, not the internal name it is
    // declared with -- same source `display::list` reads.
    let labels: Vec<(String, Option<String>)> =
        match sqlx::query_as("SELECT name, label FROM displays")
            .fetch_all(&state.pool)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::error!("Failed to read display labels for cast info: {}", e);
                Vec::new()
            }
        };

    // One session's lock at a time, released before the next is taken: lock
    // order is settings -> cast_attempts -> cast -> override_item, and nothing
    // here needs two sessions held together.
    let mut screens = Vec::with_capacity(state.displays.len());
    for display in state.displays.iter() {
        let label = labels
            .iter()
            .find(|(name, _)| name == &display.name)
            .and_then(|(_, label)| label.clone())
            .unwrap_or_else(|| display.name.clone());
        let session = display.cast.lock().await;
        screens.push(json!({
            "name": display.name,
            "label": label,
            // Relative to the asking address, like `claim` -- a guest already
            // holding this screen's reservation must not be told it is busy.
            "busy": session.taken_by_other(peer.ip()),
            // What the display can show. The sender needs this *before* it
            // calls getDisplayMedia, and at that moment it has no socket yet.
            "max_edge": session.display_limits.map(|limits| limits.max_edge),
        }));
    }

    Json(json!({
        "enabled": enabled,
        // Its own switch, not a detail of `enabled`: a device too weak for
        // WebRTC can still render a page, so the guest page shows one control
        // and not the other.
        "page_enabled": page_enabled,
        "auth": auth,
        "screens": screens,
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
    /// Which screen the guest wants. Omitted resolves while exactly one display
    /// is declared and refuses with `409` once several are -- `display::resolve`,
    /// the same rule the playback routes follow.
    #[serde(default)]
    display: Option<String>,
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

    // Checked before `display::resolve`, whose unknown-name path answers with
    // the full list of declared screens (`display::unknown`) -- exactly the
    // enumeration `/api/cast/info` is careful to avoid by omitting `screens`
    // entirely while disabled. Resolving first would let a guest try names
    // until one stops 404ing while the operator switch is off, learning the
    // venue's screens for a feature they cannot use. Neither switch here names
    // a screen, so the refusal says nothing about what exists.
    let enabled = {
        let settings = state.settings.read().await;
        match payload.mode {
            ClaimMode::Cast => settings.cast_enabled,
            ClaimMode::Page => settings.guest_pages_enabled,
        }
    };
    if !enabled {
        let message = match payload.mode {
            ClaimMode::Cast => "Übertragung ist derzeit deaktiviert.",
            ClaimMode::Page => "Webseiten sind derzeit nicht erlaubt.",
        };
        return (StatusCode::FORBIDDEN, Json(json!({"error": message}))).into_response();
    }

    let addr = peer.ip();
    let display = match crate::display::resolve(&state, payload.display.as_deref()) {
        Ok(display) => display,
        Err(response) => return response,
    };

    {
        let session = display.cast.lock().await;
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

    if let Err(message) =
        authorize_sender(&state, &display, addr, payload.code.as_deref(), payload.mode).await
    {
        return (StatusCode::FORBIDDEN, Json(json!({"error": message}))).into_response();
    }

    let ticket = generate_ticket();
    {
        let mut session = display.cast.lock().await;
        session.reservation = Some(Reservation {
            ticket: ticket.clone(),
            addr,
            expires_at: Instant::now() + RESERVATION_TTL,
            mode: payload.mode,
            display: display.name.clone(),
        });
    }
    info!("Cast: session reserved by {}", addr);

    Json(json!({
        "ticket": ticket,
        "expires_in": RESERVATION_TTL.as_secs(),
    }))
    .into_response()
}

#[derive(Deserialize, Default)]
pub struct ReleaseRequest {
    /// Which screen to release. Same rule as `ClaimRequest::display`.
    #[serde(default)]
    display: Option<String>,
}

/// Give the slot back without having streamed -- the guest cancelled the picker
/// or closed the tab. Without this the next person waits out the full TTL.
///
/// TEMPORARY: `web/index.html:315` sends this with no body at all on
/// `pagehide` (`fetch(..., { method: 'DELETE', keepalive: true })`, no
/// `Content-Type`), so the extractor tolerates an absent one exactly like
/// `start_pairing` does, and for the same reason: `Option<Json<ReleaseRequest>>`
/// also yields `None` for a request whose `Content-Type` is not JSON, body and
/// all. Until Task 10 sends `{}` at minimum with the header, a cancel with no
/// screen named goes through `display::resolve(None)` exactly like an unscoped
/// claim: on the single-display deployment every venue runs today it still
/// releases the right (only) screen; on a multi-display one it is refused with
/// `409` rather than releasing the wrong screen, so the reservation the guest
/// meant to cancel is left to expire on its own TTL instead. Once the page
/// always sends a body, this can become a plain `Json<ReleaseRequest>`.
pub async fn release_session(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    payload: Option<Json<ReleaseRequest>>,
) -> Response {
    let display_name = payload.and_then(|Json(p)| p.display);
    let display = match crate::display::resolve(&state, display_name.as_deref()) {
        Ok(display) => display,
        Err(response) => return response,
    };
    let mut session = display.cast.lock().await;
    let mine = session
        .reservation
        .as_ref()
        .is_some_and(|held| held.addr == peer.ip());
    if mine {
        session.reservation = None;
        info!("Cast: reservation released by {}", peer.ip());
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Operator override: cut the cast short and put the playlist back.
pub async fn stop_cast(State(state): State<AppState>) -> impl IntoResponse {
    end_session(&state, &state.primary(), "operator").await;
    StatusCode::NO_CONTENT
}

#[derive(Deserialize, Default)]
pub struct PairRequest {
    /// Which screen the guest wants. Same rule as `ClaimRequest::display`.
    #[serde(default)]
    display: Option<String>,
}

/// Ask the display to show a fresh pairing code (`--cast-auth=pairing` only).
///
/// The code is deliberately not in the response: proving you can see the screen
/// is the entire point, and returning it would reduce this to "no auth".
///
/// TEMPORARY: `web/index.html:664` posts no body today, so the extractor has to
/// tolerate an absent one (`Option<Json<PairRequest>>`) rather than requiring
/// `Json<PairRequest>` outright. The same tolerance also swallows a body with
/// the wrong (or missing) `Content-Type`: under axum 0.8, `Option<Json<T>>`
/// yields `None` for that too, body and all, so a request naming a screen
/// without the JSON header pairs the primary display instead of refusing.
/// Task 10 updates the page to send `{}` at minimum, with the header; once it
/// does, this can become a plain `Json<PairRequest>`, which is what turns a
/// missing or wrong `Content-Type` back into a visible error instead of a
/// silently wrong screen.
pub async fn start_pairing(
    State(state): State<AppState>,
    payload: Option<Json<PairRequest>>,
) -> Response {
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
    let display_name = payload.and_then(|Json(p)| p.display);
    let display = match crate::display::resolve(&state, display_name.as_deref()) {
        Ok(display) => display,
        Err(response) => return response,
    };
    if display.cast.lock().await.is_taken() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "Es überträgt bereits jemand."})),
        )
            .into_response();
    }

    let code = generate_code();
    let display_tx = {
        let mut session = display.cast.lock().await;
        session.pairing = Some(Pairing {
            code: code.clone(),
            expires_at: Instant::now() + PAIRING_TTL,
        });
        session.display.as_ref().map(|peer| peer.tx.clone())
    };

    // The pairing code is drawn by the cast page, so the display has to be
    // pinned to it before anybody is streaming. No sender exists yet, which is
    // also why no `cast.started` comes out of this -- see `register_peer`.
    activate_display(&state, &display, Showing::Cast, None).await;

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

    let epoch = display.cast.lock().await.epoch;
    watch_pairing_expiry(state.clone(), display.clone(), epoch);

    Json(json!({"expires_in": PAIRING_TTL.as_secs()})).into_response()
}
