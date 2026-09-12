//! Which screens this deployment drives.
//!
//! Declared, not discovered. Discovery was prototyped against sway and works,
//! but it puts compositor-specific knowledge inside the controller — sway calls
//! an output `HDMI-A-1` where i3 says `HDMI-1` — and it takes window placement
//! away from the window manager, which is where this project already puts it.
//! See `docs/superpowers/specs/2026-09-12-multi-display-design.md` for the
//! measurements.

use std::path::PathBuf;

use crate::models::Args;

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
}
