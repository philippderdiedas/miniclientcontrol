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
    
    // Serialized JSON stored in DB
    #[sqlx(default)]
    pub scroll_config: sqlx::types::Json<ScrollMode>,

    // Asset fields
    pub local_path: Option<String>,
    pub mimetype: Option<String>,
    #[sqlx(default)]
    pub asset_duration: Option<i64>,
}

// --- Application State ---

#[derive(Clone)]
pub struct AppState {
    pub pool: sqlx::SqlitePool,
    pub args: Arc<Args>,
    pub skip_signal: Arc<Notify>,
    pub playlist_signal: Arc<Notify>,
    pub override_signal: Arc<Notify>,
    /// What the browser loop is currently showing. Owned by the loop; the API only reads it.
    pub current_item_id: Arc<Mutex<Option<i64>>>,
    /// "Play now" request. Written by the API; cleared by the loop only once the target
    /// has been looked up in a freshly fetched playlist. It must survive a lookup miss:
    /// the loop's playlist snapshot can be a whole item duration stale, so an item added
    /// or re-enabled since the last fetch is not in it yet. Kept separate from
    /// `current_item_id`, which the loop overwrites at the start of every item and would
    /// therefore clobber the request.
    pub pending_jump: Arc<Mutex<Option<i64>>>,
    pub override_item: Arc<Mutex<Option<OverrideItem>>>,
    /// The port the cast listener actually bound, which is not necessarily the
    /// one in `args` — see `tls::bind_cast_listener`.
    pub cast_tls_port: u16,
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
    /// PID of the browser we launched, when we launched it. Used to tell the
    /// cast's own audio stream apart from everything else making sound.
    pub browser_pid: Arc<Mutex<Option<u32>>>,
    /// Screen-cast session. A running cast owns `override_item`; see `cast.rs`.
    pub cast: crate::cast::SharedCastSession,
}