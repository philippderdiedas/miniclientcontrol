use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::sync::Mutex;
use clap::Parser;
use std::path::PathBuf;

// --- Config ---

#[derive(Parser, Clone, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Port for the web server
    #[arg(long, env, default_value_t = 3000)]
    pub port: u16,

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
}

// --- Models ---

#[derive(Debug, Serialize, Deserialize, FromRow, Clone)]
pub struct Asset {
    pub id: i64,
    pub filename: String,
    pub local_path: String,
    pub mimetype: String,
    pub created_at: Option<String>,
}

// --- Scroll Models ---

#[derive(Serialize, Deserialize, Debug, Clone)]
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

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OverrideItem {
    pub asset_id: Option<i64>,
    pub url: Option<String>,
    pub local_path: Option<String>,
    pub mimetype: Option<String>,
    pub scroll_config: ScrollMode,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StepOptions {
    pub step_time: Option<u64>, // If None jump, else smooth duration in ms
    pub step_px: Option<u64>,   // If None viewport height
    pub step_delay: u64,        // ms wait between steps
}

#[derive(Serialize, Deserialize, Debug, Clone)]
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
    pub current_item_id: Arc<Mutex<Option<i64>>>,
    pub override_item: Arc<Mutex<Option<OverrideItem>>>,
}