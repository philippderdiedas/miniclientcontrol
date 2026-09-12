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

/// The first CDP port, and the one a single-display deployment has always used.
const BASE_CDP_PORT: u16 = 9222;

#[derive(Clone, Debug)]
pub struct DisplayConfig {
    pub name: String,
    pub cdp_url: String,
    /// Becomes the Wayland `app_id`, which is how the window manager tells two
    /// of our windows apart and puts each on the right output.
    ///
    /// No reader yet: `chromium::spawn` still derives both of these from `Args`,
    /// and the task that gives each display its own browser is what makes them
    /// live. Allowed by field rather than for the module, so everything else
    /// here keeps its dead-code check.
    ///
    /// REMOVE these two allows in the commit that first reads them.
    #[allow(dead_code)]
    pub window_class: String,
    #[allow(dead_code)]
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
    fn two_displays_cannot_share_a_port() {
        assert!(configure(&args_with(vec!["a:9300".into(), "b:9300".into()])).is_err());
    }
}
