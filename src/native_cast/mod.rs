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
