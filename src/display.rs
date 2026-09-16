//! Which screens this deployment drives.
//!
//! Declared, not discovered. Discovery was prototyped against sway and works,
//! but it puts compositor-specific knowledge inside the controller — sway calls
//! an output `HDMI-A-1` where i3 says `HDMI-1` — and it takes window placement
//! away from the window manager, which is where this project already puts it.
//! See `docs/superpowers/specs/2026-09-12-multi-display-design.md` for the
//! measurements.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, put},
    Json, Router,
};
use serde_json::json;

use crate::models::{AppState, Args, Display};

/// The clap default for `--cdp-url`. Kept here so the declared branch can tell
/// "the operator named a URL" from "nobody passed one" -- the flag has a default
/// and an env var, so its value alone does not say which.
const DEFAULT_CDP_URL: &str = "http://127.0.0.1:9222";

/// The first CDP port, and the one a single-display deployment has always used.
const BASE_CDP_PORT: u16 = 9222;

#[derive(Clone, Debug)]
pub struct DisplayConfig {
    pub name: String,
    pub cdp_url: String,
    /// Becomes the Wayland `app_id`, which is how the window manager tells two
    /// of our windows apart and puts each on the right output.
    pub window_class: String,
    /// One profile per display. Two Chromiums sharing a profile directory
    /// corrupt it, so this is the field that makes several browsers possible at
    /// all.
    pub user_data_dir: PathBuf,
}

/// A name ends up in a window-manager config and in a filesystem path, so it is
/// restricted to what both hold without quoting or escaping.
///
/// Deliberately not called `validate_name`: `playlists::validate_name` already
/// exists and returns the trimmed name rather than `()`, and two same-named
/// helpers with different return types is how a call site ends up quietly
/// wrong.
fn validate_display_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Ein Display-Name darf nicht leer sein.".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "Display-Name '{name}': nur Buchstaben, Ziffern, - und _ sind erlaubt."
        ));
    }
    Ok(())
}

/// Resolve the declared displays, or the single implicit one.
///
/// Fails the process rather than degrading: a typo that silently dropped a
/// screen would show up as a black panel in a venue, with nothing saying why.
pub fn configure(args: &Args) -> Result<Vec<DisplayConfig>, String> {
    if args.display.is_empty() {
        // Exactly today's behaviour, down to the class derived from the port.
        let port = crate::chromium::debugging_port(&args.cdp_url).unwrap_or(BASE_CDP_PORT);
        let out = vec![DisplayConfig {
            name: "default".to_string(),
            cdp_url: args.cdp_url.clone(),
            window_class: crate::chromium::window_class(args, port),
            user_data_dir: crate::chromium::user_data_dir(args, port),
        }];
        // Checked on this branch too: the implicit display is called `default`,
        // so `--cast-display foyer` without any `--display` names nothing and
        // would otherwise fall back to the implicit screen without a word.
        check_cast_display(args, &out)?;
        return Ok(out);
    }

    // `--class` inside `--chromium-arg` is appended after the derived or pinned
    // `--class` on the Chromium command line, and Chromium is last-wins for a
    // repeated switch. Left alone, it would quietly collapse every display onto
    // one `app_id`, which is exactly the placement failure a named display
    // exists to avoid. There is already a purpose-built flag for this
    // (`--chromium-class`).
    //
    // Below the early return on purpose: with no `--display` there is no
    // second window to collide with, the smuggled class simply wins as it
    // always did, and refusing there would turn a command line that works
    // today into a refusal to boot.
    if args
        .chromium_arg
        .iter()
        .any(|a| a == "--class" || a.starts_with("--class="))
    {
        return Err(
            "--class gehört nicht in --chromium-arg: es würde als letztes Flag über die \
             abgeleitete oder gepinnte Klasse gewinnen und mehrere Displays auf denselben \
             app_id kollabieren lassen. --chromium-class verwenden."
                .to_string(),
        );
    }

    // The declared branch below always derives the port from a display's
    // position and the class and profile from its name, and never reads these
    // three flags back -- so a deployment that passes one of them alongside
    // any `--display` would have it silently do nothing, which is what the
    // "a flag actually passed pins its setting" rule (see `settings.rs`) exists
    // to prevent. Refusing this for one declared display too, not only several,
    // is deliberate: honouring the pin for exactly one would be a rule that
    // changes meaning the moment a venue adds a second panel, which is the
    // shape that breaks later. With several displays it is also the collision
    // it always was -- a shared profile corrupts, a shared `app_id` only
    // misplaces -- but that is no longer the only reason it is refused.
    // Compared whole, not by port: `--cdp-url http://192.168.1.5:9222` names a
    // different host on the default port, and a port-only check waved it
    // through to be silently rewritten to loopback -- the very silent drop this
    // guard exists to stop.
    let cdp_url_pinned = args.cdp_url != DEFAULT_CDP_URL;
    if args.chromium_class.is_some() || args.chromium_user_data_dir.is_some() || cdp_url_pinned {
        return Err(
            "--chromium-class, --chromium-user-data-dir und --cdp-url werden mit --display \
             aus Position und Namen des Displays abgeleitet und nicht aus diesen Flags \
             gelesen. Diese Flags nur ohne --display verwenden."
                .to_string(),
        );
    }

    let mut out: Vec<DisplayConfig> = Vec::new();
    for (index, raw) in args.display.iter().enumerate() {
        let (name, explicit) = match raw.split_once(':') {
            Some((name, port)) => {
                let parsed: u16 = port
                    .parse()
                    .map_err(|_| format!("Display '{name}': '{port}' ist kein Port."))?;
                (name, Some(parsed))
            }
            None => (raw.as_str(), None),
        };
        validate_display_name(name)?;
        if out.iter().any(|d| d.name == name) {
            return Err(format!("Display '{name}' ist doppelt deklariert."));
        }
        // Implicit ports count from the base by declaration index, so giving one
        // display an explicit port does not shift another one's.
        let port = explicit.unwrap_or(BASE_CDP_PORT + index as u16);
        if out.iter().any(|d| d.cdp_url.ends_with(&format!(":{port}"))) {
            return Err(format!("CDP-Port {port} ist doppelt vergeben."));
        }
        out.push(DisplayConfig {
            name: name.to_string(),
            cdp_url: format!("http://127.0.0.1:{port}"),
            window_class: format!("miniclientcontrol-{name}"),
            user_data_dir: PathBuf::from(format!("/tmp/miniclientcontrol-chromium-{name}")),
        });
    }
    check_cast_display(args, &out)?;
    Ok(out)
}

/// Refuse a `--cast-display` naming a screen this deployment does not drive.
///
/// At startup, because nothing downstream can tell a typo from a name: a cast
/// resolves a screen without consulting this flag, so the mistake would surface
/// as a guest scanning a QR code and the cast appearing on the wrong panel,
/// hours later and with nothing in the log tying the two together.
fn check_cast_display(args: &Args, out: &[DisplayConfig]) -> Result<(), String> {
    if let Some(wanted) = &args.cast_display {
        if !out.iter().any(|d| &d.name == wanted) {
            return Err(format!(
                "--cast-display '{wanted}' ist kein deklariertes Display."
            ));
        }
    }
    Ok(())
}

/// Whether an unscoped legacy path has to refuse.
///
/// One display is the deployment every existing venue runs, and there is no
/// ambiguity to report there -- an upgrade must keep whatever scripts an
/// operator already has working untouched.
pub fn unscoped_is_ambiguous(display_count: usize) -> bool {
    display_count > 1
}

/// Resolve the display a request is about.
///
/// `None` is the legacy unscoped path. It resolves while one display exists and
/// refuses once several do -- picking one would be a coin flip an operator's
/// existing script cannot see, and a screen changing on its own is exactly the
/// failure this project treats as worst.
pub fn resolve(state: &AppState, name: Option<&str>) -> Result<Arc<Display>, Response> {
    match name {
        Some(name) => state.display(name).ok_or_else(|| unknown(state, name)),
        None if unscoped_is_ambiguous(state.displays.len()) => Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "Mehrere Displays: bitte /api/displays/<name>/… verwenden.",
                "displays": declared_names(state),
            })),
        )
            .into_response()),
        None => Ok(state.primary()),
    }
}

/// The declared names, which every refusal carries: a caller that guessed wrong
/// is told what it could have said instead, because the alternative is an
/// operator reading a 404 with nothing to try next.
fn declared_names(state: &AppState) -> Vec<String> {
    state.displays.iter().map(|d| d.name.clone()).collect()
}

fn unknown(state: &AppState, name: &str) -> Response {
    refusal(name, declared_names(state))
}

/// The same `404`, for `PUT /api/displays/{name}`, which accepts more names than
/// the scoped playback routes do: a stored row this deployment no longer
/// declares is still writable, because that is the screen taken away whose
/// playlist is being reassigned. Listing only the declared names there would
/// hand an operator who mistyped such a name a list excluding every name that
/// would have worked.
async fn unknown_here(state: &AppState, name: &str) -> Response {
    let mut names = declared_names(state);
    match sqlx::query_scalar::<_, String>("SELECT name FROM displays ORDER BY name ASC")
        .fetch_all(&state.pool)
        .await
    {
        Ok(stored) => names.extend(stored.into_iter().filter(|n| state.display(n).is_none())),
        // The declared names alone are still a useful answer, and a refusal is
        // no place to turn one failed read into a second error.
        Err(e) => tracing::error!("Failed to list stored displays for a refusal: {}", e),
    }
    refusal(name, names)
}

fn refusal(name: &str, names: Vec<String>) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": format!("Unbekanntes Display '{name}'."),
            "displays": names,
        })),
    )
        .into_response()
}

/// Make sure every declared display has a row, and carry a single-screen
/// deployment's playlist across the upgrade.
///
/// Nothing else ever writes this table on its own, so without this a display's
/// assignment is `NULL` on every database that exists -- and "no assignment
/// means the idle screen" would blank every venue that upgrades, single-screen
/// ones included, with no way to repair it from the outside.
///
/// The assignment half fires while nobody has yet decided the first declared
/// display's playlist -- which is `displays.assignment_decided`, not "this call
/// inserted the row". The migration moved every pre-existing item into a
/// playlist named `Standard`, the lowest id on any upgraded database, so the
/// first declared display picking it up is the screen carrying on with exactly
/// what it was playing before.
///
/// The flag is stored rather than inferred from the insert because the question
/// it answers outlives the process. A fresh install starts with no playlists at
/// all, so there is nothing to inherit on the first run and the decision is
/// still open when the operator creates one -- with a per-call gate that screen
/// would never play anything and no restart would repair it. The same applies to
/// a power loss between the insert and the assignment, which on a signage Pi is
/// an ordinary way to stop. What the flag does *not* do is undo a decision:
/// every path that writes `playlist_id`, here and in `update`, sets it in the
/// same statement, so an operator's "(keine)" is a decided `NULL` and is left
/// alone for good.
pub async fn register(
    pool: &sqlx::SqlitePool,
    configured: &[DisplayConfig],
) -> anyhow::Result<()> {
    // One transaction around both halves. They are two statements describing one
    // decision, and a machine that loses power between them must come back to
    // either both or neither rather than to a registered screen that will never
    // be offered a playlist.
    let mut tx = pool.begin().await?;
    for (index, config) in configured.iter().enumerate() {
        sqlx::query("INSERT INTO displays (name) VALUES (?) ON CONFLICT(name) DO NOTHING")
            .bind(&config.name)
            .execute(&mut *tx)
            .await?;
        // Inside the loop, indexed, rather than over `configured.first()` after
        // it: `configure` never yields an empty slice, so the early return that
        // used to guard the first element could not fire and only looked like a
        // case somebody had thought about.
        //
        // Only when exactly one display is declared. The rule exists to carry an
        // *upgrade* across -- a device that played a playlist before this
        // feature existed must keep playing it -- and a deployment that declares
        // two screens is not that. Left ungated, a fresh two-screen install
        // where the operator creates one playlist for the workshop would hand it
        // to the foyer instead.
        if index == 0 && configured.len() == 1 {
            inherit_oldest_playlist(&mut tx, &config.name).await?;
        }
    }
    // Say so rather than leaving the operator to wonder. With several screens
    // declared nothing is inherited, so a device that used to play something and
    // has just been given two `--display` flags comes up with both screens idle;
    // that is the intended outcome, but only if it is discoverable.
    if configured.len() > 1 {
        let undecided: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM displays WHERE playlist_id IS NULL
               AND COALESCE(assignment_decided, 0) = 0",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(0);
        let playlists: i64 = sqlx::query_scalar("SELECT count(*) FROM playlists")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(0);
        if undecided > 0 && playlists > 0 {
            tracing::info!(
                "{} display(s) have no playlist yet. With several screens declared \
                 none is assigned automatically -- pick one per screen on the \
                 displays page.",
                undecided
            );
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Give a display the oldest playlist, once, while its assignment is still
/// nobody's decision.
/// Offer the one declared display a playlist the moment one first exists.
///
/// `register` runs at startup, and a fresh install has no playlist to inherit
/// then -- so without this an operator who creates their first playlist watches
/// the screen stay on the idle page until they also assign it, or restart. The
/// gate is the same one `register` uses, so a deliberate clearing is still
/// permanent and a deployment with several screens still chooses explicitly:
/// handing the first screen a playlist made for the second would be wrong
/// content, which reads as deliberate, where an idle screen reads as
/// "configure me".
pub async fn offer_first_playlist(state: &AppState) {
    if state.displays.len() != 1 {
        return;
    }
    let name = state.primary().name.clone();
    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!("Failed to offer a playlist to '{}': {}", name, e);
            return;
        }
    };
    if let Err(e) = inherit_oldest_playlist(&mut tx, &name).await {
        tracing::error!("Failed to offer a playlist to '{}': {}", name, e);
        return;
    }
    if let Err(e) = tx.commit().await {
        tracing::error!("Failed to offer a playlist to '{}': {}", name, e);
    }
}

async fn inherit_oldest_playlist(
    tx: &mut sqlx::SqliteConnection,
    name: &str,
) -> anyhow::Result<()> {
    let row: Option<(Option<i64>, i64)> = sqlx::query_as(
        "SELECT playlist_id, COALESCE(assignment_decided, 0) FROM displays WHERE name = ?",
    )
    .bind(name)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((assigned, decided)) = row else {
        return Ok(());
    };
    if decided != 0 {
        return Ok(());
    }

    // An assignment that is already there without the flag came from a database
    // written before this column existed, or from somebody editing the file.
    // Either way it is a decision, and recording it stops the next start from
    // reading the screen as undecided forever.
    if assigned.is_some() {
        sqlx::query("UPDATE displays SET assignment_decided = 1 WHERE name = ?")
            .bind(name)
            .execute(&mut *tx)
            .await?;
        return Ok(());
    }

    let oldest: Option<i64> = sqlx::query_scalar("SELECT id FROM playlists ORDER BY id ASC LIMIT 1")
        .fetch_optional(&mut *tx)
        .await?;
    let Some(playlist_id) = oldest else {
        // Deliberately leaves the display undecided. A fresh install has no
        // playlist to inherit on its first start, and the operator creating one
        // afterwards is exactly the case that has to still be picked up -- the
        // alternative is a screen that stays black with nothing to change.
        return Ok(());
    };

    // `playlist_id IS NULL` is already true here and is spelled out anyway: it is
    // the condition the rule is really about, and a later edit that loosens the
    // gate above must not turn this into an overwrite of somebody's assignment.
    sqlx::query(
        "UPDATE displays SET playlist_id = ?, assignment_decided = 1
         WHERE name = ? AND playlist_id IS NULL",
    )
    .bind(playlist_id)
    .bind(name)
    .execute(&mut *tx)
    .await?;
    tracing::info!(
        "Display '{}' had no playlist yet and was assigned playlist {}, the oldest one",
        name,
        playlist_id
    );
    Ok(())
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/displays", get(list))
        .route("/api/displays/{name}", put(update))
        .route(
            "/api/displays/{name}/control/current",
            get(crate::handlers::get_current_for).post(crate::handlers::set_current_for),
        )
        .route(
            "/api/displays/{name}/override",
            get(crate::handlers::get_override_for)
                .post(crate::handlers::set_override_for)
                .delete(crate::handlers::clear_override_for),
        )
}

async fn list(State(state): State<AppState>) -> Response {
    let rows = match sqlx::query_as::<_, (String, Option<String>, Option<i64>)>(
        "SELECT name, label, playlist_id FROM displays ORDER BY name ASC",
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!("Failed to list displays: {}", e);
            // Swallowing this one would render every declared screen as
            // unassigned, which is a real state an operator acts on -- they
            // would go and assign a playlist that is already assigned, or read
            // a blank screen's cause off a list that invented it. Saying
            // nothing could be read is the only honest answer here.
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Displays konnten nicht gelesen werden." })),
            )
                .into_response();
        }
    };

    // Driven off the declared displays, not off the table: a row for a screen
    // this deployment no longer declares must still be visible (so its playlist
    // can be reassigned) but must not claim to be attached.
    let out: Vec<_> = state
        .displays
        .iter()
        .map(|d| {
            let stored = rows.iter().find(|(name, _, _)| name == &d.name);
            json!({
                "name": d.name,
                "label": stored.and_then(|(_, label, _)| label.clone()).unwrap_or_else(|| d.name.clone()),
                "playlist_id": stored.and_then(|(_, _, id)| *id),
                "declared": true,
            })
        })
        .chain(
            rows.iter()
                .filter(|(name, _, _)| state.display(name).is_none())
                .map(|(name, label, playlist_id)| {
                    json!({
                        "name": name,
                        "label": label.clone().unwrap_or_else(|| name.clone()),
                        "playlist_id": playlist_id,
                        "declared": false,
                    })
                }),
        )
        .collect();
    Json(out).into_response()
}

#[derive(serde::Deserialize)]
struct UpdateDisplay {
    /// `double_option` for the same reason `playlist_id` has it: without it a
    /// JSON `null` collapses into the outer `None` and reads as "field absent",
    /// so a label could only ever be cleared by sending `""` -- a rule nothing
    /// states and the next client would not guess.
    #[serde(default, deserialize_with = "crate::handlers::double_option")]
    label: Option<Option<String>>,
    #[serde(default, deserialize_with = "crate::handlers::double_option")]
    playlist_id: Option<Option<i64>>,
}

async fn update(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(payload): Json<UpdateDisplay>,
) -> Response {
    // A declared display is upserted rather than looked up, so a row deleted out
    // of band comes back instead of turning into a refusal an operator cannot
    // act on. A name this deployment does *not* declare is only writable when it
    // already has a row -- that is the screen taken away whose playlist is being
    // reassigned. Anything else is a typo, and accepting it would write a row
    // nothing reads, answer `200`, and leave the operator believing a screen was
    // configured.
    let declared = state.display(&name).is_some();
    if declared {
        if let Err(e) =
            sqlx::query("INSERT INTO displays (name) VALUES (?) ON CONFLICT(name) DO NOTHING")
                .bind(&name)
                .execute(&state.pool)
                .await
        {
            tracing::error!("Failed to ensure display row for {}: {}", name, e);
        }
    } else {
        let known: i64 = sqlx::query_scalar("SELECT count(*) FROM displays WHERE name = ?")
            .bind(&name)
            .fetch_one(&state.pool)
            .await
            .unwrap_or_else(|e| {
                tracing::error!("Failed to look up display {}: {}", name, e);
                0
            });
        if known == 0 {
            return unknown_here(&state, &name).await;
        }
    }

    // Checked before anything is written, and checked at all because the foreign
    // key would otherwise refuse the write, the error would be logged and
    // swallowed, and the operator would be told `200` for an assignment that
    // never happened -- ending in the blank screen `PUT /api/playlist/{id}`
    // checks `asset_id` against `assets` to avoid.
    if let Some(Some(playlist_id)) = payload.playlist_id {
        match sqlx::query_scalar::<_, i64>("SELECT id FROM playlists WHERE id = ?")
            .bind(playlist_id)
            .fetch_optional(&state.pool)
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "Unbekannte Playlist." })),
                )
                    .into_response()
            }
            Err(e) => {
                tracing::error!("Failed to check playlist {}: {}", playlist_id, e);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    }

    if let Some(label) = &payload.label {
        // An empty or absent label is stored as SQL `NULL` rather than "": the
        // read path falls back to the name only on `NULL`, so an operator
        // clearing the field would otherwise get a nameless row in the list.
        let stored = label.as_deref().map(str::trim).filter(|l| !l.is_empty());
        if let Err(e) = sqlx::query("UPDATE displays SET label = ? WHERE name = ?")
            .bind(stored)
            .bind(&name)
            .execute(&state.pool)
            .await
        {
            tracing::error!("Failed to set label for display {}: {}", name, e);
        }
    }
    if let Some(playlist_id) = payload.playlist_id {
        // `assignment_decided` is set in the same statement as the assignment,
        // not beside it: the record that somebody chose and what they chose can
        // then not be torn apart by a power loss, which is the only way the next
        // start could inherit a playlist over a deliberate "(keine)".
        if let Err(e) = sqlx::query(
            "UPDATE displays SET playlist_id = ?, assignment_decided = 1 WHERE name = ?",
        )
        .bind(playlist_id)
        .bind(&name)
        .execute(&state.pool)
        .await
        {
            tracing::error!("Failed to assign playlist to display {}: {}", name, e);
        }
        // The loop re-reads its assignment every pass, but poking it means the
        // change lands on the next item rather than at the end of this one.
        if let Some(display) = state.display(&name) {
            display.playlist_signal.notify_one();
        }
    }
    Json(json!({ "ok": true })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn args_with(displays: Vec<String>) -> crate::models::Args {
        let mut args = crate::models::Args::parse_from(["miniclientcontrol"]);
        args.display = displays;
        args
    }

    #[test]
    fn no_flag_means_one_display_that_behaves_exactly_as_today() {
        let configured = configure(&args_with(vec![])).unwrap();
        assert_eq!(configured.len(), 1);
        assert_eq!(configured[0].name, "default");
        // The existing defaults, untouched: these devices run unattended and an
        // upgrade must not move their CDP port or their window class.
        assert_eq!(configured[0].cdp_url, "http://127.0.0.1:9222");
        assert_eq!(configured[0].window_class, "miniclientcontrol-9222");
        // Asserted too, because a regression in this one field alone would
        // point a second browser at the first one's profile directory.
        assert_eq!(
            configured[0].user_data_dir,
            std::path::PathBuf::from("/tmp/miniclientcontrol-chromium-9222")
        );
    }

    #[test]
    fn declared_names_get_derived_ports_and_classes() {
        let configured =
            configure(&args_with(vec!["foyer".into(), "werkstatt".into()])).unwrap();
        assert_eq!(configured.len(), 2);
        assert_eq!(configured[0].cdp_url, "http://127.0.0.1:9222");
        assert_eq!(configured[1].cdp_url, "http://127.0.0.1:9223");
        // Named, not numbered: a human writes the window-manager config and
        // should read "werkstatt" there, not "9223".
        assert_eq!(configured[0].window_class, "miniclientcontrol-foyer");
        assert_eq!(configured[1].window_class, "miniclientcontrol-werkstatt");
        assert!(configured[1].user_data_dir.to_string_lossy().contains("werkstatt"));
    }

    #[test]
    fn an_explicit_port_wins() {
        let configured =
            configure(&args_with(vec!["foyer:9300".into(), "werkstatt".into()])).unwrap();
        assert_eq!(configured[0].cdp_url, "http://127.0.0.1:9300");
        // The implicit one still counts from the base by index, so declaring an
        // explicit port for one display does not silently move another.
        assert_eq!(configured[1].cdp_url, "http://127.0.0.1:9223");
    }

    #[test]
    fn duplicates_and_nonsense_are_refused_at_startup() {
        assert!(configure(&args_with(vec!["foyer".into(), "foyer".into()])).is_err());
        assert!(configure(&args_with(vec!["".into()])).is_err());
        assert!(configure(&args_with(vec!["foyer:nichtszahl".into()])).is_err());
        // A name reaches a window-manager config and a filesystem path, so keep
        // it to something both can hold without quoting.
        assert!(configure(&args_with(vec!["foyer schirm".into()])).is_err());
        assert!(configure(&args_with(vec!["../etc".into()])).is_err());
    }

    #[test]
    fn the_primary_display_is_the_first_declared() {
        // `--cast-display` and the legacy unscoped API paths both resolve
        // through this, so which one is primary is not an implementation detail.
        let configured = configure(&args_with(vec!["foyer".into(), "werkstatt".into()])).unwrap();
        assert_eq!(configured[0].name, "foyer");
    }

    #[test]
    fn an_unknown_cast_display_fails_at_startup() {
        let mut args = args_with(vec!["foyer".into()]);
        args.cast_display = Some("kueche".into());
        assert!(configure(&args).is_err());
        args.cast_display = Some("foyer".into());
        assert!(configure(&args).is_ok());

        // Without `--display` the one screen is called `default`, and a name
        // that is not it must be refused here as well -- that branch returns
        // early, so it is the easy one to leave unchecked.
        let mut implicit = args_with(vec![]);
        implicit.cast_display = Some("foyer".into());
        assert!(configure(&implicit).is_err());
        implicit.cast_display = Some("default".into());
        assert!(configure(&implicit).is_ok());
    }

    #[test]
    fn a_pinned_flag_is_refused_once_any_display_is_declared() {
        // The declared branch derives port, class and profile from the name and
        // its position; a pin alongside it would silently do nothing, so this is
        // refused for one declared display exactly as for several -- honouring
        // it for exactly one would be a rule that changes meaning the moment a
        // second display is added.
        let mut single_class = args_with(vec!["a".into()]);
        single_class.chromium_class = Some("fest".into());
        assert!(configure(&single_class).is_err());

        let mut single_profile = args_with(vec!["a".into()]);
        single_profile.chromium_user_data_dir = Some(std::path::PathBuf::from("/tmp/fest"));
        assert!(configure(&single_profile).is_err());

        let mut single_cdp = args_with(vec!["a".into()]);
        single_cdp.cdp_url = "http://127.0.0.1:9999".into();
        assert!(configure(&single_cdp).is_err());

        let mut several_class = args_with(vec!["a".into(), "b".into()]);
        several_class.chromium_class = Some("fest".into());
        assert!(configure(&several_class).is_err());

        let mut several_profile = args_with(vec!["a".into(), "b".into()]);
        several_profile.chromium_user_data_dir = Some(std::path::PathBuf::from("/tmp/fest"));
        assert!(configure(&several_profile).is_err());

        let mut several_cdp = args_with(vec!["a".into(), "b".into()]);
        several_cdp.cdp_url = "http://127.0.0.1:9999".into();
        assert!(configure(&several_cdp).is_err());

        // No --display at all still honours every pin: that is the deployment
        // real venues use today, and it must not move.
        let mut no_display = args_with(vec![]);
        no_display.chromium_class = Some("fest".into());
        assert!(configure(&no_display).is_ok());
        assert_eq!(configure(&no_display).unwrap()[0].window_class, "fest");
    }

    #[test]
    fn a_smuggled_class_via_chromium_arg_is_refused() {
        // A repeated Chromium switch is last-wins, and this one is appended
        // after the derived or pinned `--class` on the command line, so left
        // alone it would collapse every display onto one `app_id`.
        let mut args = args_with(vec!["a".into(), "b".into()]);
        args.chromium_arg = vec!["--class=sneaky".into()];
        assert!(configure(&args).is_err());

        // But NOT without --display. There is no second window to collide with
        // there, the smuggled class simply wins as it always did, and refusing
        // would turn a command line that works today into a refusal to boot --
        // which the "no --display behaves exactly as today" rule forbids.
        let mut no_display = args_with(vec![]);
        no_display.chromium_arg = vec!["--class=sneaky".into()];
        assert!(
            configure(&no_display).is_ok(),
            "refusing this without --display breaks an existing command line"
        );
    }

    #[test]
    fn two_displays_cannot_share_a_port() {
        assert!(configure(&args_with(vec!["a:9300".into(), "b:9300".into()])).is_err());
    }

    async fn pool(name: &str) -> sqlx::SqlitePool {
        // A named in-memory database, not the bare `sqlite::memory:?cache=shared`:
        // the shared cache is keyed by that name process-wide, so two tests
        // running in parallel would otherwise land in one database.
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite:file:{name}?mode=memory&cache=shared"
        ))
        .await
        .unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        pool
    }

    async fn assignment(pool: &sqlx::SqlitePool, name: &str) -> Option<Option<i64>> {
        sqlx::query_scalar::<_, Option<i64>>("SELECT playlist_id FROM displays WHERE name = ?")
            .bind(name)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    #[test]
    fn an_unscoped_path_resolves_only_while_one_display_exists() {
        // With one display there is no ambiguity to report.
        assert!(!unscoped_is_ambiguous(1));
        // With two, picking one would be a coin flip an operator's script
        // cannot see, so the request is refused and told the names instead.
        assert!(unscoped_is_ambiguous(2));
    }

    #[tokio::test]
    async fn an_upgraded_deployment_keeps_playing_its_standard_playlist() {
        let pool = pool("display_register_upgrade").await;
        // What the item backfill leaves behind on any device that upgrades.
        sqlx::query("INSERT INTO playlists (name) VALUES ('Standard')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO playlists (name) VALUES ('Werkstatt')")
            .execute(&pool)
            .await
            .unwrap();
        let standard: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name = 'Standard'")
            .fetch_one(&pool)
            .await
            .unwrap();

        let configured = configure(&args_with(vec![])).unwrap();
        register(&pool, &configured).await.unwrap();

        // The screen that was playing these items before the upgrade keeps
        // playing them; the oldest playlist is the one the backfill created.
        assert_eq!(assignment(&pool, "default").await, Some(Some(standard)));

        // Idempotent: a second start changes nothing.
        register(&pool, &configured).await.unwrap();
        assert_eq!(assignment(&pool, "default").await, Some(Some(standard)));
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM displays")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);

        // And an operator who chose "(keine)" keeps that choice across a
        // restart: re-assigning the oldest playlist here would silently undo a
        // decision somebody made on purpose.
        sqlx::query("UPDATE displays SET playlist_id = NULL WHERE name = 'default'")
            .execute(&pool)
            .await
            .unwrap();
        register(&pool, &configured).await.unwrap();
        assert_eq!(assignment(&pool, "default").await, Some(None));
    }

    #[tokio::test]
    async fn a_fresh_database_registers_the_displays_and_assigns_nothing() {
        let pool = pool("display_register_fresh").await;
        let configured = configure(&args_with(vec!["foyer".into(), "werkstatt".into()])).unwrap();
        register(&pool, &configured).await.unwrap();

        // Registered, so the operator has something to assign a playlist to...
        assert_eq!(assignment(&pool, "foyer").await, Some(None));
        assert_eq!(assignment(&pool, "werkstatt").await, Some(None));
        // ...and nothing assigned, because there is no playlist to guess at.
        // That is the operator's first decision, not ours.
    }

    #[tokio::test]
    async fn a_fresh_install_inherits_the_playlist_the_operator_creates_later() {
        let pool = pool("display_register_fresh_then_playlist").await;
        let configured = configure(&args_with(vec![])).unwrap();

        // First start of a brand new device: a row, no playlist to inherit, and
        // therefore nothing decided yet.
        register(&pool, &configured).await.unwrap();
        assert_eq!(assignment(&pool, "default").await, Some(None));

        // The operator does the first thing anybody does with a new device.
        sqlx::query("INSERT INTO playlists (name) VALUES ('Eingang')")
            .execute(&pool)
            .await
            .unwrap();
        let eingang: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name = 'Eingang'")
            .fetch_one(&pool)
            .await
            .unwrap();

        // The next start picks it up. A gate that only fired for a row inserted
        // by this very call would leave the screen black here, with no restart
        // and no API call that repairs it.
        register(&pool, &configured).await.unwrap();
        assert_eq!(assignment(&pool, "default").await, Some(Some(eingang)));
    }

    #[tokio::test]
    async fn a_torn_write_between_the_insert_and_the_assignment_recovers() {
        let pool = pool("display_register_torn_write").await;
        sqlx::query("INSERT INTO playlists (name) VALUES ('Standard')")
            .execute(&pool)
            .await
            .unwrap();
        let standard: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name = 'Standard'")
            .fetch_one(&pool)
            .await
            .unwrap();

        // What a power loss between the two statements used to leave behind: the
        // row exists, nothing is assigned, and nothing records that the question
        // was ever asked. On an upgrading single-screen venue this is precisely
        // the "cannot play any more" failure registration exists to prevent.
        sqlx::query("INSERT INTO displays (name, assignment_decided) VALUES ('default', 0)")
            .execute(&pool)
            .await
            .unwrap();

        let configured = configure(&args_with(vec![])).unwrap();
        register(&pool, &configured).await.unwrap();
        assert_eq!(assignment(&pool, "default").await, Some(Some(standard)));

        // And the repair is itself a decision, so clearing it afterwards sticks.
        sqlx::query("UPDATE displays SET playlist_id = NULL WHERE name = 'default'")
            .execute(&pool)
            .await
            .unwrap();
        register(&pool, &configured).await.unwrap();
        assert_eq!(assignment(&pool, "default").await, Some(None));
    }

    #[tokio::test]
    async fn a_cleared_assignment_survives_a_restart_that_finds_a_playlist() {
        let pool = pool("display_register_keine_survives").await;
        sqlx::query("INSERT INTO playlists (name) VALUES ('Standard')")
            .execute(&pool)
            .await
            .unwrap();
        let configured = configure(&args_with(vec![])).unwrap();
        register(&pool, &configured).await.unwrap();

        // What `PUT /api/displays/{name}` writes for "(keine)": the assignment
        // and the record that somebody chose it, in one statement.
        sqlx::query(
            "UPDATE displays SET playlist_id = NULL, assignment_decided = 1 WHERE name = 'default'",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Playlists keep arriving; none of them is an invitation to overrule the
        // operator at the next restart.
        sqlx::query("INSERT INTO playlists (name) VALUES ('Neu')")
            .execute(&pool)
            .await
            .unwrap();
        register(&pool, &configured).await.unwrap();
        assert_eq!(assignment(&pool, "default").await, Some(None));
    }

    #[tokio::test]
    async fn several_declared_displays_inherit_nothing() {
        let pool = pool("display_register_several").await;
        sqlx::query("INSERT INTO playlists (name) VALUES ('Werkstatt')")
            .execute(&pool)
            .await
            .unwrap();

        let configured = configure(&args_with(vec!["foyer".into(), "werkstatt".into()])).unwrap();
        register(&pool, &configured).await.unwrap();

        // The inheritance carries an *upgrade* across: a device that played a
        // playlist before this feature existed must keep playing it. A
        // deployment declaring two screens is not that, and handing the first
        // one a playlist the operator made for the second would be wrong
        // content rather than no content -- which is worse, because no content
        // reads as "configure me" and wrong content reads as deliberate.
        assert_eq!(assignment(&pool, "foyer").await, Some(None));
        assert_eq!(assignment(&pool, "werkstatt").await, Some(None));
    }

    #[tokio::test]
    async fn a_single_declared_display_still_inherits() {
        let pool = pool("display_register_single_named").await;
        sqlx::query("INSERT INTO playlists (name) VALUES ('Standard')")
            .execute(&pool)
            .await
            .unwrap();
        let standard: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name = 'Standard'")
            .fetch_one(&pool)
            .await
            .unwrap();

        // Naming the one screen it already had is still an upgrade, so the rule
        // applies -- it is the count that decides, not whether a flag was used.
        let configured = configure(&args_with(vec!["foyer".into()])).unwrap();
        register(&pool, &configured).await.unwrap();

        assert_eq!(assignment(&pool, "foyer").await, Some(Some(standard)));
    }
}
