use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::sync::{Mutex, RwLock};
use clap::{Parser, ValueEnum};
use std::path::PathBuf;

// --- Config ---

#[derive(Parser, Clone, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Port for the plain-HTTP server
    #[arg(long, env, default_value_t = 3000)]
    pub port: u16,

    /// Address the plain-HTTP server binds to.
    ///
    /// Loopback by default: that listener exists for the display browser, which
    /// fetches assets from `http://127.0.0.1` and treats it as a secure context
    /// anyway. Everything a human touches goes over TLS, so basic-auth
    /// credentials never cross the network in the clear. Set to `0.0.0.0` only
    /// if something outside this device really needs the unencrypted API.
    #[arg(long, env, default_value = "127.0.0.1")]
    pub http_listen: std::net::IpAddr,

    /// Directory to store uploaded assets
    #[arg(long, env, default_value = "./assets")]
    pub assets_dir: PathBuf,

    /// Path to SQLite database file
    #[arg(long, env, default_value = "miniclient.db")]
    pub database_path: String,

    /// Chrome DevTools Protocol URL
    #[arg(long, env, default_value = "http://127.0.0.1:9222")]
    pub cdp_url: String,

    /// A screen this deployment drives, as `name` or `name:cdp-port`.
    ///
    /// Repeat it once per screen. The name is the identity an operator's
    /// playlist assignment is stored against, and it is what the window-manager
    /// config matches on — `miniclientcontrol-<name>` becomes the Wayland
    /// `app_id`. Passing none keeps the single-display behaviour exactly as it
    /// was, which is why this is a `Vec` with no clap default rather than an
    /// `Option`.
    #[arg(long = "display", env = "DISPLAYS", value_delimiter = ',')]
    pub display: Vec<String>,

    /// Basic auth username (set together with basic_auth_password)
    #[arg(long, env)]
    pub basic_auth_user: Option<String>,

    /// Basic auth password (set together with basic_auth_user)
    #[arg(long, env)]
    pub basic_auth_password: Option<String>,

    /// Disable screen casting entirely (no HTTPS listener, no signaling endpoints)
    #[arg(long, env, default_value_t = false)]
    pub disable_cast: bool,

    /// Port for the HTTPS listener that serves the cast sender page.
    ///
    /// Screen sharing needs a secure context, which plain HTTP on a LAN address
    /// is not. Left unset, the listener takes 3443 or the next free port after
    /// it; set explicitly, a clash is a startup error instead.
    #[arg(long, env)]
    pub cast_tls_port: Option<u16>,

    /// Path to the self-signed cast certificate (generated if missing)
    #[arg(long, env, default_value = "cast-cert.pem")]
    pub cast_cert_path: PathBuf,

    /// Extra hostnames/IPs to put in the cast certificate. Repeatable.
    #[arg(long, env, value_delimiter = ',')]
    pub cast_cert_san: Vec<String>,

    /// How a sender proves it is allowed to cast.
    ///
    /// Unset means the stored setting applies and the admin UI may change it;
    /// passing it pins the value and locks that control.
    #[arg(long, env, value_enum)]
    pub cast_auth: Option<CastAuth>,

    /// Fixed pairing code for --cast-auth=code. Pins the stored value when given.
    #[arg(long, env)]
    pub cast_code: Option<String>,

    /// Do not start a browser; only connect to one that is already running.
    #[arg(long, env, default_value_t = false)]
    pub no_launch_browser: bool,

    /// Path to the Chrome/Chromium binary. Autodetected when unset.
    #[arg(long, env)]
    pub chromium: Option<PathBuf>,

    /// Profile directory for the browser we start. Rewritten on every launch.
    ///
    /// Defaults to `/tmp/miniclientcontrol-chromium-<cdp-port>`. It **must** differ
    /// between instances on one machine: a second Chromium started on a profile
    /// that is already in use hands its URL to the running one and exits, taking
    /// its debugging port with it — so the second display would silently never
    /// come up.
    #[arg(long, env)]
    pub chromium_user_data_dir: Option<PathBuf>,

    /// `WM_CLASS` of the browser window, for window-manager placement rules.
    ///
    /// Defaults to `miniclientcontrol-<cdp-port>`. With two displays this is what
    /// an i3 `assign [class="..."] <workspace>` rule matches on.
    #[arg(long, env)]
    pub chromium_class: Option<String>,

    /// Do not start the browser in kiosk mode (useful when testing on a desktop)
    #[arg(long, env, default_value_t = false)]
    pub no_kiosk: bool,

    /// Extra browser flags, repeatable (e.g. --chromium-arg=--ozone-platform=wayland)
    #[arg(long, env)]
    pub chromium_arg: Vec<String>,

    /// Language tag for the dates and times the display shows, e.g. `de-DE`.
    ///
    /// Distinct from `--browser-language`, which decides what websites are asked
    /// to serve. Unset, the stored setting applies, and failing that the tag is
    /// taken from `LC_ALL`, `LC_TIME` or `LANG`.
    #[arg(long, env)]
    pub locale: Option<String>,

    /// Languages written into the browser profile.
    ///
    /// Chromium offers to translate a page whose language is not in this list, and
    /// that bubble cannot be suppressed by a flag on Linux — so the fix is to make
    /// the list match what the signage actually shows.
    #[arg(long, env, default_value = "de,de-DE,en-US,en")]
    pub browser_language: String,

    /// How guests reach this device, when it is not simply its LAN address.
    ///
    /// * `none`    — use the primary IPv4 address (default)
    /// * `mdns`    — use `<hostname>.local`, which needs Avahi on the device
    /// * anything else — used literally, either as a host (`signage.example.com`)
    ///   or as a full base URL (`https://signage.example.com`) for a device that
    ///   sits behind a proxy
    #[arg(long, env, default_value = "none")]
    pub public_url: String,

    /// Whether to take a real certificate for a name derived from the LAN
    /// address, instead of generating a self-signed one.
    ///
    /// Only applies when `--public-url` is `none`: any other setting is a name
    /// the operator chose, and second-guessing it would be wrong. When it
    /// applies, the device is advertised as `<address>.clientctrl.cc`, whose
    /// public DNS resolves straight back to the private address, and the
    /// matching wildcard certificate is fetched and kept fresh. Guests then get
    /// no warning page at all.
    ///
    /// `off` keeps the self-signed certificate and the bare address. Worth
    /// reaching for on a device that must not talk to anything outside the LAN,
    /// since `auto` contacts the certificate API at startup.
    #[arg(long, env, default_value = "auto", value_parser = ["auto", "off"])]
    pub managed_cert: String,

    /// Whether guests may put a web page on the display instead of casting.
    ///
    /// Off unless said otherwise: this decides whether strangers on the LAN can
    /// place content on the venue's screen. Passing the flag pins the setting,
    /// so a deployment can nail it down and the admin UI shows it as locked --
    /// which is why there is no clap default. `None` has to mean "not given".
    #[arg(long, env, value_parser = ["on", "off"])]
    pub guest_pages: Option<String>,

    /// Cap on the longest frame edge a cast may send here, in pixels.
    ///
    /// The display reports what it can *show* (panel size, GPU texture limit).
    /// Neither says anything about whether the CPU can decode that in real time,
    /// and there is no signal in WebRTC for "my decoder is drowning" — so on weak
    /// hardware the operator has to say. A Raspberry Pi 2 holds 1080p for about
    /// two minutes before the sender gives up on it.
    #[arg(long, env)]
    pub cast_max_edge: Option<u32>,

    /// Optional STUN server for cast ICE. Only needed when host candidates on the
    /// LAN do not connect (mDNS `.local` candidates failing to resolve).
    #[arg(long, env)]
    pub cast_stun_url: Option<String>,
}

/// How the cast sender authenticates. Basic auth is deliberately not an option:
/// it would hand the operator password to every guest who wants to share a screen.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CastAuth {
    /// Anyone who can reach the page may cast. Appropriate on a trusted LAN.
    None,
    /// A fixed code from --cast-code, known out of band (label on the display).
    Code,
    /// A fresh code shown on the display for 30s when a sender asks to pair.
    Pairing,
}

// --- Models ---

#[derive(Debug, Serialize, Deserialize, FromRow, Clone)]
pub struct Asset {
    pub id: i64,
    pub filename: String,
    pub local_path: String,
    pub mimetype: String,
    #[sqlx(default)]
    pub duration: Option<i64>,
    pub created_at: Option<String>,
}

// --- Scroll Models ---

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", content = "options")]
pub enum ScrollMode {
    None,
    Step(StepOptions),
    Continuous(ScrollOptions),
}

impl Default for ScrollMode {
    fn default() -> Self {
        ScrollMode::None
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct OverrideItem {
    pub asset_id: Option<i64>,
    pub url: Option<String>,
    pub local_path: Option<String>,
    pub mimetype: Option<String>,
    pub scroll_config: ScrollMode,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct StepOptions {
    pub step_time: Option<u64>, // If None jump, else smooth duration in ms
    pub step_px: Option<u64>,   // If None viewport height
    pub step_delay: u64,        // ms wait between steps
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ScrollOptions {
    pub speed: f64,      // pixels per frame (approx 60fps)
    pub top_delay: u64,  // ms to wait at top before scrolling
    pub return_delay: u64, // ms to wait at bottom before returning to top
}

impl Default for StepOptions {
    fn default() -> Self {
        Self {
            step_time: Some(500),
            step_px: None,
            step_delay: 2000,
        }
    }
}

impl Default for ScrollOptions {
    fn default() -> Self {
        Self {
            speed: 1.0,
            top_delay: 2000,
            return_delay: 2000,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, FromRow, Clone)]
pub struct PlaylistItemWithAsset {
    pub id: i64,
    pub asset_id: Option<i64>,
    pub url: Option<String>,
    pub play_order: i64,
    pub duration: Option<i64>,
    pub enabled: bool,
    #[sqlx(default)]
    pub is_enabled: bool,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    #[sqlx(default)]
    pub keep_loaded: bool,

    /// Which playlist this item belongs to. `Option` because the column is added
    /// by migration and an item written by an older binary has none until the
    /// backfill runs.
    #[sqlx(default)]
    pub playlist_id: Option<i64>,

    // Serialized JSON stored in DB
    #[sqlx(default)]
    pub scroll_config: sqlx::types::Json<ScrollMode>,

    /// This item's own overlay, drawn in addition to the global one. `None` for
    /// the many items that do not want one.
    ///
    /// Only the API selects this column; the control loop reads the overlay fresh
    /// per item (`db::load_item_overlay`), because its playlist snapshot can be a
    /// whole item duration out of date.
    #[sqlx(default)]
    pub overlay_config: sqlx::types::Json<Option<crate::settings::ItemOverlay>>,

    // Asset fields
    pub local_path: Option<String>,
    pub mimetype: Option<String>,
    #[sqlx(default)]
    pub asset_duration: Option<i64>,

    /// The asset's original upload name, which is what a webhook calls the item.
    /// `local_path` is a sanitised derivative and reads badly in a notification.
    #[sqlx(default)]
    pub filename: Option<String>,
}

// --- Application State ---

/// One screen's playback state.
///
/// These were fields on `AppState` when there was one screen. They are the only
/// things that had to become per-display: everything else the controller owns —
/// assets, settings, the overlay, credentials, audio, webhook targets — is
/// shared, because duplicating *configuration* was never the problem.
pub struct Display {
    pub name: String,
    pub cdp_url: String,
    /// What this display's loop is currently showing. Owned by the loop; the API only reads it.
    pub current_item_id: Mutex<Option<i64>>,
    /// "Play now" for this display. Written by the API; cleared by the loop only once the
    /// target has been looked up in a freshly fetched playlist. It must survive a lookup
    /// miss: the loop's playlist snapshot can be a whole item duration stale, so an item
    /// added or re-enabled since the last fetch is not in it yet. Kept separate from
    /// `current_item_id`, which the loop overwrites at the start of every item and would
    /// therefore clobber the request.
    pub pending_jump: Mutex<Option<i64>>,
    pub override_item: Mutex<Option<OverrideItem>>,
    /// PID of the browser we launched for this display, when we launched it. Used to
    /// tell the cast's own audio stream apart from everything else making sound.
    ///
    /// An `Arc` rather than a bare `Mutex` because `chromium::supervise` is handed the
    /// slot itself and outlives no particular borrow of the `Display`.
    pub browser_pid: crate::chromium::PidSlot,
    /// This screen's cast session. One per display: two guests casting to two
    /// screens share no state, no timers and no reservation. `attempts` is the
    /// deliberate exception and stays on `AppState` -- see `CastSession`.
    pub cast: crate::cast::SharedCastSession,
    pub skip_signal: Notify,
    pub playlist_signal: Notify,
    pub override_signal: Notify,
    /// Poked when the overlay configuration changes, so the badge appears on the
    /// item that is already on screen instead of at the next navigation.
    pub overlay_signal: Notify,
}

impl Display {
    pub fn new(name: &str, cdp_url: &str) -> Self {
        Self {
            name: name.to_string(),
            cdp_url: cdp_url.to_string(),
            current_item_id: Mutex::new(None),
            pending_jump: Mutex::new(None),
            override_item: Mutex::new(None),
            browser_pid: Default::default(),
            cast: Default::default(),
            skip_signal: Notify::new(),
            playlist_signal: Notify::new(),
            override_signal: Notify::new(),
            overlay_signal: Notify::new(),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub pool: sqlx::SqlitePool,
    pub args: Arc<Args>,
    /// The screens this deployment drives, in declaration order. Built once at
    /// startup and never changed, so nothing guards the list itself — only the
    /// fields inside each `Display`.
    pub displays: Arc<Vec<Arc<Display>>>,
    /// The port the cast listener actually bound, which is not necessarily the
    /// one in `args` — see `tls::bind_cast_listener`.
    pub cast_tls_port: u16,
    /// Whether a managed certificate is in use, which decides what name guests
    /// are given. The name itself is *not* cached: it follows the machine's
    /// current address, and the wildcard covers whatever that turns into.
    pub managed_cert: bool,
    /// Operator-editable configuration. Read on nearly every request, so it sits
    /// behind its own `RwLock` rather than inside the cast session's mutex,
    /// which is held across signaling work.
    pub settings: Arc<RwLock<crate::settings::AppSettings>>,
    /// Which settings the command line pinned. Fixed for the process lifetime.
    pub locks: crate::settings::Locks,
    /// The last `Authorization` header that verified successfully.
    ///
    /// Stored passwords are PBKDF2 hashes, which are deliberately slow; the
    /// operator UI polls every two seconds, so verifying every request would
    /// burn real time on a Pi. Cleared whenever the credentials change.
    pub auth_cache: Arc<Mutex<Option<String>>>,
    /// How the venue's audio is controlled, decided once at startup.
    pub audio: Arc<crate::audio::Backend>,
    /// Which screen's cast currently owns the room audio, if any.
    ///
    /// The venue has one speaker pair, so audio is one resource however many
    /// screens are casting. Claimed by the first cast to turn sound on and
    /// released on that cast's teardown. Validated against that display's
    /// `is_active()` on every read, so a cast that dies without a clean teardown
    /// frees the audio by itself rather than leaving the room mute until a
    /// restart.
    pub audio_owner: Arc<Mutex<Option<String>>>,
    /// Failed pairing-code attempts per source address, controller-wide.
    ///
    /// Deliberately not per display: five tries is five tries for the venue, not
    /// five per screen. `tests/cast/test_pairing.py` case [8] is what pins this.
    pub cast_attempts:
        Arc<Mutex<std::collections::HashMap<std::net::IpAddr, crate::cast::Attempts>>>,
    /// Outbound webhooks. `fire` is synchronous and infallible, which is what
    /// lets the control loop call it.
    pub webhooks: Arc<crate::webhook::Dispatcher>,
}

impl AppState {
    /// The display of that name, if this deployment declares one.
    ///
    /// Resolved out of a request path by the display-scoped API
    /// (`display::resolve`), which is also where an unknown name turns into a
    /// `404` listing the real ones.
    pub fn display(&self, name: &str) -> Option<Arc<Display>> {
        self.displays.iter().find(|d| d.name == name).cloned()
    }

    /// The first declared display. What an unscoped legacy API path resolves to
    /// when only one display exists, and the fallback for anything that has to
    /// name one screen without being told which.
    ///
    /// The index cannot panic: `display::configure` returns at least the
    /// implicit `default` display, so the list is never empty.
    pub fn primary(&self) -> Arc<Display> {
        self.displays[0].clone()
    }

    /// Tell every display that the playlist content changed.
    ///
    /// Not display-scoped, unlike playback: an edited item can be on any screen
    /// showing the playlist it belongs to, and a loop that was not told carries
    /// a stale snapshot until its next pass. Poking a display that does not show
    /// the item costs one extra read of a table with single-digit rows.
    ///
    /// `notify_one` and never `notify_waiters`: a loop is only parked for part
    /// of its cycle, and `notify_waiters` drops the notification when nobody is
    /// parked, which is a silently lost edit.
    pub fn notify_playlist_changed(&self) {
        for display in self.displays.iter() {
            display.playlist_signal.notify_one();
        }
    }

    /// Tell every display that an overlay changed.
    ///
    /// The global overlay is the building's, not one panel's, and an item's own
    /// overlay travels with the item wherever it is shown. Both reach every
    /// screen for the same reason, so both come through here.
    pub fn notify_overlay_changed(&self) {
        for display in self.displays.iter() {
            display.overlay_signal.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_display_starts_with_nothing_playing() {
        let display = Display::new("foyer", "http://127.0.0.1:9222");
        assert_eq!(display.name, "foyer");
        assert!(display.current_item_id.try_lock().unwrap().is_none());
        assert!(display.override_item.try_lock().unwrap().is_none());
    }
}
