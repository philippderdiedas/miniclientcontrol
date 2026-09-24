//! The provider this device signs in with, and what its groups mean here.

use serde::{Deserialize, Serialize};

use crate::models::AppState;

const KEY: &str = "oidc";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Admin,
    Manager,
    Editor,
    /// A cast session and no account.
    Cast,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMapping {
    pub group: String,
    pub target: Target,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub groups_claim: String,
    pub label: String,
    pub mapping: Vec<GroupMapping>,
    pub everyone_casts: bool,
    pub local_passwords: bool,
}

impl Default for OidcConfig {
    fn default() -> Self {
        Self {
            issuer: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            groups_claim: "groups".into(),
            label: String::new(),
            mapping: Vec::new(),
            everyone_casts: false,
            local_passwords: true,
        }
    }
}

impl OidcConfig {
    pub fn configured(&self) -> bool {
        !self.issuer.is_empty() && !self.client_id.is_empty()
    }

    /// What the login button says: the configured label, else the issuer's host.
    pub fn button_label(&self) -> String {
        if !self.label.trim().is_empty() {
            return self.label.trim().to_string();
        }
        url::Url::parse(&self.issuer)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| "SSO".into())
    }
}

/// The callback this device registers with the provider, built from the one
/// place that answers what the device is called (`cast::sender_url`).
pub fn redirect_uri(state: &AppState) -> String {
    let sender = crate::cast::sender_url(state, None);
    match url::Url::parse(&sender) {
        Ok(url) => format!("{}/api/oidc/callback", url.origin().ascii_serialization()),
        Err(_) => format!("{}/api/oidc/callback", sender.trim_end_matches('/')),
    }
}

/// Whether the device's name survives a DHCP move. A managed certificate's
/// name follows the address, and so does a bare LAN address -- but only the
/// former is the same *name*; `--public-url` pins one. A provider refuses a
/// redirect URI that changed, so the admin card warns when this is false.
pub fn redirect_stable(state: &AppState) -> bool {
    state.managed_cert
        || !matches!(crate::tls::public_url(&state.args.public_url), crate::tls::PublicUrl::LanAddress)
}

pub async fn load(pool: &sqlx::SqlitePool) -> OidcConfig {
    crate::db::load_setting(pool, KEY)
        .await
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub async fn save(pool: &sqlx::SqlitePool, config: &OidcConfig) -> anyhow::Result<()> {
    crate::db::save_setting(pool, KEY, &serde_json::to_string(config)?).await
}
