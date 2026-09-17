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
/// Two independent ways the recorded owner can point at a display that no
/// longer holds anything, neither of which any code path releases directly:
///
/// - The display was undeclared from the configuration between the claim and
///   this read, so `state.display(&held)` returns `None`.
/// - The claiming session's own socket dropped before it was ever activated.
///   `caster_only` only requires a registered sender at a matching address --
///   a page-mode guest satisfies that, and so can claim the room audio, the
///   moment its socket registers, well before it says what to show. If that
///   socket drops in that window, `unregister_peer` clears `sender` while
///   `holding_override` is still `false`, so `deactivate_display` never runs
///   (it no-ops unless `holding_override` is set) and never had anything to
///   release.
///
/// Checking `is_active()` on every read is what frees the room again in
/// either case, instead of leaving it mute until a restart. It is *not* what
/// frees a cast that crashes or a browser that is killed: those never touch
/// `CastSession` at all, so `is_active()` would still read true -- see the
/// GPU-wedge case in `CLAUDE.md`.
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
        // Compare-and-clear, not an unconditional write: between the read above
        // and this reacquisition, a different screen's `claim_audio` can have
        // already self-healed the same staleness and claimed the room for
        // itself. Clearing unconditionally here would erase that newer claim
        // instead of the stale one this call observed. Same shape as
        // `deactivate_display` in `cast::mod`.
        let mut owner = state.audio_owner.lock().await;
        if owner.as_deref() == Some(held.as_str()) {
            *owner = None;
        }
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
///
/// The read-or-claim has to be one critical section, not two. `owning_display`
/// above only ever *releases* the lock it takes, so running it first is safe
/// -- but the decision "is it free, and if so mine" must happen under a single
/// `audio_owner` acquisition, or two concurrent callers for two different
/// screens can both observe it free and both write, the second silently
/// clobbering the first with no `409` ever shown to either guest. `audio_owner`
/// stays a leaf: nothing else is locked while it is held, here or anywhere else
/// in this module.
async fn claim_audio(state: &AppState, display: &Arc<Display>) -> Result<(), Response> {
    // Self-heal first, in its own short-lived lock scope: it can only clear a
    // stale owner, never set one, so nothing below can race against it in a
    // way that matters -- the atomic check-and-write is what actually decides.
    owning_display(state).await;

    let mut owner = state.audio_owner.lock().await;
    match owner.clone() {
        Some(name) if name == display.name => Ok(()),
        Some(name) => {
            // Nothing further is written on this path, so the lock can be
            // dropped before the (awaiting) label lookup.
            drop(owner);
            let label = display_label(state, &name).await;
            Err((
                StatusCode::CONFLICT,
                Json(json!({"error": format!("Ton läuft gerade auf „{label}“.")})),
            )
                .into_response())
        }
        None => {
            *owner = Some(display.name.clone());
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// A minimal multi-display state, with no webhook receiver.
    ///
    /// Nothing under test here fires a webhook directly (only
    /// `activate_display`/`deactivate_display` in `cast::mod` do that), but
    /// `max_connections(1)` is still load-bearing exactly as it is in
    /// `cast::mod`'s own `state_for_displays`: a second connection to a fresh
    /// anonymous `sqlite::memory:` database sees no schema at all, and nothing
    /// here needs more than one connection at a time.
    async fn state_for_displays(names: &[&str]) -> AppState {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::run_migrations(&pool).await.unwrap();

        let mut args = crate::models::Args::parse_from(["miniclientcontrol"]);
        args.display = names.iter().map(|n| n.to_string()).collect();
        let settings = crate::settings::load(&pool, &args).await;
        let displays = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                Arc::new(Display::new(name, &format!("http://127.0.0.1:{}", 9222 + index)))
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
            audio_owner: Default::default(),
            cast_attempts: Default::default(),
            webhooks: Arc::new(crate::webhook::Dispatcher::new(pool)),
        }
    }

    /// The regression this exists to catch: before the fix, `claim_audio`'s
    /// `None` arm read `owning_display` (one lock, released) and then
    /// unconditionally wrote the owner under a second, independent lock, with
    /// no re-check in between. Many callers racing that gap could all observe
    /// "free" and all win.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_claims_for_different_screens_admit_exactly_one() {
        const N: usize = 32;
        // The race window (between `owning_display` releasing its lock and
        // `claim_audio`'s old second, independent acquisition) is narrow, so
        // one round is not reliable evidence either way. Repeating the whole
        // race many times against a fresh `audio_owner` makes a single-round
        // near-miss irrelevant: it only takes one round out of many to prove
        // the property false.
        const ROUNDS: usize = 25;

        let names: Vec<String> = (0..N).map(|i| format!("screen{i}")).collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let state = state_for_displays(&name_refs).await;

        // Every screen is an active cast, or `owning_display`'s self-heal
        // would legitimately treat a screen's own just-won claim as stale the
        // moment a *different* screen's task re-reads it -- self-heal only
        // ever knows `is_active()`, not "did someone else just win this" --
        // and clear it back to `None`. That is a separate, correct behaviour
        // of this module (a real cast always has `is_active()` true by the
        // time it can claim audio at all; these bare test displays do not),
        // and it would mask the winner count here, so every screen is made
        // active to take it out of play. It is not the check-then-act race
        // this test exists to catch; that race is instead covered below by
        // `stale_recorded_owner_cannot_be_cleared_out_from_under_a_winner`,
        // which races a *stale, inactive* recorded owner against winners
        // that stay active throughout.
        for name in &names {
            state.display(name).unwrap().cast.lock().await.holding_override = true;
        }

        for round in 0..ROUNDS {
            *state.audio_owner.lock().await = None;

            let mut handles = Vec::new();
            for name in &names {
                let state = state.clone();
                let display = state.display(name).unwrap();
                handles.push(tokio::spawn(async move { claim_audio(&state, &display).await }));
            }

            let mut winners = 0;
            for handle in handles {
                if handle.await.unwrap().is_ok() {
                    winners += 1;
                }
            }

            assert_eq!(
                winners, 1,
                "round {round}: exactly one of {N} concurrent claims for different screens \
                 should win the room audio -- more than one means the check-then-act race let \
                 two screens both believe they own it"
            );
            assert!(
                state.audio_owner.lock().await.is_some(),
                "round {round}: the room audio must end up claimed by whichever screen won"
            );
        }
    }

    /// The regression `owning_display` used to have: it read the recorded
    /// owner, released the lock, awaited the owning display's `cast` mutex,
    /// and then cleared the owner **unconditionally** -- not "if it is still
    /// what I read". A task whose read lands on a stale name and then loses
    /// the scheduler for a while can wake up long after a different screen
    /// has legitimately won the claim, and wipe that screen's claim out from
    /// under it.
    ///
    /// Every candidate screen here is held active (`holding_override`) for
    /// the same reason as the sibling test above: a real winner's own
    /// `is_active()` is always true by the time it can claim audio at all
    /// (`caster_only` already requires a registered sender), so making these
    /// test displays active too keeps self-heal from correctly-but-
    /// distractingly reclaiming a winner for being "inactive". What is
    /// deliberately left stale is the *recorded owner* itself: `"ghost"`
    /// names no declared display, so every task's `owning_display` call
    /// finds it inactive and, on the buggy code, unconditionally clears
    /// whatever `audio_owner` holds *at that later moment* -- which by then
    /// can easily be a different screen's real, active win.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn stale_recorded_owner_cannot_be_cleared_out_from_under_a_winner() {
        const N: usize = 32;
        // Measured against the pre-fix code: 50 rounds only caught the bug
        // about half the time, since the window between `owning_display`'s
        // two lock acquisitions is narrow. 300 pushes that well past even
        // odds while still running in well under a second.
        const ROUNDS: usize = 300;

        let names: Vec<String> = (0..N).map(|i| format!("screen{i}")).collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let state = state_for_displays(&name_refs).await;

        for name in &names {
            state.display(name).unwrap().cast.lock().await.holding_override = true;
        }

        for round in 0..ROUNDS {
            // Stale and undeclared: no task claiming one of the real screens
            // above can ever be "still the recorded owner" here except by
            // way of the very self-heal under test.
            *state.audio_owner.lock().await = Some("ghost".to_string());

            let mut handles = Vec::new();
            for name in &names {
                let state = state.clone();
                let display = state.display(name).unwrap();
                handles.push(tokio::spawn(async move { claim_audio(&state, &display).await }));
            }

            let mut winners = 0;
            for handle in handles {
                if handle.await.unwrap().is_ok() {
                    winners += 1;
                }
            }

            assert_eq!(
                winners, 1,
                "round {round}: a stale, undeclared recorded owner raced by {N} concurrent \
                 claims must still leave exactly one winner -- more than one (or the winner \
                 changing after the fact) means a late self-heal cleared a screen that had \
                 already legitimately won"
            );
        }
    }

    /// `owning_display`'s first branch: the recorded owner no longer names a
    /// declared display at all.
    #[tokio::test]
    async fn self_heal_frees_audio_left_by_an_undeclared_display() {
        let state = state_for_displays(&["foyer"]).await;
        *state.audio_owner.lock().await = Some("werkstatt".to_string());

        assert_eq!(
            owning_display(&state).await, None,
            "a name that names no declared display must be treated as free"
        );
        assert_eq!(
            *state.audio_owner.lock().await, None,
            "the stale owner must be cleared, not just ignored for this one read"
        );
    }

    /// `owning_display`'s second branch: a declared display whose session
    /// never became active. This is the shape a page-mode guest's socket
    /// leaves behind if it drops before ever presenting -- `caster_only` only
    /// needs a registered sender, so it can claim the audio before
    /// `activate_display` (and therefore `deactivate_display`) ever runs for
    /// it; see the updated doc comment on `owning_display`.
    #[tokio::test]
    async fn self_heal_frees_audio_from_a_session_that_was_never_activated() {
        let state = state_for_displays(&["foyer"]).await;
        let foyer = state.display("foyer").unwrap();
        assert!(
            !foyer.cast.lock().await.is_active(),
            "a freshly built session must start inactive, or this test proves nothing"
        );
        *state.audio_owner.lock().await = Some("foyer".to_string());

        assert_eq!(
            owning_display(&state).await, None,
            "a declared display whose session was never activated must be treated as free"
        );
        assert_eq!(*state.audio_owner.lock().await, None);

        // And `claim_audio` must be able to hand it to the same screen
        // afterwards -- the self-heal is only useful if a later claim actually
        // succeeds because of it.
        assert!(claim_audio(&state, &foyer).await.is_ok());
    }
}
