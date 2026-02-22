mod db;
mod handlers;
mod models;
mod browser;
mod web;

use anyhow::Result;
use axum::{
    extract::State,
    extract::DefaultBodyLimit,
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, put}, // Only get and put are used as starting points
    Router,
};
use clap::Parser;
use sqlx::sqlite::SqlitePoolOptions;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tower_http::{cors::CorsLayer, services::ServeDir};
use models::{AppState, Args};
use base64::Engine;
use handlers::{
    list_assets, upload_asset, update_asset, delete_asset,
    get_playlist, add_to_playlist, update_playlist_item, delete_playlist_item,
    get_current, set_current, set_override, clear_override
};
use browser::browser_loop;
use web::serve_embedded_ui;

#[derive(Clone)]
struct BasicAuthConfig {
    expected_header: String,
}

async fn basic_auth_middleware(
    State(auth): State<BasicAuthConfig>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let authorized = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(|value| value == auth.expected_header)
        .unwrap_or(false);

    if authorized {
        return next.run(request).await;
    }

    let mut response = StatusCode::UNAUTHORIZED.into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        header::HeaderValue::from_static("Basic realm=\"Mini Client Control\", charset=\"UTF-8\""),
    );
    response
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // 1. Setup Environment
    if !args.assets_dir.exists() {
        tokio::fs::create_dir_all(&args.assets_dir).await?;
    }

    // 2. Setup Database
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(format!("sqlite:{}?mode=rwc", args.database_path).as_str())
        .await?;
    
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS assets (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            filename    TEXT NOT NULL,
            local_path  TEXT NOT NULL UNIQUE,
            mimetype    TEXT NOT NULL,
            duration    INTEGER DEFAULT 10,
            created_at  DATETIME DEFAULT CURRENT_TIMESTAMP
        );"
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS playlist_items (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            asset_id    INTEGER,
            url         TEXT,
            play_order  INTEGER NOT NULL,
            duration    INTEGER,
            is_enabled  BOOLEAN DEFAULT 1,
            start_date  TEXT,
            end_date    TEXT,
            FOREIGN KEY(asset_id) REFERENCES assets(id) ON DELETE CASCADE
        );"
    )
    .execute(&pool)
    .await?;
    
    // Run migrations (add new columns)
    db::run_migrations(&pool).await?;

    let basic_auth_config = match (&args.basic_auth_user, &args.basic_auth_password) {
        (Some(user), Some(password)) => {
            let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", user, password));
            Some(BasicAuthConfig {
                expected_header: format!("Basic {}", encoded),
            })
        }
        (None, None) => None,
        _ => anyhow::bail!("Both --basic-auth-user and --basic-auth-password must be set together"),
    };

    // 3. Init State
    let state = AppState {
        pool: pool.clone(),
        args: Arc::new(args.clone()),
        skip_signal: Arc::new(Notify::new()),
        playlist_signal: Arc::new(Notify::new()),
        override_signal: Arc::new(Notify::new()),
        current_item_id: Arc::new(Mutex::new(None)),
        override_item: Arc::new(Mutex::new(None)),
    };

    // 4. Spawn Browser Controller Task
    let browser_state = state.clone();
    tokio::spawn(async move {
        browser_loop(browser_state).await;
    });

    // 5. Start Web Server
    let uploaded_assets_dir = args.assets_dir.clone();
    let serve_dir = ServeDir::new(&uploaded_assets_dir);

    let app = Router::new()
        .route("/api/assets", get(list_assets).post(upload_asset))
        .route("/api/assets/{id}", put(update_asset).delete(delete_asset)) 
        .route("/api/playlist", get(get_playlist).post(add_to_playlist))
        .route("/api/playlist/{id}", put(update_playlist_item).delete(delete_playlist_item))
        .route("/api/control/current", get(get_current).post(set_current))
        .route("/api/override", axum::routing::post(set_override).delete(clear_override))
        .nest_service("/uploads", serve_dir)
        .fallback(serve_embedded_ui)
        .layer(DefaultBodyLimit::max(1024 * 1024 * 500)) 
        .layer(CorsLayer::permissive())
        .with_state(state);

    let app = if let Some(auth) = basic_auth_config {
        tracing::info!("HTTP Basic Auth enabled");
        app.layer(middleware::from_fn_with_state(auth, basic_auth_middleware))
    } else {
        tracing::warn!("HTTP Basic Auth is disabled");
        app
    };

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], args.port));
    tracing::info!("Listening on {}", addr);
    
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
