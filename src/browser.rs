use std::sync::Arc;
use std::time::{Duration, Instant};
use std::collections::{HashMap, HashSet};
use chromiumoxide::{Browser, Page};
use chromiumoxide::cdp::browser_protocol::browser::{SetDownloadBehaviorBehavior, SetDownloadBehaviorParams};
use chromiumoxide::cdp::browser_protocol::page::{AddScriptToEvaluateOnNewDocumentParams, EnableParams as PageEnableParams, NavigateParams, ReloadParams, RemoveScriptToEvaluateOnNewDocumentParams, ScriptIdentifier};
use chromiumoxide::cdp::browser_protocol::target::{EventAttachedToTarget, SetAutoAttachParams, TargetInfo};
use chromiumoxide::cdp::js_protocol::runtime::EnableParams as RuntimeEnableParams;
use chromiumoxide::error::CdpError;
use futures::StreamExt;
use chromiumoxide::listeners::EventStream;
use serde_json::Value;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};
use crate::models::{AppState, Display, FitMode, OverrideItem, PlaylistItemWithAsset, ScrollMode};
use urlencoding::encode;

/// Drive one screen.
///
/// One task per declared display, each owning its own browser and its own
/// playback state. Nothing is shared between two of them but the database, the
/// settings and the webhook dispatcher -- which is why the edge-tracking locals
/// below are locals: a second screen gets its own copy for free.
pub async fn browser_loop(state: AppState, display: Arc<Display>) {
    // Bound out of the macro's reach: `tracing`'s own `display()` field helper is
    // in scope inside `info!`, so `display.name` there resolves to that function
    // and not to this local.
    let display_name = display.name.clone();
    let cdp_url = display.cdp_url.clone();
    info!("Starting browser loop for display '{}'...", display_name);

    // play_order of the last item we finished. The playlist is re-fetched whenever it
    // changes, and without this every edit (or reconnect) restarted playback at item
    // one — on a device whose playlist is touched regularly, later items never played.
    let mut resume_after_order: Option<i64> = None;

    // The assignment the pass before this one played, so that a reassignment can
    // be told from a plain re-read. A function-local like the edge-tracking
    // below, which is what makes it per display.
    let mut last_assigned: Option<i64> = None;

    // Edge-tracking for the two events that would otherwise repeat themselves.
    // The empty-playlist branch re-runs every five seconds for as long as the
    // playlist stays empty, and the connect path runs on every reconnect, so
    // firing from either unguarded would be a notification on a timer.
    let mut announced_empty = false;
    let mut connected_before = false;

    loop {
        // 1. Launch or Connect to Chrome
        // Deliberately permissive. Signage routinely points at internal dashboards
        // on self-signed certificates, and whoever adds a playlist URL is the one
        // deciding it is trustworthy -- there is no end user here to protect from
        // their own click. This is also chromiumoxide's own default.
        //
        // This cannot be made per item: `Security.setIgnoreCertificateErrors` sent
        // to the adopted control page has no effect at all -- the page still ends on
        // `chrome-error://chromewebdata/`. Only the handler setting takes, and it is
        // applied once, while each target is initialised.
        let handler_config = chromiumoxide::handler::HandlerConfig {
            ignore_https_errors: true,
            ..Default::default()
        };
        let (mut browser, mut handler) = match Browser::connect_with_config(
            &cdp_url,
            handler_config,
        )
        .await
        {
            Ok(res) => res,
            Err(_) => {
                info!("Could not launch browser, trying to connect to {}", cdp_url);
                sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        // Fired when this connection's handler ends -- the browser died, was
        // restarted after a freeze, or closed the socket. Without it the loop
        // noticed only at its next CDP call, which inside a ten-minute item is
        // ten minutes of a dead screen. One per connection, so a permit left
        // over from the last one cannot tear down the next.
        let lost = std::sync::Arc::new(tokio::sync::Notify::new());
        let lost_tx = lost.clone();

        // Spawn the handler needed for chromiumoxide
        let _handle = tokio::spawn(async move {
            while let Some(h) = handler.next().await {
                if h.is_err() {
                    break;
                }
            }
            lost_tx.notify_one();
        });

        // A URL need not be a page. Anything served with `Content-Disposition:
        // attachment` -- or any type Chromium will not render -- becomes a
        // *download*, and enough of those fill an SD card and take the database,
        // the certificate and the uploads with it. A guest who can set a URL
        // could do that on purpose.
        //
        // Browser-wide rather than per page, so it covers every target we ever
        // navigate, and unconditional rather than tied to any setting: a guard
        // against exhaustion that depends on a switch has the wrong shape, and a
        // signage display never wanted a download in the first place.
        //
        // `events_enabled` is what lets a guest be told their link was a file
        // rather than watching the screen not change.
        match SetDownloadBehaviorParams::builder()
            .behavior(SetDownloadBehaviorBehavior::Deny)
            .events_enabled(true)
            .build()
        {
            Ok(params) => {
                if let Err(e) = browser.execute(params).await {
                    // Not fatal: a browser that will not take the command is
                    // still better off running than not running.
                    warn!("Could not disable downloads: {}", e);
                }
            }
            Err(e) => warn!("Could not build the download-behavior command: {}", e),
        }

        let mut attached_events = match browser.event_listener::<EventAttachedToTarget>().await {
            Ok(stream) => stream,
            Err(e) => {
                error!("Failed to subscribe to Target.attachedToTarget events: {}", e);
                sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        if let Err(e) = configure_target_auto_attach(&browser).await {
            error!("Failed to configure target auto-attach: {}", e);
            if is_connection_lost(&e) {
                sleep(Duration::from_secs(2)).await;
                continue;
            }
        }

        // Reuse one page and close all stale pages/tabs.
        let page = match ensure_single_control_page(&mut browser).await {
            Ok(p) => p,
            Err(e) => {
                // Without the backoff this spins, hammering CDP as fast as it can fail.
                error!("Failed to initialize control page: {}", e);
                sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        // The payload a page with no item-specific layer gets. Every navigation
        // below re-seeds with its own before it navigates.
        let mut overlay_seed = OverlaySeed::default();
        {
            let payload = crate::settings::overlay_payload(&state, &display, None).await;
            if let Err(e) = seed_overlay_runtime(&page, &mut overlay_seed, &payload).await {
                debug!("Failed to register overlay runtime script: {}", e);
            }
        }

        if let Err(e) = register_scroll_runtime_script(&page).await {
            error!("Failed to register scroll runtime script: {}", e);
            if is_connection_lost(&e) {
                error!("Browser connection lost while registering runtime. Reconnecting...");
                sleep(Duration::from_secs(2)).await;
                continue;
            }
        }

        if let Err(e) = ensure_scroll_runtime(&page).await {
            error!("Failed to inject scroll runtime: {}", e);
            if is_connection_lost(&e) {
                error!("Browser connection lost while injecting runtime. Reconnecting...");
                sleep(Duration::from_secs(2)).await;
                continue;
            }
        }

        if let Err(e) = install_runtime_for_known_pages(&browser).await {
            debug!("Failed initial runtime install for known pages: {}", e);
        }

        if let Err(e) = drain_attached_target_events(&browser, &mut attached_events, Duration::from_millis(250)).await {
            debug!("Failed to drain initial attached target events: {}", e);
        }

        let dynamic_page = page.clone();
        let mut keep_loaded_tabs: HashMap<i64, (Page, String)> = HashMap::new();

        // Announced here rather than straight after `Browser::connect`: every
        // step between the two can still bail out to the top of this loop, and a
        // display whose control page failed to initialise is not one anybody
        // would call connected. Reaching this line means the browser is set up
        // and about to be driven.
        //
        // `reconnect` means "again within this process", which is narrower than
        // what an operator means by the word. `connected_before` starts false at
        // every start, and `chromium.rs` deliberately leaves the browser running
        // when the controller stops, so the first connect after a deploy or a
        // crash reports `reconnect: false` while the browser it attached to never
        // went anywhere. Widening it would mean persisting the flag, which is an
        // SD-card write for a field nobody acts on.
        state.webhooks.fire(&display_name, crate::webhook::Event::DisplayConnected {
            reconnect: connected_before,
        });
        connected_before = true;

        // Inner loop mainly for playlist iteration
        let mut reconnect_needed = false;
        loop {
            let active_override = {
                let lock = display.override_item.lock().await;
                lock.clone()
            };

            if let Some(override_item) = active_override {
                if let Err(e) = run_override_loop(&state, &display, &browser, &mut attached_events, &page, &mut overlay_seed, override_item, &lost).await {
                    error!("Override playback failed: {}", e);
                    if is_connection_lost(e.as_ref()) {
                        reconnect_needed = true;
                        break;
                    }
                }
                continue;
            }

            // 2. Fetch Playlist
            // What this screen plays is resolved per pass and never cached: an
            // operator editing the timetable expects the next item to follow it,
            // and a window that opened since the last pass is exactly that. A
            // failure to read it is *not* treated as "no playlist" -- a locked
            // database would then blank a screen that is playing perfectly well.
            let assigned = match crate::schedule::active_playlist(&state.pool, &display_name).await {
                Ok(active) => active.playlist_id,
                Err(e) => {
                    error!("Failed to read the timetable: {}", e);
                    sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            // A playlist just assigned starts at its beginning. `play_order` is
            // numbered per playlist, so `resume_after_order` -- an order within
            // the list it was recorded in -- means nothing in a different one,
            // and carrying it across would drop the operator into the middle of
            // the playlist they just picked.
            if assigned != last_assigned {
                resume_after_order = None;
                last_assigned = assigned;
            }

            // A screen nobody has chosen a playlist for plays nothing and takes
            // the idle branch below. Falling back to every item in the table
            // would mean a second screen mirrors the first the moment it is
            // declared, which is the opposite of what declaring it asked for.
            let playlist = match assigned {
                Some(playlist_id) => match sqlx::query_as::<_, PlaylistItemWithAsset>(
                    r#"
                SELECT
                    p.id, p.asset_id, p.url, p.play_order, COALESCE(p.advance, '{"on":"time","seconds":10}') as advance, p.is_enabled as enabled, p.is_enabled,
                    p.start_date, p.end_date,
                    COALESCE(p.keep_loaded, 0) as keep_loaded,
                    COALESCE(p.scroll_config, '{"type":"None","options":null}') as scroll_config,
                    COALESCE(p.fit_mode, 'contain') as fit_mode,
                    COALESCE(p.fit_background, '#000000') as fit_background,
                    a.local_path, a.mimetype, a.filename
                FROM playlist_items p
                LEFT JOIN assets a ON p.asset_id = a.id
                                WHERE p.is_enabled = 1
                                    AND p.playlist_id = ?
                                    AND (p.start_date IS NULL OR datetime(p.start_date) <= datetime('now'))
                                    AND (p.end_date IS NULL OR datetime('now') <= datetime(p.end_date))
                ORDER BY p.play_order ASC
                "#,
                )
                .bind(playlist_id)
                .fetch_all(&state.pool)
                .await
                {
                    Ok(list) => list,
                    Err(e) => {
                        error!("DB Error: {}", e);
                        sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                },
                None => Vec::new(),
            };

            if let Err(e) = reconcile_keep_loaded_tabs(
                &mut browser,
                &state,
                &playlist,
                &mut keep_loaded_tabs,
                &dynamic_page,
            )
            .await
            {
                error!("Failed to reconcile keep_loaded tabs: {}", e);
                if is_connection_lost(&e) {
                    reconnect_needed = true;
                    break;
                }
            }

            if playlist.is_empty() {
                // This branch re-runs every 5s while the playlist stays empty. Only
                // navigate if we are not already on the placeholder, otherwise the
                // idle screen reloads itself every 5 seconds.
                //
                // Seeded before the navigation, so the idle page is born with
                // the badge. The idle screen is a page like any other, and the
                // one most likely to be up when somebody sets a notice.
                let overlay = crate::settings::overlay_payload(&state, &display, None).await;
                if let Err(e) = seed_overlay_runtime(&page, &mut overlay_seed, &overlay).await {
                    debug!("Failed to seed the overlay for the idle page: {}", e);
                    if is_connection_lost(&e) {
                        reconnect_needed = true;
                        break;
                    }
                }
                let empty_url = empty_playlist_url(state.args.port, &display_name);
                *display.screen_page.lock().await = Some(page.clone());
                let already_showing = match page.url().await {
                    Ok(Some(current)) => current == empty_url,
                    _ => false,
                };

                if !already_showing {
                    if let Err(e) = navigate_page(&page, &empty_url).await {
                        error!("Failed to navigate blank page: {}", e);
                        if is_connection_lost(&e) {
                            reconnect_needed = true;
                            break;
                        }
                    }
                }
                // Update state
                {
                    let mut lock = display.current_item_id.lock().await;
                    *lock = None;
                }

                if !announced_empty {
                    announced_empty = true;
                    state.webhooks.fire(&display_name, crate::webhook::Event::PlaylistEmpty);
                }

                if let Err(e) = apply_overlay_payload(&page, &overlay).await {
                    debug!("Failed to apply overlay on the idle page: {}", e);
                }

                // Wait a bit before checking DB again
                tokio::select! {
                    _ = sleep(Duration::from_secs(5)) => {},
                    _ = display.skip_signal.notified() => {
                        info!("Skip signal received (while empty), reloading playlist...");
                    }
                    _ = display.playlist_signal.notified() => {
                        info!("Playlist changed while empty, reloading playlist...");
                    }
                    _ = display.override_signal.notified() => {
                        info!("Override signal received while empty.");
                    }
                    _ = display.overlay_signal.notified() => {
                        info!("Overlay settings changed while empty.");
                    }
                }
                continue;
            }

            // 3. Iterate Items (index-based; explicit jumps are handled on skip signal)
            // Resume after the last item we finished. Matching on play_order rather
            // than id means this still works if that item was the thing deleted.
            let mut index: usize = match resume_after_order.take() {
                Some(order) => playlist
                    .iter()
                    .position(|x| x.play_order > order)
                    .unwrap_or(0),
                None => 0,
            };

            // A "Play now" for an item added or re-enabled since the previous
            // playlist fetch cannot be resolved against that older snapshot, so it is
            // resolved here against the list just read. It must only be cleared once
            // it has been checked against a fresh list: consuming it on a miss drops
            // the click and resumes playback on an unrelated item.
            {
                let mut pending = display.pending_jump.lock().await;
                if let Some(target_id) = *pending {
                    match playlist.iter().position(|x| x.id == target_id) {
                        Some(pos) => {
                            info!("Resolving pending play-now for item {}", target_id);
                            index = pos;
                        }
                        None => warn!(
                            "Play-now target {} is not in the active playlist, ignoring",
                            target_id
                        ),
                    }
                    *pending = None;
                }
            }

            while index < playlist.len() {
                let item = &playlist[index];
                // Update current item ID
                {
                    let mut lock = display.current_item_id.lock().await;
                    *lock = Some(item.id);
                }

                let target_url = playlist_target_url(&state, item);

                // Clamp before the cast: a negative i64 wraps to a ~584-billion-year
                // u64 and parks the playlist on this item forever; 0 spins the loop.
                let advance = item.advance.0.clamped();
                // A time runs out on its own; passes are counted by the page.
                let (time_limit, pass_target) = match advance {
                    crate::advance::Advance::Time { seconds } => (Some(Duration::from_secs(u64::from(seconds))), None),
                    crate::advance::Advance::Passes { count } => (None, Some(count)),
                };

                // Something is playing again, so the next empty playlist is worth
                // announcing afresh.
                announced_empty = false;
                state.webhooks.fire(&display_name, crate::webhook::Event::ItemChanged {
                    item_id: item.id,
                    kind: if item.asset_id.is_some() { "asset" } else { "url" },
                    // A URL item has no name but its URL, and that URL can carry
                    // credentials exactly as the `url` field can -- so it is
                    // redacted here too rather than only in `url`.
                    title: item
                        .filename
                        .clone()
                        .or_else(|| item.url.as_deref().map(redact_str))
                        .unwrap_or_default(),
                    url: redact_str(&target_url),
                    advance,
                });

                let (active_page, do_navigate) = if item.keep_loaded {
                    if let Some((tab, _)) = keep_loaded_tabs.get(&item.id) {
                        (tab.clone(), false)
                    } else {
                        (dynamic_page.clone(), true)
                    }
                } else {
                    (dynamic_page.clone(), true)
                };

                info!(
                    "Showing item {} (keep_loaded: {}) at {} (advance: {:?})",
                    item.id,
                    item.keep_loaded,
                    redact_str(&target_url),
                    advance
                );

                *display.screen_page.lock().await = Some(active_page.clone());
                if let Err(e) = active_page.bring_to_front().await {
                    warn!("Failed to bring page to front: {}", e);
                    if is_connection_lost(&e) {
                        reconnect_needed = true;
                        break;
                    }
                }

                // Built before the navigation rather than after it: the control
                // page is seeded with it so the next document is born with the
                // badge. Read fresh rather than taken from the playlist
                // snapshot: that snapshot is read once per inner-loop pass and
                // can be a whole item duration old, so an overlay edited while
                // the previous item was up would otherwise appear one full
                // rotation late.
                let item_overlay = crate::db::load_item_overlay(&state.pool, item.id).await;
                let overlay =
                    crate::settings::overlay_payload(&state, &display, item_overlay.as_ref()).await;

                if do_navigate {
                    if let Err(e) = seed_overlay_runtime(&dynamic_page, &mut overlay_seed, &overlay).await {
                        debug!("Failed to seed the overlay: {}", e);
                        if is_connection_lost(&e) {
                            reconnect_needed = true;
                            break;
                        }
                    }
                    if let Err(e) = navigate_page(&active_page, &target_url).await {
                        error!("Navigation failed: {}", e);
                        if is_connection_lost(&e) {
                            reconnect_needed = true;
                            break;
                        }
                    }
                }

                if let Err(e) = drain_attached_target_events(&browser, &mut attached_events, Duration::from_millis(700)).await {
                    debug!("Failed to process attached target events: {}", e);
                }

                if let Err(e) = install_runtime_for_known_pages(&browser).await {
                    debug!("Failed runtime install for known targets: {}", e);
                }

                if let Err(e) = wait_for_scroll_readiness(&active_page, Duration::from_secs(12)).await {
                    debug!("Scroll readiness wait failed: {}", e);
                }

                let uses_internal_viewer = is_internal_pdf_viewer_url(&target_url);

                if !uses_internal_viewer {
                    if let Err(e) = start_scrolling(&active_page, &item.scroll_config.0).await {
                        error!("Failed to start scrolling: {}", e);
                        if is_connection_lost(e.as_ref()) {
                            reconnect_needed = true;
                            break;
                        }
                    }
                }

                if let Err(e) = apply_overlay_payload(&active_page, &overlay).await {
                    error!("Failed to apply overlay: {}", e);
                    if is_connection_lost(e.as_ref()) {
                        reconnect_needed = true;
                        break;
                    }
                }

                tokio::time::sleep(Duration::from_millis(1200)).await;
                if let Err(e) = log_scroll_runtime_snapshot(&active_page, "after-start").await {
                    debug!("Failed to fetch scroll runtime snapshot: {}", e);
                }

                // The clock starts once the content is actually on screen. Counting
                // from before navigation meant a slow page (readiness waits up to 12s)
                // consumed its whole duration loading and flashed past instantly.
                // The video and the clock start together, so an item as long as
                // its video ends with it instead of showing its start again.
                // Also for a keep_loaded tab brought to front: counting starts
                // when it is seen, not when it was loaded. Before `start_media`,
                // so a counted video has stopped looping before it plays.
                if let Some(count) = pass_target {
                    reset_advance(&active_page, count).await;
                }
                start_media(&active_page).await;
                let item_started_at = Instant::now();
                let mut remaining = time_limit;
                let stall = Duration::from_secs(state.args.advance_stall_timeout);
                let mut missing_since: Option<Instant> = None;
                // Recomputed after every wake but the pass poll, which comes
                // twice a second and changes nothing about the timetable.
                let mut boundary: Option<tokio::time::Instant> = None;
                let mut boundary_stale = true;

                let mut skip_requested = false;
                let mut reload_before_next = false;
                loop {
                    if matches!(remaining, Some(r) if r.is_zero()) {
                        break;
                    }
                    if boundary_stale {
                        // Not computed once per item: an edited timetable pokes
                        // `playlist_signal`, which lands here, and a timer
                        // computed before the edit would fire at a boundary that
                        // no longer exists -- or miss one that does.
                        boundary = match crate::schedule::load(&state.pool, &display_name).await {
                            Ok((_, windows)) => {
                                let now = crate::schedule::now();
                                crate::schedule::next_boundary(&windows, now)
                                    .and_then(|at| (at - now).to_std().ok())
                                    .map(|wait| tokio::time::Instant::now() + wait)
                            }
                            Err(e) => {
                                debug!("Failed to read the timetable for the boundary timer: {}", e);
                                None
                            }
                        };
                        boundary_stale = false;
                    }
                    tokio::select! {
                        _ = async {
                            match remaining {
                                Some(wait) => sleep(wait).await,
                                None => std::future::pending::<()>().await,
                            }
                        } => {
                            info!("Duration ended.");
                            break;
                        },
                        _ = async {
                            match pass_target {
                                Some(_) => sleep(crate::advance::POLL).await,
                                None => std::future::pending::<()>().await,
                            }
                        } => {
                            let count = pass_target.unwrap_or(1);
                            let report = match read_advance(&active_page).await {
                                Ok(report) => report,
                                Err(e) => {
                                    error!("Lost the page while counting passes: {}", e);
                                    reconnect_needed = true;
                                    break;
                                }
                            };
                            if report.is_some() {
                                missing_since = None;
                            } else if missing_since.is_none() {
                                missing_since = Some(Instant::now());
                            }
                            let missing_for = missing_since.map(|t| t.elapsed()).unwrap_or_default();
                            match crate::advance::verdict(report.as_ref(), count, missing_for, stall) {
                                crate::advance::Verdict::Wait => {}
                                crate::advance::Verdict::Done => {
                                    info!("Item {} ran its {} pass(es).", item.id, count);
                                    break;
                                }
                                crate::advance::Verdict::Stalled(why) => {
                                    warn!("Item {} stalled ({}), moving on.", item.id, why);
                                    break;
                                }
                            }
                        },
                        _ = lost.notified() => {
                            warn!("The browser connection closed while item {} was showing.", item.id);
                            reconnect_needed = true;
                            break;
                        },
                        _ = display.skip_signal.notified() => {
                            info!("Skip signal received.");
                            skip_requested = true;
                            break;
                        },
                        _ = display.override_signal.notified() => {
                            info!("Override signal received, interrupting item.");
                            break;
                        },
                        _ = display.overlay_signal.notified() => {
                            boundary_stale = true;
                            // Applied to the page already on screen, and the
                            // remaining time is recomputed rather than restarted
                            // -- an overlay edit must not silently extend the
                            // item it lands on.
                            info!("Overlay settings changed, re-applying.");
                            // Re-read: the edit that woke us may well be this
                            // item's own layer.
                            let fresh =
                                crate::db::load_item_overlay(&state.pool, item.id).await;
                            let payload = crate::settings::overlay_payload(
                                &state, &display, fresh.as_ref()).await;
                            // Re-seeded too, when the item is on the control
                            // page: the page may navigate itself before the loop
                            // does, and it must come back with this badge, not
                            // the one from before the edit.
                            if do_navigate {
                                if let Err(e) = seed_overlay_runtime(
                                    &dynamic_page, &mut overlay_seed, &payload).await
                                {
                                    debug!("Failed to re-seed the overlay: {}", e);
                                }
                            }
                            if let Err(e) = apply_overlay_payload(&active_page, &payload).await
                            {
                                error!("Failed to re-apply overlay: {}", e);
                                if is_connection_lost(e.as_ref()) {
                                    reconnect_needed = true;
                                    break;
                                }
                            }
                            remaining = time_limit.map(|t| t.saturating_sub(item_started_at.elapsed()));
                        },
                        _ = display.playlist_signal.notified() => {
                            boundary_stale = true;
                            let still_active =
                                is_playlist_item_active_now(&state, &display_name, item.id).await;
                            if still_active {
                                reload_before_next = true;
                                remaining = time_limit.map(|t| t.saturating_sub(item_started_at.elapsed()));
                            } else {
                                info!("Current item {} became inactive or was removed, skipping now.", item.id);
                                reload_before_next = true;
                                break;
                            }
                        }
                        _ = async {
                            match boundary {
                                Some(at) => tokio::time::sleep_until(at).await,
                                None => std::future::pending::<()>().await,
                            }
                        } => {
                            boundary_stale = true;
                            // A window opened or closed. Switching is immediate,
                            // like a reassignment; two adjacent windows naming the
                            // same playlist are not a switch, and the item carries
                            // on with its remaining time.
                            let live = crate::schedule::active_playlist(&state.pool, &display_name)
                                .await
                                .map(|active| active.playlist_id);
                            if matches!(live, Ok(live) if live != assigned) {
                                info!("The timetable changed what this screen plays, leaving item {} now.", item.id);
                                reload_before_next = true;
                                break;
                            }
                            remaining = time_limit.map(|t| t.saturating_sub(item_started_at.elapsed()));
                        },
                    }
                }

                if reconnect_needed {
                    break;
                }

                let override_active = {
                    let lock = display.override_item.lock().await;
                    lock.is_some()
                };

                if !uses_internal_viewer {
                    if let Err(e) = stop_scrolling(&active_page).await {
                        error!("Failed to stop scrolling: {}", e);
                        if is_connection_lost(&e) {
                            reconnect_needed = true;
                            break;
                        }
                    }
                }

                if override_active {
                    // Pick playback back up here once the override is cleared.
                    resume_after_order = Some(item.play_order);
                    break;
                }

                if skip_requested {
                    // Peek instead of take(): if the target is missing from this
                    // snapshot the playlist is re-read at the top of the loop and the
                    // jump is resolved there. Consuming it here threw the click away,
                    // because the snapshot can be a whole item duration out of date.
                    let target_id = *display.pending_jump.lock().await;
                    if let Some(target_id) = target_id {
                        match playlist.iter().position(|x| x.id == target_id) {
                            Some(pos) => {
                                *display.pending_jump.lock().await = None;
                                index = pos;
                                continue;
                            }
                            None => {
                                warn!(
                                    "Play-now target {} not in current snapshot, re-reading playlist",
                                    target_id
                                );
                                // If the fresh list does not contain it either (disabled
                                // item, outside its date window), carry on from here
                                // instead of restarting at the top of the playlist. A
                                // resolvable jump overrides this below.
                                resume_after_order = Some(item.play_order);
                                break;
                            }
                        }
                    }
                    // No explicit target: plain skip to the next item.
                }

                if reload_before_next {
                    // The playlist changed under us: re-read it, but carry on from
                    // where we were instead of jumping back to the first item.
                    resume_after_order = Some(item.play_order);
                    break;
                }

                index += 1;
            }

            if reconnect_needed {
                break;
            }
        }

        if reconnect_needed {
            error!("CDP session lost. Reconnecting to browser...");
            // Nothing is on screen as far as this controller can tell until the
            // connection is back: no freeze to judge, no picture to take.
            *display.screen_page.lock().await = None;
            // The only way out of the inner loop, so this is the one path that
            // loses a connection that was working. The guard is belt and braces
            // against a future early `continue` slipping in above.
            if connected_before {
                state.webhooks.fire(&display_name, crate::webhook::Event::DisplayDisconnected {
                    error: "the CDP connection was lost".to_string(),
                });
            }
            sleep(Duration::from_secs(2)).await;
        }
    }
}

async fn ensure_single_control_page(browser: &mut Browser) -> Result<Page, chromiumoxide::error::CdpError> {
    if let Err(e) = browser.fetch_targets().await {
        warn!("Failed to fetch existing targets: {}", e);
    } else {
        tokio::time::sleep(Duration::from_millis(120)).await;
    }

    let mut pages = browser.pages().await.unwrap_or_default();

    if pages.is_empty() {
        return browser.new_page("about:blank").await;
    }

    let control_page = pages.remove(0);



    for stale in pages {
        debug!("STALE PAGE: {}", redact_str(&stale.url().await.unwrap_or(None).unwrap_or_default()));
        if let Err(e) = stale.close().await {
            warn!("Failed to close stale tab: {}", e);
        }
    }

    Ok(control_page)
}

async fn run_override_loop(
    state: &AppState,
    // The display `browser_loop` is driving, passed in rather than resolved a
    // second time: resolving here would pin every screen's override loop to the
    // first display's signals.
    display: &Display,
    browser: &Browser,
    attached_events: &mut EventStream<EventAttachedToTarget>,
    page: &Page,
    overlay_seed: &mut OverlaySeed,
    mut override_item: OverrideItem,
    lost: &tokio::sync::Notify,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    loop {
        let target_url = override_target_url(state, &override_item);
        info!("Override active. Navigating to: {}", redact_str(&target_url));

        let _ = page.bring_to_front().await;
        *display.screen_page.lock().await = Some(page.clone());
        let overlay = crate::settings::overlay_payload(state, display, None).await;
        seed_overlay_runtime(page, overlay_seed, &overlay).await?;
        navigate_page(page, &target_url).await?;
        let _ = drain_attached_target_events(browser, attached_events, Duration::from_millis(700)).await;
        let _ = install_runtime_for_known_pages(browser).await;
        let _ = wait_for_scroll_readiness(page, Duration::from_secs(12)).await;
        let uses_internal_viewer = is_internal_pdf_viewer_url(&target_url);
        if !uses_internal_viewer {
            start_scrolling(page, &override_item.scroll_config).await?;
        }
        if let Err(e) = apply_overlay_payload(page, &overlay).await {
            error!("Failed to apply overlay on the override page: {}", e);
        }
        start_media(page).await;

        tokio::time::sleep(Duration::from_millis(1200)).await;

        loop {
            // An overlay edit must reach an override too -- a cast or a pinned
            // page can be on screen for hours, which is exactly when a notice
            // matters. Re-applying does not touch the page otherwise, so a live
            // RTCPeerConnection survives it.
            tokio::select! {
                // Worded so `is_connection_lost` recognises it: the caller then
                // reconnects, as for any other lost connection.
                _ = lost.notified() => return Err("the CDP connection closed".into()),
                _ = display.override_signal.notified() => {},
                _ = display.overlay_signal.notified() => {
                    info!("Overlay settings changed while an override is up, re-applying.");
                    let payload = crate::settings::overlay_payload(state, display, None).await;
                    if let Err(e) = seed_overlay_runtime(page, overlay_seed, &payload).await {
                        debug!("Failed to re-seed the overlay: {}", e);
                    }
                    // Touches nothing but the badge, so a live RTCPeerConnection
                    // survives it.
                    if let Err(e) = apply_overlay_payload(page, &payload).await {
                        error!("Failed to re-apply overlay: {}", e);
                    }
                    continue;
                }
            }

            let current_override = {
                let lock = display.override_item.lock().await;
                lock.clone()
            };

            match current_override {
                Some(next_override) => {
                    // The notification that got us here may be the same one that set
                    // this override in the first place (notify_one leaves a permit).
                    // Re-navigating on an unchanged override restarts the page for no
                    // reason, so wait for a real change instead.
                    if next_override == override_item {
                        continue;
                    }
                    if !uses_internal_viewer {
                        stop_scrolling(page).await?;
                    }
                    override_item = next_override;
                    break;
                }
                None => {
                    if !uses_internal_viewer {
                        stop_scrolling(page).await?;
                    }
                    return Ok(());
                }
            }
        }
    }
}

async fn reconcile_keep_loaded_tabs(
    browser: &mut Browser,
    state: &AppState,
    playlist: &[PlaylistItemWithAsset],
    tabs: &mut HashMap<i64, (Page, String)>,
    dynamic_page: &Page,
) -> Result<(), CdpError> {
    let wanted: HashSet<i64> = playlist
        .iter()
        .filter(|item| item.keep_loaded)
        .map(|item| item.id)
        .collect();

    let existing_ids: Vec<i64> = tabs.keys().copied().collect();
    for id in existing_ids {
        if !wanted.contains(&id) {
            if let Some((page, _)) = tabs.remove(&id) {
                if page.target_id() != dynamic_page.target_id() {
                    let _ = page.close().await;
                }
            }
        }
    }

    for item in playlist.iter().filter(|item| item.keep_loaded) {
        let target_url = playlist_target_url(state, item);
        if let Some((tab, loaded_url)) = tabs.get_mut(&item.id) {
            if loaded_url != &target_url {
                navigate_page(tab, &target_url).await?;
                *loaded_url = target_url;
            }
            continue;
        }

        let tab = browser.new_page("about:blank").await?;
        let _ = register_overlay_runtime_script(&tab).await;
        let _ = register_scroll_runtime_script(&tab).await;
        let _ = ensure_scroll_runtime(&tab).await;
        navigate_page(&tab, &target_url).await?;
        tabs.insert(item.id, (tab, target_url));
    }

    Ok(())
}

fn playlist_target_url(state: &AppState, item: &PlaylistItemWithAsset) -> String {
    if let Some(url) = &item.url {
        return url.clone();
    }

    if let Some(path) = &item.local_path {
        let full_path = state.args.assets_dir.join(path);
        if !full_path.exists() {
            return no_content_url(state.args.port);
        }
        return asset_target_url(
            state.args.port,
            path,
            item.mimetype.as_deref(),
            &item.scroll_config.0,
            FitMode::from_value(&item.fit_mode),
            &item.fit_background,
        );
    }

    no_content_url(state.args.port)
}

fn override_target_url(state: &AppState, item: &OverrideItem) -> String {
    if let Some(url) = &item.url {
        return url.clone();
    }

    if let Some(path) = &item.local_path {
        return asset_target_url(
            state.args.port,
            path,
            item.mimetype.as_deref(),
            &item.scroll_config,
            item.fit_mode,
            &item.fit_background,
        );
    }

    "about:blank".to_string()
}

/// Where an asset is shown, given what it is and how its item wants it to sit.
///
/// Images and videos go through `media_viewer.html` rather than to `/uploads/`
/// directly: navigated to directly, Chromium builds its own image or media
/// document, whose layout no setting of ours can reach and whose video comes
/// with a control bar nothing turns off. PDFs keep their own viewer; anything
/// else is navigated to as before. The existence check stays with the caller,
/// so this is testable without an `AppState`.
fn asset_target_url(
    port: u16,
    local_path: &str,
    mimetype: Option<&str>,
    scroll: &ScrollMode,
    fit: FitMode,
    background: &str,
) -> String {
    if is_internal_pdf_mimetype(mimetype) {
        return internal_pdf_viewer_url(port, local_path, scroll, fit, background);
    }
    if let Some(kind) = media_kind(mimetype) {
        return format!(
            "http://127.0.0.1:{}/media_viewer.html?asset={}&kind={}&fit={}&bg={}",
            port,
            encode(local_path),
            kind,
            fit.as_str(),
            encode(background)
        );
    }
    format!("http://127.0.0.1:{}/uploads/{}#toolbar=0&navpanes=0&view=FitH", port, local_path)
}

fn media_kind(mimetype: Option<&str>) -> Option<&'static str> {
    let m = mimetype.unwrap_or_default().to_ascii_lowercase();
    if m.starts_with("image/") {
        Some("image")
    } else if m.starts_with("video/") {
        Some("video")
    } else {
        None
    }
}

fn is_internal_pdf_mimetype(mimetype: Option<&str>) -> bool {
    let m = mimetype.unwrap_or_default().to_ascii_lowercase();
    m == "application/pdf"
}

fn is_internal_pdf_viewer_url(url: &str) -> bool {
    url.contains("/pdf_viewer.html?")
}

fn internal_pdf_viewer_url(
    port: u16,
    local_path: &str,
    mode: &ScrollMode,
    fit: FitMode,
    background: &str,
) -> String {
    let mut query = vec![format!("asset={}", encode(local_path))];
    match mode {
        ScrollMode::None => {
            query.push("mode=none".to_string());
        }
        ScrollMode::Continuous(opts) => {
            query.push("mode=continuous".to_string());
            let px_per_sec = (opts.speed * 60.0).max(1.0);
            query.push(format!("speed={}", px_per_sec));
            query.push(format!("top_delay={}", opts.top_delay));
            query.push(format!("return_delay={}", opts.return_delay));
        }
        ScrollMode::Step(opts) => {
            query.push("mode=step".to_string());
            if let Some(px) = opts.step_px {
                query.push(format!("step_px={}", px));
            }
            query.push(format!("step_delay={}", opts.step_delay));
            if let Some(step_time) = opts.step_time {
                query.push(format!("step_time={}", step_time));
            }
        }
    }

    query.push(format!("fit={}", fit.as_str()));
    query.push(format!("bg={}", encode(background)));

    format!(
        "http://127.0.0.1:{}/pdf_viewer.html?{}",
        port,
        query.join("&")
    )
}

/// Strip credentials from a URL that is about to leave the process.
///
/// A playlist URL can carry them exactly like a guest page URL can, and neither
/// a webhook nor the journal is somewhere they may go. **Every log line in this
/// file that prints a URL goes through this** -- the control loop logs each
/// item it shows at `info`, and a dashboard that logs in through its query
/// string put a real password in a real journal before it did.
///
/// Anything unparseable is passed through: it is not a URL with credentials in
/// it either.
pub(crate) fn redact_str(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => crate::guest_page::redact(&parsed),
        Err(_) => url.to_string(),
    }
}

/// `redact_str` applied to every string in `value` that is a URL, however deep.
///
/// For a diagnostic blob that is about to be logged: walking it is the only way
/// to be sure, because the scroll runtime repeats the page's address in every
/// entry of its log buffer and a field-by-field fix would miss the next one.
fn redact_urls_in(value: &mut Value) {
    match value {
        Value::String(text) if text.starts_with("http://") || text.starts_with("https://") => {
            *text = redact_str(text);
        }
        Value::Array(items) => items.iter_mut().for_each(redact_urls_in),
        Value::Object(fields) => fields.values_mut().for_each(redact_urls_in),
        _ => {}
    }
}

fn no_content_url(port: u16) -> String {
    format!("http://127.0.0.1:{}/no_content.svg", port)
}

fn empty_playlist_url(port: u16, display: &str) -> String {
    format!(
        "http://127.0.0.1:{}/empty_playlist.html?screen={}",
        port,
        encode(display)
    )
}

/// Whether the item on screen is still one this display should be showing.
///
/// Judged against the playlist the timetable makes live *now*, not the one the
/// pass started with: a reassignment, a timetable edit and an item that was
/// disabled all land on the item already playing. Resolved here rather than
/// taken from the pass's snapshot for the reason that snapshot cannot be trusted
/// anywhere else: it may have changed since.
async fn is_playlist_item_active_now(state: &AppState, display_name: &str, id: i64) -> bool {
    let live = match crate::schedule::active_playlist(&state.pool, display_name).await {
        Ok(active) => active.playlist_id,
        Err(e) => {
            error!("Failed to read the timetable: {}", e);
            return false;
        }
    };
    let Some(playlist_id) = live else {
        return false;
    };
    let row: Result<(i64,), _> = sqlx::query_as(
        r#"
        SELECT COUNT(*)
        FROM playlist_items p
        WHERE p.id = ?
          AND p.playlist_id = ?
          AND p.is_enabled = 1
          AND (p.start_date IS NULL OR datetime(p.start_date) <= datetime('now'))
          AND (p.end_date IS NULL OR datetime('now') <= datetime(p.end_date))
        "#,
    )
    .bind(id)
    .bind(playlist_id)
    .fetch_one(&state.pool)
    .await;

    row.map(|(count,)| count > 0).unwrap_or(false)
}

/// True for `CdpError`s that mean the CDP session itself is gone, as opposed to a
/// single command failing (a JS exception, a timeout, a missing frame).
fn cdp_indicates_disconnect(err: &CdpError) -> bool {
    matches!(
        err,
        CdpError::Ws(_)
            | CdpError::Io(_)
            | CdpError::ChannelSendError(_)
            | CdpError::NoResponse
            | CdpError::LaunchExit(..)
    )
}

fn message_indicates_disconnect(msg: &str) -> bool {
    let msg = msg.to_ascii_lowercase();
    msg.contains("receiver is gone")
        || msg.contains("websocket")
        || msg.contains("connection reset")
        || msg.contains("connection closed")
        || msg.contains("connection aborted")
        || msg.contains("connection refused")
        || msg.contains("channel closed")
        || msg.contains("broken pipe")
}

/// Matching must be on whole words. A bare substring like "ws" fires on any
/// message containing "rows", "answers" or "windows", which means constant
/// reconnects.
fn is_connection_lost(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(err);
    while let Some(e) = current {
        if let Some(cdp) = e.downcast_ref::<CdpError>() {
            if cdp_indicates_disconnect(cdp) {
                return true;
            }
        }
        current = e.source();
    }

    message_indicates_disconnect(&err.to_string())
}

/// Two URLs that address the same document, i.e. they differ at most in the fragment.
fn is_same_document(current: &str, target: &str) -> bool {
    fn without_fragment(u: &str) -> &str {
        u.split('#').next().unwrap_or(u)
    }
    without_fragment(current) == without_fragment(target)
}

/// Whether the tab is still on the blank page it was born with -- asked of
/// the page itself: `Page::url` reads chromiumoxide's own frame bookkeeping,
/// which is the part suspected of being stuck when a navigate hangs.
async fn still_blank(page: &Page) -> bool {
    let answer = tokio::time::timeout(Duration::from_secs(3), page.evaluate("location.href")).await;
    match answer {
        Ok(Ok(result)) => result.into_value::<String>().map(|href| href == "about:blank").unwrap_or(false),
        _ => false,
    }
}

async fn navigate_page(page: &Page, target_url: &str) -> Result<(), CdpError> {
    // A `Page.navigate` to the document we are already on is a same-document
    // navigation. Two things go wrong with it: Chrome's reply is a message
    // chromiumoxide cannot decode, so the command is never resolved and blocks for
    // the full 30s CDP request timeout; and even when it lands it does not reload,
    // so a recurring item would show stale content. Apply the fragment (if any) and
    // reload instead. This is the normal case for a single-item playlist.
    if let Ok(Some(current_url)) = page.url().await {
        if is_same_document(&current_url, target_url) {
            if current_url != target_url {
                let literal = serde_json::to_string(target_url)
                    .unwrap_or_else(|_| "\"about:blank\"".to_string());
                page.evaluate(format!("location.href = {}", literal)).await?;
            }
            page.execute(ReloadParams::builder().ignore_cache(true).build())
                .await?;
            return Ok(());
        }
    }

    // Cap the wait ourselves: chromiumoxide's own request timeout is 30s, long
    // enough for one dropped reply to eat several playlist items.
    //
    // A timeout while the tab is still blank means the command never took
    // effect. Seen on kiosk2 right after connecting to a (re)started browser:
    // chromiumoxide's `Page.navigate` hung every time for that connection while
    // the same command sent by hand went through in 0.3 s -- so retrying it is
    // useless, and the screen stood blank for the whole item, every item. The
    // page itself still answers `evaluate`, so navigate from inside it; if even
    // that leaves it blank, report the connection as lost so the loop
    // reconnects, which is what cleared it before. A tab already elsewhere is
    // a slow page loading, and is left to finish.
    let navigate = tokio::time::timeout(
        Duration::from_secs(20),
        page.execute(NavigateParams::new(target_url)),
    )
    .await;
    let response = match navigate {
        Ok(Ok(response)) => response,
        Ok(Err(CdpError::Timeout)) | Err(_) => {
            if !still_blank(page).await {
                warn!("Navigate command timed out for '{}', continuing (navigation may still succeed)", redact_str(target_url));
                return Ok(());
            }
            warn!(
                "Navigate command timed out for '{}' with the tab still blank, navigating from the page instead",
                redact_str(target_url)
            );
            let literal = serde_json::to_string(target_url).unwrap_or_else(|_| "\"about:blank\"".to_string());
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                page.evaluate(format!("location.assign({})", literal)),
            )
            .await;
            for _ in 0..20 {
                sleep(Duration::from_millis(500)).await;
                if !still_blank(page).await {
                    return Ok(());
                }
            }
            error!("The tab stays blank for '{}'; reconnecting to the browser", redact_str(target_url));
            // Worded so `is_connection_lost` recognises it.
            return Err(CdpError::ChromeMessage("the CDP connection closed: navigation never started".into()));
        }
        Ok(Err(e)) => return Err(e),
    };
    if let Some(err_text) = response.result.error_text {
        if err_text.to_lowercase().contains("timed out") {
            warn!("Navigation reported timeout for '{}', continuing", redact_str(target_url));
            return Ok(());
        }
        return Err(CdpError::ChromeMessage(err_text));
    }
    Ok(())
}

async fn register_scroll_runtime_script(page: &Page) -> Result<(), CdpError> {
    let _ = page
        .execute(AddScriptToEvaluateOnNewDocumentParams::new(scroll_runtime_script()))
        .await?;
    Ok(())
}

async fn ensure_scroll_runtime(page: &Page) -> Result<(), chromiumoxide::error::CdpError> {
    page.execute(PageEnableParams::default()).await.map(|_| ())?;
    page.execute(RuntimeEnableParams::default()).await.map(|_| ())?;
    page.evaluate(scroll_runtime_script()).await.map(|_| ())
}

async fn start_scrolling(page: &Page, mode: &ScrollMode) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    ensure_scroll_runtime(page).await?;
    apply_scroll_settings(page, mode).await
}

async fn apply_scroll_settings(page: &Page, mode: &ScrollMode) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let has_api: bool = page
        .evaluate("(() => !!globalThis.__as)()")
        .await?
        .into_value()?;

    if !has_api {
        warn!("Autoscroll API not available in current target; skipping scroll for this page");
        return Ok(());
    }

    let settings = scroll_mode_settings(mode);
    let script = format!(
        "(() => {{
            if (globalThis.__asApply) {{
                globalThis.__asApply({{ mode: '{}', speed: {}, enable: {}, topDelay: {}, returnDelay: {}, stepPx: {}, stepTime: {}, stepDelay: {} }});
                return true;
            }}
            if (!globalThis.__as) return false;
            globalThis.__as.setMode('{}');
            globalThis.__as.setSpeed({});
            if (globalThis.__as.setTopDelay) globalThis.__as.setTopDelay({});
            if (globalThis.__as.setReturnDelay) globalThis.__as.setReturnDelay({});
            if (globalThis.__as.setStepOptions) globalThis.__as.setStepOptions({}, {}, {});
            if ({}) globalThis.__as.enable(); else globalThis.__as.disable();
            return true;
        }})()",
        settings.backend_mode,
        settings.px_per_sec,
        if settings.enable { "true" } else { "false" },
        settings.top_delay_ms,
        settings.return_delay_ms,
        settings.step_px,
        settings.step_time_ms,
        settings.step_delay_ms,
        settings.backend_mode,
        settings.px_per_sec,
        settings.top_delay_ms,
        settings.return_delay_ms,
        settings.step_px,
        settings.step_time_ms,
        settings.step_delay_ms,
        if settings.enable { "true" } else { "false" }
    );
    page.evaluate(script).await.map(|_| ())?;
    Ok(())
}

async fn stop_scrolling(page: &Page) -> Result<(), chromiumoxide::error::CdpError> {
    page
        .evaluate("(() => {
            if (globalThis.__asApply) {
                globalThis.__asApply({ mode: 'auto', speed: 120, enable: false, topDelay: 0, returnDelay: 0 });
                return;
            }
            if (globalThis.__as) globalThis.__as.disable();
        })();")
        .await
        .map(|_| ())
}

struct ScrollRuntimeSettings {
    enable: bool,
    px_per_sec: f64,
    backend_mode: &'static str,
    top_delay_ms: u64,
    return_delay_ms: u64,
    step_px: u64,
    step_time_ms: u64,
    step_delay_ms: u64,
}

fn scroll_mode_settings(mode: &ScrollMode) -> ScrollRuntimeSettings {
    match mode {
        ScrollMode::None => ScrollRuntimeSettings {
            enable: false,
            px_per_sec: 120.0,
            backend_mode: "auto",
            top_delay_ms: 0,
            return_delay_ms: 0,
            step_px: 900,
            step_time_ms: 0,
            step_delay_ms: 2000,
        },
        ScrollMode::Continuous(opts) => {
            let px_per_sec = (opts.speed * 60.0).max(1.0);
            ScrollRuntimeSettings {
                enable: true,
                px_per_sec,
                backend_mode: "auto",
                top_delay_ms: opts.top_delay,
                return_delay_ms: opts.return_delay,
                step_px: 900,
                step_time_ms: 0,
                step_delay_ms: 2000,
            }
        }
        ScrollMode::Step(opts) => ScrollRuntimeSettings {
            enable: true,
            px_per_sec: 120.0,
            backend_mode: "step",
            top_delay_ms: 0,
            return_delay_ms: 0,
            step_px: opts.step_px.unwrap_or(900),
            step_time_ms: opts.step_time.unwrap_or(0),
            step_delay_ms: opts.step_delay,
        },
    }
}

async fn configure_target_auto_attach(browser: &Browser) -> Result<(), CdpError> {
    browser
        .execute(
            SetAutoAttachParams::builder()
                .auto_attach(true)
                .flatten(true)
                .wait_for_debugger_on_start(false)
                .build()
                .map_err(CdpError::ChromeMessage)?,
        )
        .await
        .map(|_| ())
}

async fn install_runtime_for_known_pages(
    browser: &Browser,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pages = browser.pages().await?;
    for page in pages {
        if let Err(e) = ensure_scroll_runtime(&page).await {
            debug!("Runtime install skipped for known page: {}", e);
        }
    }
    Ok(())
}

async fn drain_attached_target_events(
    browser: &Browser,
    events: &mut EventStream<EventAttachedToTarget>,
    budget: Duration,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let started = Instant::now();
    loop {
        let elapsed = started.elapsed();
        if elapsed >= budget {
            break;
        }

        let wait_for = (budget - elapsed).min(Duration::from_millis(120));
        let next = tokio::time::timeout(wait_for, events.next()).await;
        let Some(event) = (match next {
            Ok(v) => v,
            Err(_) => break,
        }) else {
            break;
        };

        let event = event.as_ref();
        let target_kind = format!("{:?}", event.target_info.r#type).to_ascii_lowercase();
        if target_kind != "\"page\"" && target_kind != "page" {
            continue;
        }

        let instrument = tokio::time::timeout(
            Duration::from_millis(350),
            instrument_attached_target(browser, &event.target_info),
        )
        .await;

        if let Err(_timeout) = instrument {
            debug!(
                "Skipped target instrumentation due to timeout for {:?} ({:?})",
                event.target_info.target_id,
                event.target_info.r#type,
            );
            continue;
        }

        if let Ok(Err(e)) = instrument {
            debug!(
                "Failed target instrumentation for {:?} ({:?}): {}",
                event.target_info.target_id,
                event.target_info.r#type,
                e
            );
        }
    }
    Ok(())
}

async fn instrument_attached_target(
    browser: &Browser,
    target_info: &TargetInfo,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    for _ in 0..6 {
        match browser.get_page(target_info.target_id.clone()).await {
            Ok(page) => {
                ensure_scroll_runtime(&page).await?;
                let probe: bool = page
                    .evaluate("(() => !!globalThis.__as)()")
                    .await?
                    .into_value()?;
                if !probe {
                    debug!(
                        "Autoscroll API blocked/unavailable for target {:?} ({})",
                        target_info.target_id,
                        target_info.url
                    );
                }
                return Ok(());
            }
            Err(_) => sleep(Duration::from_millis(80)).await,
        }
    }
    Ok(())
}

async fn log_scroll_runtime_snapshot(
    page: &Page,
    label: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let script = r#"(() => {
        const out = {
            href: (() => { try { return window.location && window.location.href; } catch (_) { return null; } })(),
            installed: !!globalThis.__as,
            listener: false,
            tail: [],
            metrics: null,
        };

        try {
            const buf = Array.isArray(globalThis.__asBuffer) ? globalThis.__asBuffer : [];
            out.tail = buf.slice(-8);
        } catch (_) {}

        try {
            if (globalThis.__as) {
                const sc = globalThis.__as;
                const st = sc.state ? sc.state() : null;
                out.metrics = {
                    element: st && st.element ? st.element : null,
                    scrollTop: st && st.window ? st.window.top : null,
                    viewport: st && st.window ? st.window.viewport : null,
                    scrollHeight: st && st.window ? st.window.height : null,
                    mode: st && st.mode ? st.mode : null,
                    speed: st && st.pxPerSec ? st.pxPerSec : null,
                    enabled: st && typeof st.enabled === 'boolean' ? st.enabled : null,
                    pendingConnections: st && typeof st.pendingConnections === 'number' ? st.pendingConnections : null,
                    idleForMs: st && typeof st.idleForMs === 'number' ? st.idleForMs : null,
                };
            }
        } catch (_) {}

        return JSON.stringify(out);
    })();"#;

    let snapshot = page.evaluate(script).await?;
    let snapshot_json: String = snapshot.into_value()?;
    let mut parsed: Value = serde_json::from_str(&snapshot_json)?;
    // The page's own address is the playlist URL, secrets included -- and not
    // only in `href`: every entry of the runtime's log buffer carries it too.
    redact_urls_in(&mut parsed);
    info!("scroll runtime snapshot [{}]: {}", label, parsed);
    Ok(())
}

async fn wait_for_scroll_readiness(
    page: &Page,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let started = Instant::now();
    let mut stable_count = 0u8;
    let mut last_signature = String::new();

    loop {
        let script = r#"(() => {
            const readyState = document.readyState || 'unknown';
            const href = (() => { try { return String((window.location && window.location.href) || ''); } catch (_) { return ''; } })();
            const bodyPresent = !!document.body;
            const mediaCount = document.querySelectorAll('iframe,embed,object,canvas,img,video,pdf-viewer,viewer,main').length;
            const textLen = (() => {
                try {
                    const txt = (document.body && document.body.innerText) ? document.body.innerText : '';
                    return txt.trim().length;
                } catch (_) { return 0; }
            })();

            let element = 'root';
            let viewport = 0;
            let maxScroll = 0;
            let pendingConnections = 0;
            let idleForMs = 0;
            try {
                if (globalThis.__as && globalThis.__as.state) {
                    const st = globalThis.__as.state();
                    element = st && st.element ? st.element : element;
                    viewport = Number(st && st.window && st.window.viewport) || 0;
                    maxScroll = Math.max(0, (Number(st && st.window && st.window.height) || 0) - viewport);
                    pendingConnections = Number(st && st.pendingConnections) || 0;
                    idleForMs = Number(st && st.idleForMs) || 0;
                } else {
                    const root = document.scrollingElement || document.documentElement || document.body;
                    viewport = Number(window.innerHeight || (root && root.clientHeight) || 0);
                    maxScroll = Math.max(0, (Number(root && root.scrollHeight) || 0) - viewport);
                }
            } catch (_) {}

            const likelyLoading = (readyState !== 'complete') || (!bodyPresent) || (textLen === 0 && mediaCount === 0);

            return JSON.stringify({
                href,
                readyState,
                likelyLoading,
                element,
                viewport,
                maxScroll,
                textLen,
                mediaCount,
                pendingConnections,
                idleForMs,
            });
        })();"#;

        let eval = page.evaluate(script).await?;
        let raw: String = eval.into_value()?;
        let v: Value = serde_json::from_str(&raw)?;

        let ready_state = v.get("readyState").and_then(Value::as_str).unwrap_or("unknown");
        let likely_loading = v.get("likelyLoading").and_then(Value::as_bool).unwrap_or(true);
        let max_scroll = v.get("maxScroll").and_then(Value::as_f64).unwrap_or(0.0);
        let element = v.get("element").and_then(Value::as_str).unwrap_or("root");
        let media_count = v.get("mediaCount").and_then(Value::as_u64).unwrap_or(0);
        let text_len = v.get("textLen").and_then(Value::as_u64).unwrap_or(0);
        let pending_connections = v.get("pendingConnections").and_then(Value::as_u64).unwrap_or(0);
        let idle_for_ms = v.get("idleForMs").and_then(Value::as_u64).unwrap_or(0);

        let signature = format!("{element}|{ready_state}|{:.1}|{}|{}|{}", max_scroll, media_count, text_len / 64, pending_connections);
        if signature == last_signature {
            stable_count = stable_count.saturating_add(1);
        } else {
            last_signature = signature;
            stable_count = 0;
        }

        let scrollable_ready = max_scroll > 2.0 && stable_count >= 2;
        let network_idle = pending_connections == 0 && idle_for_ms >= 900;
        let content_ready = !likely_loading && network_idle && stable_count >= 2;

        if scrollable_ready || content_ready {
            debug!(
                "Scroll readiness met: state={}, element={}, max_scroll={:.1}, pending={}, idle_for_ms={}, stable_count={}",
                ready_state,
                element,
                max_scroll,
                pending_connections,
                idle_for_ms,
                stable_count
            );
            return Ok(());
        }

        if started.elapsed() >= timeout {
            info!(
                "Scroll readiness timeout after {:?}: state={}, element={}, max_scroll={:.1}, media_count={}, text_len={}, pending={}, idle_for_ms={}",
                timeout,
                ready_state,
                element,
                max_scroll,
                media_count,
                text_len,
                pending_connections,
                idle_for_ms
            );
            return Ok(());
        }

        sleep(Duration::from_millis(350)).await;
    }
}

fn scroll_runtime_script() -> &'static str {
    include_str!("../web/autoscroll.js")
}

/// Served over HTTP *and* compiled in, exactly like the scroll runtime and for
/// the same reason: a page with a strict CSP can stop the pre-navigation
/// injection from running, so it has to be evaluable again afterwards.
fn overlay_runtime_script() -> &'static str {
    include_str!("../web/overlay.js")
}

async fn register_overlay_runtime_script(page: &Page) -> Result<(), CdpError> {
    let _ = page
        .execute(AddScriptToEvaluateOnNewDocumentParams::new(overlay_runtime_script()))
        .await?;
    Ok(())
}

/// Start the media viewer's video now, with the item's clock.
///
/// Probed, like the scroll and overlay runtimes: on any page but the media
/// viewer `__media` does not exist and this does nothing, which is why it is
/// called for every item rather than only for video URLs. A failure is not
/// worth a log above debug -- the page starts itself after 20 s.
/// Start counting passes on `page`, with the target, right where the item's
/// clock would start. Probed, never assumed, like the other runtimes: a page
/// without the counter is the stall case the loop handles.
async fn reset_advance(page: &Page, count: u16) {
    let script = format!(
        "(() => {{ if (globalThis.__advance && globalThis.__advance.reset) globalThis.__advance.reset({count}); }})()"
    );
    if let Err(e) = page.evaluate(script).await {
        debug!("Could not reset the pass counter on this page: {}", e);
    }
}

/// What the page's counter says, or `None` when it has none. A navigation
/// under the evaluate is `None` as well; a lost connection is the error.
async fn read_advance(page: &Page) -> Result<Option<crate::advance::RuntimeState>, chromiumoxide::error::CdpError> {
    let raw = page
        .evaluate("JSON.stringify(globalThis.__advance && globalThis.__advance.state ? globalThis.__advance.state() : null)")
        .await;
    match raw {
        Ok(result) => Ok(result
            .into_value::<String>()
            .ok()
            .and_then(|text| serde_json::from_str::<Option<crate::advance::RuntimeState>>(&text).ok())
            .flatten()),
        Err(e) if is_connection_lost(&e) => Err(e),
        Err(_) => Ok(None),
    }
}

async fn start_media(page: &Page) {
    if let Err(e) = page
        .evaluate("(() => { if (globalThis.__media) globalThis.__media.start(); })()")
        .await
    {
        debug!("Could not start the media on this page: {}", e);
    }
}

/// The overlay registration the control page carries for its next document.
///
/// Held per CDP connection: a registration belongs to the session that made
/// it, so a reconnect starts from nothing.
#[derive(Default)]
struct OverlaySeed {
    id: Option<ScriptIdentifier>,
    payload: Option<Value>,
}

/// Register the overlay runtime for `page`'s next document, carrying `payload`,
/// so the badge is drawn the moment that document exists rather than after the
/// readiness waits -- the gap that made it blink at every item change. It also
/// covers a page that navigates itself (a dashboard on a meta refresh), which
/// the controller never re-applies to.
///
/// Replaces the registration `seed` holds rather than adding beside it: every
/// registration runs on every navigation for as long as the session lives, so
/// one per item change is a leak that also runs N copies of the runtime.
/// Unchanged payloads are skipped, so the idle branch calling this every five
/// seconds costs nothing.
async fn seed_overlay_runtime(
    page: &Page,
    seed: &mut OverlaySeed,
    payload: &Value,
) -> Result<(), CdpError> {
    if seed.id.is_some() && seed.payload.as_ref() == Some(payload) {
        return Ok(());
    }
    if let Some(previous) = seed.id.take() {
        // Not fatal on its own: a registration that is already gone is exactly
        // the state we want. A lost connection fails the add below as well.
        if let Err(e) = page
            .execute(RemoveScriptToEvaluateOnNewDocumentParams::new(previous))
            .await
        {
            debug!("Could not remove the previous overlay registration: {}", e);
        }
    }
    let script = format!(
        "globalThis.__ovSeeds = (globalThis.__ovSeeds || 0) + 1;\nglobalThis.__ovSeed = {};\n{}",
        serde_json::to_string(payload).unwrap_or_else(|_| "null".to_string()),
        overlay_runtime_script()
    );
    let added = page
        .execute(AddScriptToEvaluateOnNewDocumentParams::new(script))
        .await?;
    seed.id = Some(added.result.identifier);
    seed.payload = Some(payload.clone());
    Ok(())
}

/// Put the operator's badge on the page on screen, or take it off again.
///
/// Evaluated after navigation as well as seeded before it, because a strict
/// CSP can block the registered copy -- on such a page this is the only thing
/// that draws the badge, and it blinks there exactly as before. With the seed
/// in place the payload is identical and re-applying it is invisible.
///
/// No-ops when the runtime is missing, the same way `apply_scroll_settings`
/// does: a page that blocked the injection must not stall the playlist. The
/// payload comes from `settings::overlay_payload`, so the display and the
/// operator's preview cannot render different things.
async fn apply_overlay_payload(
    page: &Page,
    payload: &Value,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = page.evaluate(overlay_runtime_script()).await;
    let has_api: bool = page
        .evaluate("(() => !!globalThis.__ov)()")
        .await?
        .into_value()
        .unwrap_or(false);
    if !has_api {
        debug!("Overlay runtime missing on this page (CSP?), leaving it alone");
        return Ok(());
    }

    let script = format!(
        "(() => globalThis.__ov.apply({}))()",
        serde_json::to_string(payload).unwrap_or_else(|_| "null".to_string())
    );
    let shown: bool = page.evaluate(script).await?.into_value().unwrap_or(false);
    debug!("Overlay applied (visible: {})", shown);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::FitMode;

    #[test]
    fn an_image_goes_through_the_media_viewer_with_its_fit() {
        let url = asset_target_url(3000, "a b.png", Some("image/png"), &ScrollMode::None,
                                   FitMode::Cover, "#00ff00");
        assert_eq!(
            url,
            "http://127.0.0.1:3000/media_viewer.html?asset=a%20b.png&kind=image&fit=cover&bg=%2300ff00"
        );
    }

    #[test]
    fn a_video_goes_through_the_media_viewer_as_a_video() {
        let url = asset_target_url(3000, "clip.mp4", Some("video/mp4"), &ScrollMode::None,
                                   FitMode::Contain, "#000000");
        assert!(url.contains("/media_viewer.html?"), "{url}");
        assert!(url.contains("kind=video"), "{url}");
    }

    #[test]
    fn a_pdf_keeps_its_own_viewer_and_is_told_its_fit() {
        let url = asset_target_url(3000, "doc.pdf", Some("application/pdf"), &ScrollMode::None,
                                   FitMode::Height, "#112233");
        assert!(url.contains("/pdf_viewer.html?"), "{url}");
        assert!(url.contains("&fit=height"), "{url}");
        assert!(url.contains("&bg=%23112233"), "{url}");
        // Still exempt from start_scrolling: it drives its own scrolling.
        assert!(is_internal_pdf_viewer_url(&url), "{url}");
    }

    #[test]
    fn anything_else_is_navigated_to_directly() {
        let url = asset_target_url(3000, "page.html", Some("text/html"), &ScrollMode::None,
                                   FitMode::Cover, "#000000");
        assert!(url.starts_with("http://127.0.0.1:3000/uploads/page.html"), "{url}");
        let unknown = asset_target_url(3000, "blob", None, &ScrollMode::None,
                                       FitMode::Contain, "#000000");
        assert!(unknown.contains("/uploads/blob"), "{unknown}");
    }

    #[test]
    fn a_logged_diagnostic_blob_loses_every_secret_in_it() {
        let mut blob = serde_json::json!({
            "href": "https://a.test/?password=hunter2",
            "tail": [[1, "https://a.test/?password=hunter2", "setMode", "auto"]],
            "metrics": { "element": "root" },
        });
        redact_urls_in(&mut blob);
        let text = blob.to_string();
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("password=***"), "{text}");
        assert!(text.contains("setMode"), "{text}");
    }

    #[test]
    fn the_media_viewer_is_not_exempt_from_scrolling() {
        // `scroll` depends on the scroll runtime driving this page. Only the PDF
        // viewer scrolls itself and is skipped by `start_scrolling`.
        let url = asset_target_url(3000, "tall.png", Some("image/png"), &ScrollMode::None,
                                   FitMode::Width, "#000000");
        assert!(!is_internal_pdf_viewer_url(&url), "{url}");
    }
}
