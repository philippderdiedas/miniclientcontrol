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
        return Ok(vec![DisplayConfig {
            name: "default".to_string(),
            cdp_url: args.cdp_url.clone(),
            window_class: crate::chromium::window_class(args, port),
            user_data_dir: crate::chromium::user_data_dir(args, port),
        }]);
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
    Ok(out)
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
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": format!("Unbekanntes Display '{name}'."),
            "displays": declared_names(state),
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
/// The assignment half only fires for a row this call has just created. The
/// migration moved every pre-existing item into a playlist named `Standard`,
/// which is the lowest id on any upgraded database, so the first declared
/// display picking it up is the screen carrying on with exactly what it was
/// playing before. A row that already existed is left alone whatever it holds:
/// `NULL` there is an operator who chose "(keine)", and re-assigning it on the
/// next restart would silently undo that decision. A fresh database has no
/// playlists at all, so nothing is assigned and the operator picks one.
pub async fn register(
    pool: &sqlx::SqlitePool,
    configured: &[DisplayConfig],
) -> anyhow::Result<()> {
    let mut first_is_new = false;
    for (index, config) in configured.iter().enumerate() {
        let inserted = sqlx::query(
            "INSERT INTO displays (name) VALUES (?) ON CONFLICT(name) DO NOTHING",
        )
            .bind(&config.name)
            .execute(pool)
            .await?
            .rows_affected()
            > 0;
        if index == 0 {
            first_is_new = inserted;
        }
    }

    if !first_is_new {
        return Ok(());
    }
    let Some(first) = configured.first() else {
        return Ok(());
    };

    let oldest: Option<i64> = sqlx::query_scalar("SELECT id FROM playlists ORDER BY id ASC LIMIT 1")
        .fetch_optional(pool)
        .await?;
    let Some(playlist_id) = oldest else {
        return Ok(());
    };

    // `playlist_id IS NULL` is redundant against a row this call just inserted
    // and is spelled out anyway: it is the condition the rule is actually about,
    // and a later edit that loosens `first_is_new` must not turn this into an
    // overwrite of somebody's assignment.
    sqlx::query("UPDATE displays SET playlist_id = ? WHERE name = ? AND playlist_id IS NULL")
        .bind(playlist_id)
        .bind(&first.name)
        .execute(pool)
        .await?;
    tracing::info!(
        "Display '{}' was registered and assigned playlist {}, the oldest one",
        first.name,
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
    let rows = sqlx::query_as::<_, (String, Option<String>, Option<i64>)>(
        "SELECT name, label, playlist_id FROM displays ORDER BY name ASC",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_else(|e| {
        tracing::error!("Failed to list displays: {}", e);
        Vec::new()
    });

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
    label: Option<String>,
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
            return unknown(&state, &name);
        }
    }

    if let Some(label) = &payload.label {
        // An empty label is stored as SQL `NULL` rather than "": the read path
        // falls back to the name only on `NULL`, so an operator clearing the
        // field would otherwise get a nameless row in the list.
        let trimmed = label.trim();
        let stored = if trimmed.is_empty() { None } else { Some(trimmed) };
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
        if let Err(e) = sqlx::query("UPDATE displays SET playlist_id = ? WHERE name = ?")
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
    async fn only_the_first_declared_display_inherits_the_oldest_playlist() {
        let pool = pool("display_register_first_only").await;
        sqlx::query("INSERT INTO playlists (name) VALUES ('Standard')")
            .execute(&pool)
            .await
            .unwrap();
        let standard: i64 = sqlx::query_scalar("SELECT id FROM playlists WHERE name = 'Standard'")
            .fetch_one(&pool)
            .await
            .unwrap();

        let configured = configure(&args_with(vec!["foyer".into(), "werkstatt".into()])).unwrap();
        register(&pool, &configured).await.unwrap();

        assert_eq!(assignment(&pool, "foyer").await, Some(Some(standard)));
        // The second panel is new hardware nobody has decided about yet, and
        // mirroring the first one by default is the configuration mistake two
        // identical screens look like.
        assert_eq!(assignment(&pool, "werkstatt").await, Some(None));
    }
}
