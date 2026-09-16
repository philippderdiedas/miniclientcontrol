use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::models::{AppState, Display};

/// Which screen a guest's audio request is about. Same rule as every other
/// scoped cast route (`display::resolve`): a named screen resolves or 404s, an
/// omitted one resolves while exactly one display is declared and 409s once
/// several are declared.
#[derive(Deserialize)]
pub struct AudioQuery {
    #[serde(default)]
    screen: Option<String>,
}

/// Only the person currently casting *to this screen* may touch the venue's
/// audio through it.
///
/// Stricter than the rest of the cast-public routes on purpose: turning the
/// speakers down is a physical act in a shared room, and "anyone who can reach
/// the page" is too wide for it. The address has to match the sender actually
/// connected to `display`, so the permission ends when that cast does -- and a
/// guest casting to one screen cannot use its address to reach into another's
/// session, which is the only thing naming a screen here is for. The room
/// audio itself is a separate, shared resource (`AppState::audio_owner`); this
/// only decides who may ask to touch it at all.
pub(super) async fn caster_only(display: &Arc<Display>, peer: IpAddr) -> bool {
    let session = display.cast.lock().await;
    session.sender.is_some() && session.sender_addr == Some(peer)
}

/// The operator's label for a screen, falling back to its internal name --
/// same source `cast_info`'s screen list reads. Used only to name the current
/// owner in a refusal; the internal name never reaches a guest.
async fn display_label(state: &AppState, name: &str) -> String {
    sqlx::query_scalar::<_, Option<String>>("SELECT label FROM displays WHERE name = ?1")
        .bind(name)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
        .flatten()
        .unwrap_or_else(|| name.to_string())
}

/// The screen currently holding the room audio, if any -- and the
/// self-healing half of the contract on `AppState::audio_owner`.
///
/// Read and validated together: a cast that ended without a clean teardown
/// (a crash, a killed browser) leaves the field set with nothing to release
/// it, and checking `is_active()` on every read is what frees the room again
/// on its own instead of leaving it mute until a restart.
///
/// Deliberately two separate short-lived locks rather than one held across
/// both: `audio_owner` is a leaf everywhere in this module (see
/// `deactivate_display`'s comment in `cast::mod`), so the owning display's
/// `cast` session is checked *after* `audio_owner` has already been dropped,
/// never while it is held.
async fn owning_display(state: &AppState) -> Option<String> {
    let held = state.audio_owner.lock().await.clone()?;
    // A name no longer declared cannot be active either -- treated the same as
    // a session that ended.
    let active = match state.display(&held) {
        Some(display) => display.cast.lock().await.is_active(),
        None => false,
    };
    if active {
        Some(held)
    } else {
        *state.audio_owner.lock().await = None;
        None
    }
}

/// Claim the room audio for `display`, or refuse if another screen already
/// holds it.
///
/// Guest-only: this is what stands between "a guest turns the room's sound on"
/// and "a guest takes the room's sound away from a stranger who is mid
/// presentation" -- the second is not a guest's call, so this is never on the
/// path from `/api/audio`, which reaches `apply_audio` directly under the
/// operator's own credentials instead.
async fn claim_audio(state: &AppState, display: &Arc<Display>) -> Result<(), Response> {
    match owning_display(state).await {
        Some(name) if name == display.name => Ok(()),
        Some(name) => {
            let label = display_label(state, &name).await;
            Err((
                StatusCode::CONFLICT,
                Json(json!({"error": format!("Ton läuft gerade auf „{label}“.")})),
            )
                .into_response())
        }
        None => {
            *state.audio_owner.lock().await = Some(display.name.clone());
            Ok(())
        }
    }
}

/// Processes whose audio counts as "the cast's own".
///
/// The browser to single out is the one driving the screen the cast is on, so
/// the display is passed in rather than resolved here -- the same `Display` the
/// caller checked `caster_only` against.
pub async fn cast_process_ids(display: &Arc<Display>) -> Vec<u32> {
    let Some(pid) = *display.browser_pid.lock().await else {
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
    Query(query): Query<AudioQuery>,
) -> Response {
    let display = match crate::display::resolve(&state, query.screen.as_deref()) {
        Ok(display) => display,
        Err(response) => return response,
    };
    if !caster_only(&display, peer.ip()).await {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "Kein aktiver Cast."}))).into_response();
    }
    let pids = cast_process_ids(&display).await;
    Json(state.audio.state(&pids).await).into_response()
}

/// The part both audio routes share, after each has decided who is allowed in.
///
/// One body on purpose: the guest's panel and the operator's are the same
/// controls, and two copies would drift the moment one gains a feature.
pub async fn apply_audio(
    state: &AppState,
    display: &Arc<Display>,
    command: crate::audio::AudioCommand,
) -> Response {
    let pids = cast_process_ids(display).await;
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

    let pids = cast_process_ids(display).await;
    Json(state.audio.state(&pids).await).into_response()
}

pub async fn control_audio(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<AudioQuery>,
    Json(command): Json<crate::audio::AudioCommand>,
) -> Response {
    let display = match crate::display::resolve(&state, query.screen.as_deref()) {
        Ok(display) => display,
        Err(response) => return response,
    };
    if !caster_only(&display, peer.ip()).await {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "Kein aktiver Cast."}))).into_response();
    }

    // A guest may turn the room's own audio on, but never take it from another
    // guest's cast -- that is a stranger silencing someone mid-presentation.
    // The operator's own route (`/api/audio`) skips this and reaches
    // `apply_audio` directly, because the operator is not a guest and needs no
    // cast running at all.
    if let Err(response) = claim_audio(&state, &display).await {
        return response;
    }

    apply_audio(&state, &display, command).await
}
