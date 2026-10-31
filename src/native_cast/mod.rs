//! Native cast receivers (Chromecast) wired into the per-display framework.
//! Feature-gated: nothing here links without `native-cast`.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::cast::access::{self, Access};
use crate::cast::ClaimMode;
use crate::settings::AppSettings;

/// Access decision for a native sender on one screen. Runs on casttarget's own
/// std::thread, so `block_on` is safe (mc's runtime has other worker threads and
/// this is not one of them). `account` resolves to refuse: a native sender has no
/// mc session, so `decide(.., signed_in=false)` returns Err for Account and Off.
pub struct NativeCastHook {
    pub handle: tokio::runtime::Handle,
    pub pool: sqlx::SqlitePool,
    pub screen: String,
    pub settings: Arc<RwLock<AppSettings>>,
}

impl casttarget::AccessHook for NativeCastHook {
    fn may_cast(&self, _sender: &casttarget::SenderInfo) -> bool {
        self.handle.block_on(async {
            let venue_on = self.settings.read().await.cast_enabled;
            let access: Access = access::load(&self.pool, &self.screen, ClaimMode::Cast).await;
            access::decide(venue_on, access, false).is_ok()
        })
    }
}

use crate::models::{AppState, Display, OverrideItem};

const BASE_CAST_PORT: u16 = 8009;
/// How long to wait before trying again when the cast.db cannot be loaded or a
/// receiver does not start. A kiosk often boots before DHCP has given it an
/// address, and the cast.db of a fresh one comes over the network.
const RETRY: std::time::Duration = std::time::Duration::from_secs(30);

/// Start one native-cast receiver per declared display. Receivers live until the
/// process ends (like the browser the controller does not kill), so their handles
/// are deliberately leaked with `std::mem::forget` -- SP3+ may hold them in state
/// for teardown.
///
/// Runs on its own thread and retries rather than giving up: loading the
/// cast.db may need the network (a fresh kiosk provisions it by fetch), and a
/// first attempt during boot used to leave casting off until the next restart.
/// Not on the async runtime, because everything casttarget does is blocking;
/// the runtime's handle is taken here, where there is a current one.
pub fn supervise(state: AppState, configured: Vec<crate::display::DisplayConfig>) {
    let handle = tokio::runtime::Handle::current();
    std::thread::spawn(move || run(state, configured, handle));
}

fn run(state: AppState, configured: Vec<crate::display::DisplayConfig>, handle: tokio::runtime::Handle) {
    let cast_db = state.args.cast_db_path();
    let db_str = cast_db.to_string_lossy().to_string();

    // One shared identity -> a base device id; each display gets a unique suffix,
    // so the mDNS device ids differ while the cast identity is shared.
    // ensure_identity bootstraps an empty cast.db from the fetch endpoint when
    // egress is allowed, so a fresh kiosk provisions itself instead of silently
    // starting no receiver.
    let mut failed_before = false;
    let base = loop {
        match casttarget::certstore::ensure_identity(&db_str) {
            Ok(id) => {
                if failed_before {
                    tracing::info!("native-cast: cast.db ({}) jetzt geladen", db_str);
                }
                break casttarget::cast::stable_device_id(&id.device_cert_der);
            }
            Err(e) => {
                // Said once at error level; the retries would otherwise fill
                // the journal every thirty seconds for as long as it lasts.
                if failed_before {
                    tracing::debug!("native-cast: cast.db noch nicht ladbar: {:#}", e);
                } else {
                    tracing::error!(
                        "native-cast: cast.db ({}) nicht ladbar: {:#} -- neuer Versuch alle {} s",
                        db_str,
                        e,
                        RETRY.as_secs()
                    );
                    failed_before = true;
                }
                std::thread::sleep(RETRY);
            }
        }
    };

    for (index, cfg) in configured.into_iter().enumerate() {
        let screen = cfg.name.clone();
        // Each receiver gets a TLS port and derives its eureka port as port-1
        // (Chromecast convention). Stride by 2 so display N's eureka port
        // (its TLS-1) never collides with display N-1's TLS port: display 0 is
        // TLS 8009 / eureka 8008, display 1 is TLS 8011 / eureka 8010, ...
        let port = BASE_CAST_PORT + (index as u16) * 2;
        let app_id = format!("casttarget-{}", cfg.name);
        let device_id = format!("{base}-{index}");
        // The name Android shows for this receiver: the machine's hostname, so a
        // venue with several kiosks is distinguishable. The implicit single
        // display is named "default" and would just repeat the hostname; a
        // declared screen appends its connector so the screens are told apart.
        let cast_name = if cfg.name == "default" {
            casttarget::hostname()
        } else {
            format!("{} ({})", casttarget::hostname(), cfg.name)
        };

        let hook = Arc::new(NativeCastHook {
            handle: handle.clone(),
            pool: state.pool.clone(),
            screen: screen.clone(),
            settings: state.settings.clone(),
        });

        let (tx, rx) = std::sync::mpsc::sync_channel::<casttarget::CastEvent>(32);

        // Drain the channel on its own thread (so block_on is not on a runtime
        // worker) and turn events into the external-window override.
        let display = state.displays.iter().find(|d| d.name == screen).cloned();
        let drain_handle = handle.clone();
        let drain_screen = screen.clone();
        std::thread::spawn(move || {
            while let Ok(ev) = rx.recv() {
                let Some(display) = display.as_ref() else { continue };
                match ev {
                    casttarget::CastEvent::Started { app_id } => {
                        tracing::info!("native-cast: start auf {} (app_id={})", drain_screen, app_id);
                        drain_handle.block_on(activate(display));
                    }
                    casttarget::CastEvent::Ended => {
                        tracing::info!("native-cast: ende auf {}", drain_screen);
                        drain_handle.block_on(deactivate(display));
                    }
                }
            }
        });

        let fetch = index == 0; // only one receiver refetches the shared cast.db
        let cast_db = cast_db.clone();
        // Each screen retries on its own thread, so one that cannot start does
        // not hold back the next. A missing address no longer fails a start --
        // casttarget waits for one before announcing -- so what is left to
        // retry is a port someone else holds, or a cast.db that broke since.
        std::thread::spawn(move || {
            let mut failed_before = false;
            loop {
                let started = casttarget::start(
                    casttarget::ReceiverConfig {
                        name: cast_name.clone(),
                        port,
                        device_id: device_id.clone(),
                        app_id: app_id.clone(),
                        cast_db: cast_db.clone(),
                        fetch,
                    },
                    hook.clone(),
                    tx.clone(),
                );
                match started {
                    Ok(receiver) => {
                        tracing::info!("native-cast: '{}' auf Port {} gestartet", screen, port);
                        std::mem::forget(receiver); // lives until process exit
                        return;
                    }
                    Err(e) if failed_before => {
                        tracing::debug!("native-cast: Receiver '{}' noch nicht gestartet: {:#}", screen, e)
                    }
                    Err(e) => {
                        tracing::error!(
                            "native-cast: Receiver '{}' nicht gestartet: {:#} -- neuer Versuch alle {} s",
                            screen,
                            e,
                            RETRY.as_secs()
                        );
                        failed_before = true;
                    }
                }
                std::thread::sleep(RETRY);
            }
        });
    }
}

/// Pin the display to the external-window override (native cast started).
pub async fn activate(display: &Display) {
    let item = OverrideItem {
        external_window: true,
        // the rest is irrelevant for an external window (never drawn).
        ..Default::default()
    };
    {
        let mut lock = display.override_item.lock().await;
        if lock.as_ref().map(|o| o.external_window) == Some(true) {
            return; // already ours
        }
        *lock = Some(item);
    }
    display.override_signal.notify_one();
}

/// Clear the override on cast end, but only if what is on screen is still our
/// external-window override -- an operator who set a different override during
/// the cast made a newer decision.
pub async fn deactivate(display: &Display) {
    {
        let mut lock = display.override_item.lock().await;
        match lock.as_ref() {
            Some(o) if o.external_window => {
                *lock = None;
            }
            _ => return, // not ours (or already gone)
        }
    }
    display.override_signal.notify_one();
}

#[cfg(test)]
mod tests {
    use crate::cast::access::{decide, Access, Refusal};

    // Native casts pass signed_in=false. The policy SP2 commits to:
    #[test]
    fn anyone_accepts_native() {
        assert!(decide(true, Access::Anyone, false).is_ok());
    }

    #[test]
    fn account_refuses_native_like_off() {
        assert_eq!(decide(true, Access::Account, false), Err(Refusal::NeedsAccount));
        assert_eq!(decide(true, Access::Off, false), Err(Refusal::Off));
    }

    #[test]
    fn venue_off_refuses_even_anyone() {
        assert_eq!(decide(false, Access::Anyone, false), Err(Refusal::Off));
    }
}
