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

/// Start one native-cast receiver per declared display. Receivers live until the
/// process ends (like the browser the controller does not kill), so their handles
/// are deliberately leaked with `std::mem::forget` -- SP3+ may hold them in state
/// for teardown.
pub fn supervise(state: AppState, configured: Vec<crate::display::DisplayConfig>) {
    let cast_db = state.args.cast_db_path();
    let db_str = cast_db.to_string_lossy().to_string();

    // One shared identity -> a base device id; each display gets a unique suffix,
    // so the mDNS device ids differ while the cast identity is shared.
    let base = match casttarget::certstore::load_identity(&db_str) {
        Ok(id) => casttarget::cast::stable_device_id(&id.device_cert_der),
        Err(e) => {
            tracing::error!("native-cast: cast.db ({}) nicht ladbar: {:#} -- Receiver aus", db_str, e);
            return;
        }
    };

    for (index, cfg) in configured.into_iter().enumerate() {
        let screen = cfg.name.clone();
        let port = BASE_CAST_PORT + index as u16;
        let app_id = format!("casttarget-{}", cfg.name);
        let device_id = format!("{base}-{index}");

        let hook = Arc::new(NativeCastHook {
            handle: tokio::runtime::Handle::current(),
            pool: state.pool.clone(),
            screen: screen.clone(),
            settings: state.settings.clone(),
        });

        let (tx, rx) = std::sync::mpsc::sync_channel::<casttarget::CastEvent>(32);

        // Drain the channel off the runtime; SP3 turns these into overrides.
        let drain_screen = screen.clone();
        tokio::task::spawn_blocking(move || {
            while let Ok(ev) = rx.recv() {
                match ev {
                    casttarget::CastEvent::Started { app_id } => {
                        tracing::info!("native-cast: start auf {} (app_id={})", drain_screen, app_id)
                    }
                    casttarget::CastEvent::Ended => {
                        tracing::info!("native-cast: ende auf {}", drain_screen)
                    }
                }
            }
        });

        let fetch = index == 0; // only one receiver refetches the shared cast.db
        match casttarget::start(
            casttarget::ReceiverConfig {
                name: cfg.name.clone(),
                port,
                device_id,
                app_id,
                cast_db: cast_db.clone(),
                fetch,
            },
            hook,
            tx,
        ) {
            Ok(handle) => {
                tracing::info!("native-cast: '{}' auf Port {} announced", screen, port);
                std::mem::forget(handle); // lives until process exit
            }
            Err(e) => tracing::error!("native-cast: Receiver '{}' nicht gestartet: {:#}", screen, e),
        }
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
