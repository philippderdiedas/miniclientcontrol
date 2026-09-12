//! Launching and supervising the display browser.
//!
//! The controller starts the browser and keeps it alive, unless something is
//! already listening on the CDP port — an externally managed Chromium is then
//! driven as it stands, so a deployment that launches it from a session file
//! needs no change here.
//!
//! Keeping the launch in one place keeps the debugging port and the browser's
//! flags from having to agree across two, and lets a browser that exits be
//! restarted.
//!
//! ## Why the profile is written here
//!
//! Chromium's "translate this page?" bubble cannot be turned off with a flag on
//! Linux, and there is no CDP command for it. The managed policy under
//! `/etc/chromium/policies/managed/` does work, but needs root.
//!
//! What stops the bubble is the language list: the prompt appears when the page's
//! language is not among the profile's accepted languages. So
//! `intl.accept_languages` is set from `--browser-language` and
//! `translate.enabled` to false. Chromium keeps both keys when it rewrites the
//! file, and writing them on every start covers a profile directory that does not
//! survive a reboot.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::{Child, Command};
use tracing::{debug, error, info, warn};

use crate::models::Args;

/// Where the supervisor records the running browser's PID, so other parts of the
/// controller can tell its processes apart from everything else on the machine.
pub type PidSlot = std::sync::Arc<tokio::sync::Mutex<Option<u32>>>;

/// Extra flags every launch gets.
///
/// `--disable-features=Translate` is included even though it does not gate the
/// bubble (it is applied and inherited, it just does not do this job) — it costs
/// nothing and removes the translate machinery from the child processes.
const BASE_ARGS: &[&str] = &[
    "--no-first-run",
    "--no-default-browser-check",
    "--noerrdialogs",
    "--disable-session-crashed-bubble",
    "--disable-features=Translate",
    // An unattended display has nobody to click "allow sound".
    "--autoplay-policy=no-user-gesture-required",
];

pub fn detect_executable(configured: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = configured {
        if !path.exists() {
            anyhow::bail!("configured browser {} does not exist", path.display());
        }
        return Ok(path.to_path_buf());
    }
    chromiumoxide::detection::default_executable(chromiumoxide::detection::DetectionOptions {
        // Edge would be a surprising thing to end up on a signage box.
        msedge: false,
        unstable: false,
    })
    .map_err(|e| anyhow::anyhow!("could not find Chrome or Chromium: {e}"))
}

/// The port `--cdp-url` points at, which is the port we must tell Chromium to
/// open. Keeping one source of truth avoids the classic "launched on 9222,
/// connecting to 9223" afternoon.
pub(crate) fn debugging_port(cdp_url: &str) -> Result<u16> {
    let rest = cdp_url.split("://").nth(1).unwrap_or(cdp_url);
    let host_port = rest.split('/').next().unwrap_or(rest);
    host_port
        .rsplit(':')
        .next()
        .and_then(|port| port.parse().ok())
        .with_context(|| format!("no port in --cdp-url '{}'", cdp_url))
}

/// Merge our keys into the profile's `Preferences`, keeping anything already
/// there. Overwriting wholesale would discard zoom levels, window bounds and
/// whatever else Chromium keeps in that file.
fn write_preferences(user_data_dir: &Path, languages: &str) -> Result<()> {
    let profile = user_data_dir.join("Default");
    std::fs::create_dir_all(&profile)
        .with_context(|| format!("creating {}", profile.display()))?;
    let path = profile.join("Preferences");

    let mut prefs: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    if !prefs.is_object() {
        prefs = serde_json::json!({});
    }
    let root = prefs.as_object_mut().expect("object");

    let intl = root
        .entry("intl")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(intl) = intl.as_object_mut() {
        intl.insert("accept_languages".into(), languages.into());
        intl.insert("selected_languages".into(), languages.into());
    }

    let translate = root
        .entry("translate")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(translate) = translate.as_object_mut() {
        translate.insert("enabled".into(), serde_json::Value::Bool(false));
    }

    std::fs::write(&path, serde_json::to_string(&prefs)?)
        .with_context(|| format!("writing {}", path.display()))?;
    debug!("Seeded {} with languages '{}'", path.display(), languages);
    Ok(())
}

/// Is something already listening on the debugging port?
///
/// A plain TCP connect rather than an HTTP probe: the only thing we need to know
/// is whether to start a browser, and anything holding that port means we must
/// not — starting a second one would just fail to bind and fight the first.
async fn cdp_reachable(cdp_url: &str) -> bool {
    let Ok(port) = debugging_port(cdp_url) else {
        return false;
    };
    matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await,
        Ok(Ok(_))
    )
}

/// Per-instance defaults, keyed off the debugging port.
///
/// Two controllers on one machine already need different CDP ports, so deriving
/// from it makes a second display work without any extra configuration — and
/// makes the collision that would otherwise be silent impossible.
pub fn user_data_dir(args: &Args, port: u16) -> PathBuf {
    args.chromium_user_data_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/miniclientcontrol-chromium-{}", port)))
}

pub fn window_class(args: &Args, port: u16) -> String {
    args.chromium_class
        .clone()
        .unwrap_or_else(|| format!("miniclientcontrol-{}", port))
}

fn spawn(args: &Args) -> Result<Child> {
    let executable = detect_executable(args.chromium.as_deref())?;
    let port = debugging_port(&args.cdp_url)?;
    let profile = user_data_dir(args, port);
    write_preferences(&profile, &args.browser_language)?;

    let mut command = Command::new(&executable);
    command
        .arg(format!("--remote-debugging-port={}", port))
        .arg(format!("--user-data-dir={}", profile.display()))
        // WM_CLASS's second field, which is what i3's `class` matcher reads.
        .arg(format!("--class={}", window_class(args, port)))
        .args(BASE_ARGS);

    if !args.no_kiosk {
        command.arg("--kiosk");
    }
    for extra in &args.chromium_arg {
        command.arg(extra);
    }
    // Something has to be open for CDP to attach to; the control loop navigates
    // away from it immediately.
    command.arg("about:blank");

    // Not `kill_on_drop`: restarting the controller (a deploy, a crash) should not
    // blank the screen. The next start finds the CDP port answering and simply
    // reconnects to the browser that is already up.
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    info!(
        "Starting {} on debugging port {} (class {}, profile {})",
        executable.display(),
        port,
        window_class(args, port),
        profile.display()
    );
    command.spawn().context("spawning the browser")
}

/// Keep a browser available on the CDP port for the control loop to drive.
pub async fn supervise(args: std::sync::Arc<Args>, pid_slot: PidSlot) {
    let mut child: Option<Child> = None;

    loop {
        // Reap our own child so a crashed browser is noticed rather than lingering.
        if let Some(running) = child.as_mut() {
            match running.try_wait() {
                Ok(Some(status)) => {
                    warn!("Browser exited ({}), restarting", status);
                    child = None;
                    *pid_slot.lock().await = None;
                }
                Ok(None) => {}
                Err(e) => {
                    error!("Failed to poll the browser process: {}", e);
                    child = None;
                    *pid_slot.lock().await = None;
                }
            }
        }

        if child.is_none() && !cdp_reachable(&args.cdp_url).await {
            match spawn(&args) {
                Ok(spawned) => {
                    *pid_slot.lock().await = spawned.id();
                    child = Some(spawned);
                }
                // Keep retrying rather than giving up: on a slow boot the display
                // server may simply not be ready yet.
                Err(e) => error!("Could not start the browser: {:#}", e),
            }
        }

        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}
