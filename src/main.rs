mod frames;
mod builtin;
mod layout;
mod screenshot;
mod freeze;
mod oidc;
mod advance;
mod db;
mod display;
mod handlers;
mod models;
mod browser;
mod web;
mod guest_page;
mod managed_cert;
mod tls;
mod cast;
mod settings;
mod chromium;
mod mdns;
mod audio;
mod webhook;
mod playlists;
mod schedule;
mod accounts;
mod proposals;
mod mcp;

use anyhow::Result;
use axum::{
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, put}, // Only get and put are used as starting points
    Router,
};
use clap::Parser;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::{cors::CorsLayer, services::ServeDir};
use models::{AppState, Args};
use handlers::{
    list_assets, upload_asset, update_asset, delete_asset,
    get_playlist, add_to_playlist, update_playlist_item, delete_playlist_item,
    move_playlist_item, get_current, set_current, get_override, set_override, clear_override
};
use browser::browser_loop;
use web::serve_embedded_ui;



/// Paths the *display* browser fetches. Chromium runs on this device over CDP and
/// has no way to present credentials, so requiring auth here blanks the signage.
/// These stay reachable without auth, but only from loopback.
pub(crate) fn is_display_path(path: &str) -> bool {
    // A layout's widgets, which only the display browser asks for: the page it
    // builds from them is in the list below.
    if path.starts_with("/uploads/") || path.starts_with("/api/layout/") {
        return true;
    }
    matches!(
        path,
        "/pdf_viewer.html"
            | "/media_viewer.html"
            | "/pdf.min.js"
            | "/pdf.worker.min.js"
            | "/autoscroll.js"
            | "/no_content.svg"
            | "/empty_playlist.html"
            | "/layout.html"
            | "/widget.html"
            | "/logo.svg"
            // the cast display page reads this to show the sender's HTTPS address;
            // remote operators still need credentials for it
            | "/api/cast/state"
    )
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
    // `foreign_keys` is OFF by default in SQLite, so ON DELETE CASCADE only works
    // if we turn it on for every pooled connection.
    let connect_options = SqliteConnectOptions::from_str(&format!("sqlite:{}", args.database_path))?
        .create_if_missing(true)
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(connect_options)
        .await?;

    // Schema lives entirely in db::run_migrations so there is a single source of truth.
    db::run_migrations(&pool).await?;
    // Before anything reads an account: a device upgrading from the single
    // basic-auth credential keeps it, as its first admin.
    accounts::adopt_stored_credential(&pool).await?;

    // Stored settings, with anything passed on the command line winning.
    let app_settings = settings::load(&pool, &args).await;
    let locks = settings::Locks::from_args(&args);
    // Not a hard failure: casting must never keep the signage from booting, and
    // an operator can now fix this in the admin UI without a restart. Senders are
    // refused with an explicit reason until then.
    if !args.disable_cast
        && app_settings.cast_auth == models::CastAuth::Code
        && app_settings.cast_code.is_empty()
    {
        tracing::error!(
            "Cast auth is set to 'code' but no code is configured -- every sender \
             will be refused until one is set in the admin UI or via --cast-code"
        );
    }

    if args.basic_auth_user.is_some() != args.basic_auth_password.is_some() {
        anyhow::bail!("Both --basic-auth-user and --basic-auth-password must be set together");
    }

    // Bound before anything else needs the port: a clash has to surface as a
    // startup failure, and the resolved port feeds the URLs handed to guests.
    let cast_listener = if args.disable_cast {
        None
    } else {
        tls::check_mdns(&args.public_url).await;
        let listener = tls::bind_cast_listener(args.cast_tls_port)?;
        listener.set_nonblocking(true)?;
        Some(listener)
    };
    let cast_tls_port = cast_listener
        .as_ref()
        .and_then(|listener| listener.local_addr().ok())
        .map(|addr| addr.port())
        .unwrap_or(tls::DEFAULT_CAST_TLS_PORT);

    // Resolved here, before AppState exists, because the name guests are given
    // depends on whether this succeeded -- and every URL the API hands out is
    // built from that answer.
    //
    // Only for `--public-url none`: anything else is a name the operator chose.
    let managed = if cast_listener.is_some()
        && args.managed_cert == "auto"
        && matches!(tls::public_url(&args.public_url), tls::PublicUrl::LanAddress)
    {
        match tls::managed_name() {
            Some(name) => {
                // Log the mapping, not just the name: when a guest's resolver refuses
                // it, the first question is always which address it should answer with.
                tracing::info!("Advertising this device as {} -> {}", name.host, name.addr);
                // A hint, not a verdict -- see the note on the function.
                tls::check_managed_name(&name).await;
                managed_cert::obtain(&args.cast_cert_path, &name.host)
                    .await
                    .map(|bundle| (name, bundle))
            }
            None => {
                tracing::info!(
                    "No private IPv4 and no usable IPv6 address; keeping the \
                     self-signed certificate and the bare LAN address"
                );
                None
            }
        }
    } else {
        None
    };
    let managed_cert_active = managed.is_some();

    // Resolved before `AppState` exists, because the per-display playback state
    // is built from it. A bad `--display` exits rather than degrading: a typo
    // that silently dropped a screen would show up as a black panel in a venue,
    // with nothing saying why.
    let configured = match display::configure(&args) {
        Ok(configured) => configured,
        Err(message) => {
            tracing::error!("{}", message);
            std::process::exit(1);
        }
    };
    tracing::info!(
        "Driving {} display(s): {}",
        configured.len(),
        configured
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let displays: Vec<Arc<models::Display>> = configured
        .iter()
        .map(|c| Arc::new(models::Display::new(&c.name, &c.cdp_url)))
        .collect();

    // Every declared display needs a row before any loop reads its assignment:
    // nothing else writes this table, so without it a fresh upgrade would have
    // every screen unassigned and therefore idle. Placed here rather than in
    // `db::run_migrations` because it is not schema -- it depends on what this
    // command line declared, which the migration has no business knowing.
    display::register(&pool, &configured).await?;

    // 3. Init State
    let state = AppState {
        pool: pool.clone(),
        args: Arc::new(args.clone()),
        displays: Arc::new(displays),
        cast_attempts: Arc::new(Mutex::new(Default::default())),
        cast_tls_port,
        managed_cert: managed_cert_active,
        settings: Arc::new(tokio::sync::RwLock::new(app_settings)),
        locks,
        login_attempts: Arc::new(Mutex::new(Default::default())),
        basic_cache: Arc::new(Mutex::new(Default::default())),
        oidc: Arc::new(tokio::sync::RwLock::new(crate::oidc::config::load(&pool).await)),
        oidc_pending: Arc::new(Mutex::new(Default::default())),
        rescue: args.basic_auth_user.clone().zip(args.basic_auth_password.clone()),
        router: Default::default(),
        audio: Arc::new(audio::Backend::detect().await),
        audio_owner: Arc::new(Mutex::new(None)),
        webhooks: Arc::new(webhook::Dispatcher::new(pool.clone())),
    };

    // A custom `.local` name has to be announced; Avahi only does the hostname.
    let mdns_args = state.args.clone();
    tokio::spawn(async move { mdns::supervise(mdns_args).await });

    // Keep a browser alive on each display's CDP port. Skipped when something
    // else manages them (an existing sway `exec` line), which the supervisor
    // detects by finding the port already answering.
    if !args.no_launch_browser {
        // `state.displays` is built by mapping over `configured` a few lines
        // up, so the two are the same length by construction today -- but
        // construction is not a proof that survives a later edit, and a zip
        // that silently drops the tail of the longer side would leave a
        // display with no supervisor and nothing saying why.
        debug_assert_eq!(
            configured.len(),
            state.displays.len(),
            "state.displays should be built by mapping over configured"
        );
        for (config, display) in configured.iter().zip(state.displays.iter()) {
            let browser_args = state.args.clone();
            let config = config.clone();
            let pid_slot = display.browser_pid.clone();
            let restart = display.browser_restart.clone();
            tokio::spawn(async move { chromium::supervise(browser_args, config, pid_slot, restart).await });
        }
    }

    // 4. One control loop per display. Each owns its own screen and reads the
    // playlist that screen is assigned; nothing is shared between them but the
    // database, the settings and the webhook dispatcher.
    for display in state.displays.iter() {
        let loop_state = state.clone();
        let loop_display = display.clone();
        tokio::spawn(async move {
            browser_loop(loop_state, loop_display).await;
        });
        // Its own task, not a branch of the loop: the loop parks in several
        // places for long stretches, and a hung compositor is exactly when it
        // would not come round to check.
        let watcher_state = state.clone();
        let watched = display.clone();
        tokio::spawn(async move { freeze::watch(watcher_state, watched).await });
        // Its own raw CDP connection, so unlocking layout frames never sits in
        // the path of the control loop's commands.
        let frames_state = state.clone();
        let frames_cdp = display.cdp_url.clone();
        tokio::spawn(async move { frames::run(frames_state, frames_cdp).await });
    }

    // 5. Start Web Server
    let uploaded_assets_dir = args.assets_dir.clone();
    let serve_dir = ServeDir::new(&uploaded_assets_dir);

    let app = Router::new()
        .route("/api/assets", get(list_assets).post(upload_asset))
        .route("/api/assets/{id}", put(update_asset).delete(delete_asset)) 
        .route("/api/playlist", get(get_playlist).post(add_to_playlist))
        .route("/api/playlist/{id}", put(update_playlist_item).delete(delete_playlist_item))
        .route("/api/playlist/{id}/move", axum::routing::post(move_playlist_item))
        .route("/api/playlist/{id}/duplicate", axum::routing::post(handlers::duplicate_playlist_item))
        .route("/api/layout/{id}", get(layout::widgets_for_display))
        .route("/api/control/current", get(get_current).post(set_current))
        .route(
            "/api/override",
            get(get_override).post(set_override).delete(clear_override),
        )
        .merge(cast::routes())
        .merge(settings::routes())
        .merge(audio::routes())
        .merge(playlists::routes())
        .merge(display::routes())
        .nest_service("/uploads", serve_dir)
        .fallback(serve_embedded_ui)
        .layer(DefaultBodyLimit::max(1024 * 1024 * 500)) 
        .layer(CorsLayer::permissive())
        // Merged *after* the CORS layer, which in axum applies only to the
        // routes registered before it, so the webhook API answers no
        // cross-origin read. Nothing in this repository needs it: every page
        // under `web/` fetches its own origin with a relative path, and the
        // cast socket is a WebSocket, which CORS does not govern. The rest of
        // the operator API stays permissive because that is what it has always
        // been -- the difference here is that a target's headers hold a *third
        // party's* credential, which unlike `cast_code` is worth something off
        // this network, so a page the operator happens to visit must not be
        // able to read it back. This is not an access-control boundary and does
        // not pretend to be one: configure basic auth, as
        // docs/deployment.md says. Being merged last also leaves these routes
        // on axum's default body limit rather than the 500 MB an upload needs,
        // which is the right way round: a target is a few kilobytes of
        // template.
        .merge(webhook::api::routes())
        .merge(accounts::api::routes())
        .merge(oidc::api::routes())
        .merge(proposals::api::routes())
        // After the CORS layer too: no page on another site has any business
        // with it, and a browser's preflight for its JSON then fails. Its own
        // body limit, sized for an upload carried as base64.
        .merge(mcp::routes())
        .with_state(state.clone());

    let app = app.layer(middleware::from_fn_with_state(
        state.clone(),
        accounts::middleware::auth_middleware,
    ));
    // Kept for replaying an approved proposal through the very same routes,
    // middleware included.
    let _ = state.router.set(app.clone());

    // The cast sender page needs HTTPS (secure context), everything else is happy
    // over plain HTTP. Both listeners serve the *same* Router and the same AppState,
    // so a sender on the TLS origin and the display on loopback meet in one signaling
    // registry, and the sender never fetches across origins -- which a page served
    // over TLS could not do towards a plain-HTTP API anyway.
    if let Some(listener) = cast_listener {
        // A name guests are told to type has to be in the certificate too, or
        // they get a name mismatch on top of the unknown-issuer warning. A
        // managed certificate already covers its name by construction, and its
        // SANs are not ours to choose, so that path skips the generator whole.
        let loaded = match &managed {
            Some((_, bundle)) => axum_server::tls_rustls::RustlsConfig::from_pem(
                bundle.fullchain.clone().into_bytes(),
                bundle.key.clone().into_bytes(),
            )
            .await
            .map_err(|e| anyhow::anyhow!("loading the managed certificate into rustls: {e}")),
            None => {
                let mut cert_names = args.cast_cert_san.clone();
                if let Some(host) = tls::public_host(&args.public_url) {
                    cert_names.push(host);
                }
                tls::load_cast_tls(&args.cast_cert_path, &cert_names).await
            }
        };
        match loaded {
            Ok(config) => {
                if let Some((name, bundle)) = managed {
                    managed_cert::spawn_renewal(
                        config.clone(),
                        args.cast_cert_path.clone(),
                        name.host,
                        bundle,
                    );
                }
                // Marked, so a session cookie set over TLS is `Secure`.
                let tls_app = app.clone().layer(axum::Extension(accounts::middleware::ViaTls));
                // Must go through the same resolution the API and the QR code
                // use, or the first thing an operator reads on startup disagrees
                // with the address guests are actually given.
                let base = cast::sender_url(&state, None);
                tracing::info!("Guests: {base}  |  Operator: {base}admin.html");
                let server = axum_server::from_tcp_rustls(listener, config)?;
                tokio::spawn(async move {
                    // ConnectInfo here too: without it the loopback exemption in
                    // auth_middleware panics on the extractor for TLS requests.
                    if let Err(e) = server
                        .serve(tls_app.into_make_service_with_connect_info::<SocketAddr>())
                        .await
                    {
                        tracing::error!("Cast HTTPS listener stopped: {}", e);
                    }
                });
            }
            // A certificate problem must never take the signage down with it --
            // the playlist does not need TLS, only screen casting does. A port
            // clash is different and already failed above: that one usually means
            // a second copy of this binary is running.
            Err(e) => tracing::error!("Cast HTTPS listener disabled: {:#}", e),
        }
    }

    // Loopback by default -- see Args::http_listen. The display browser is the
    // only intended client of this listener.
    let addr = SocketAddr::new(args.http_listen, args.port);
    if addr.ip().is_loopback() {
        tracing::info!("Local HTTP (display browser) on {}", addr);
    } else {
        tracing::warn!(
            "Plain HTTP is exposed on {} -- basic-auth credentials sent to it \
             travel unencrypted",
            addr
        );
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    // ConnectInfo is required by auth_middleware to recognise loopback peers.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_media_viewer_is_reachable_by_the_display_browser() {
        // The display browser is driven over CDP and presents no credentials; a
        // page it loads that is not here is a 401 on every screen the moment
        // basic auth is configured.
        assert!(super::is_display_path("/media_viewer.html"));
    }
}
