use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::models::AppState;

/// Only the person currently casting may touch the venue's audio.
///
/// Stricter than the rest of the cast-public routes on purpose: turning the
/// speakers down is a physical act in a shared room, and "anyone who can reach
/// the page" is too wide for it. The address has to match the sender that is
/// actually connected, so the permission ends when the cast does.
pub(super) async fn caster_only(state: &AppState, peer: IpAddr) -> bool {
    let session = state.cast.lock().await;
    session.sender.is_some() && session.sender_addr == Some(peer)
}

/// Processes whose audio counts as "the cast's own".
pub async fn cast_process_ids(state: &AppState) -> Vec<u32> {
    // The casting display's own browser: the stream to single out belongs to
    // the screen the cast is on, so this resolves it exactly as the two override
    // sites do.
    let Some(pid) = *state.cast_display().browser_pid.lock().await else {
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
