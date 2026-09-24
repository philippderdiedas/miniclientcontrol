//! The authorization-code flow: start, callback, and what a sign-in becomes --
//! an account for a mapped role, a cast session for a cast-only mapping, or a
//! refusal.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::Value;

use super::config::{OidcConfig, Target};
use super::jwt::Jwk;

/// The cookie a cast session travels under. Its own name, so the ordinary
/// session lookup never reads it.
pub const CAST_COOKIE: &str = "mcc_cast";
/// Binds a `state` to the browser that started the sign-in.
pub const BINDING_COOKIE: &str = "mcc_oidc";
pub const PENDING_TTL: Duration = Duration::from_secs(600);
const CAST_SESSION_LIFETIME: &str = "+12 hours";

/// A sign-in on its way through the provider, keyed by its `state`.
pub struct Pending {
    pub nonce: String,
    pub verifier: String,
    pub next: String,
    /// Must equal the `mcc_oidc` cookie on the callback: a `state` alone could
    /// be replayed into another browser.
    pub binding: String,
    pub created: Instant,
}

pub struct Discovery {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    /// `client_secret_basic` unless the provider says it only takes the post form.
    pub basic_auth: bool,
}

/// Discovery and keys, cached until the configuration changes.
static CACHE: tokio::sync::Mutex<Option<(String, std::sync::Arc<Discovery>, Vec<Jwk>)>> =
    tokio::sync::Mutex::const_new(None);

pub async fn forget_discovery() {
    *CACHE.lock().await = None;
}

fn keys_from(value: &Value) -> Vec<Jwk> {
    value
        .get("keys")
        .and_then(|keys| serde_json::from_value(keys.clone()).ok())
        .unwrap_or_default()
}

/// The provider's endpoints and keys, from cache or fetched.
pub async fn discover(config: &OidcConfig) -> Result<(std::sync::Arc<Discovery>, Vec<Jwk>)> {
    if let Some((issuer, discovery, keys)) = CACHE.lock().await.as_ref() {
        if issuer == &config.issuer {
            return Ok((discovery.clone(), keys.clone()));
        }
    }
    let doc = super::http::get_json(&format!("{}/.well-known/openid-configuration", config.issuer)).await?;
    let text = |key: &str| doc.get(key).and_then(Value::as_str).map(str::to_string);
    // A provider that disagrees about its own name is misconfigured, or not the
    // provider this device was set up for.
    if text("issuer").as_deref().map(|i| i.trim_end_matches('/')) != Some(config.issuer.as_str()) {
        return Err(anyhow!("the provider names itself differently than configured"));
    }
    let discovery = Discovery {
        authorization_endpoint: text("authorization_endpoint").ok_or_else(|| anyhow!("no authorization_endpoint"))?,
        token_endpoint: text("token_endpoint").ok_or_else(|| anyhow!("no token_endpoint"))?,
        jwks_uri: text("jwks_uri").ok_or_else(|| anyhow!("no jwks_uri"))?,
        basic_auth: doc
            .get("token_endpoint_auth_methods_supported")
            .and_then(Value::as_array)
            .map(|methods| methods.iter().any(|m| m == "client_secret_basic"))
            .unwrap_or(true),
    };
    for url in [&discovery.authorization_endpoint, &discovery.token_endpoint, &discovery.jwks_uri] {
        if !super::http::allowed_url(url) {
            return Err(anyhow!("the provider points at {url}, which is not https"));
        }
    }
    let keys = keys_from(&super::http::get_json(&discovery.jwks_uri).await?);
    let discovery = std::sync::Arc::new(discovery);
    *CACHE.lock().await = Some((config.issuer.clone(), discovery.clone(), keys.clone()));
    Ok((discovery, keys))
}

/// The key set again, once, for a token naming a key the cache does not have --
/// providers rotate keys.
pub async fn refresh_keys(config: &OidcConfig, discovery: &Discovery) -> Result<Vec<Jwk>> {
    let keys = keys_from(&super::http::get_json(&discovery.jwks_uri).await?);
    if let Some(entry) = CACHE.lock().await.as_mut() {
        if entry.0 == config.issuer {
            entry.2 = keys.clone();
        }
    }
    Ok(keys)
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()))
}

/// Only a path on this host: the rule the login page applies to `next`.
pub fn safe_next(next: Option<String>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") && !n.contains('\\') => n,
        _ => "/admin.html".into(),
    }
}

/// What the groups mean here: the highest role any of them maps to; a
/// cast session only when none maps to a role; `everyone_casts` when nothing
/// matched at all.
pub fn decide(groups: &[String], config: &OidcConfig) -> Option<Target> {
    let matched: Vec<Target> = config
        .mapping
        .iter()
        .filter(|m| groups.iter().any(|g| g == &m.group))
        .map(|m| m.target)
        .collect();
    for wanted in [Target::Admin, Target::Manager, Target::Editor, Target::Cast] {
        if matched.contains(&wanted) {
            return Some(wanted);
        }
    }
    config.everyone_casts.then_some(Target::Cast)
}

/// The groups claim as strings; absent or of another shape means none.
pub fn groups_of(claims: &Value, claim: &str) -> Vec<String> {
    match claims.get(claim) {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Some(Value::String(one)) => vec![one.clone()],
        _ => Vec::new(),
    }
}

/// What to call the person: the provider's username, else their name, else
/// the subject.
pub fn display_name(claims: &Value) -> String {
    ["preferred_username", "name", "sub"]
        .iter()
        .find_map(|key| claims.get(*key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()))
        .unwrap_or("sso")
        .to_string()
}

/// The account linked to this provider identity, created or brought up to
/// date. Linked by (issuer, subject) only; a name already taken gets `-sso`,
/// `-sso2`, ... so a provider account can never become a local one.
pub async fn account_for(
    pool: &sqlx::SqlitePool,
    issuer: &str,
    subject: &str,
    preferred: &str,
    role: crate::accounts::Role,
) -> Result<(i64, String)> {
    let mut tx = pool.begin().await?;
    let linked: Option<(i64, String)> = sqlx::query_as(
        "SELECT u.id, u.name FROM user_identities i JOIN users u ON u.id = i.user_id
         WHERE i.issuer = ? AND i.subject = ?",
    )
    .bind(issuer)
    .bind(subject)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((id, name)) = linked {
        sqlx::query("UPDATE users SET role = ?, disabled = 0 WHERE id = ?")
            .bind(role.as_str())
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok((id, name));
    }
    let base = preferred.trim();
    let base = if base.is_empty() { subject } else { base };
    let mut candidate = base.to_string();
    for n in 1.. {
        let taken: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE name = ?")
            .bind(&candidate)
            .fetch_one(&mut *tx)
            .await?;
        if taken == 0 {
            break;
        }
        candidate = if n == 1 { format!("{base}-sso") } else { format!("{base}-sso{n}") };
    }
    // An empty hash is "no password": `verify_hash` refuses it, so neither the
    // login form nor Basic can reach an SSO account.
    let id = sqlx::query("INSERT INTO users (name, password_hash, role) VALUES (?, '', ?)")
        .bind(&candidate)
        .bind(role.as_str())
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
    sqlx::query("INSERT INTO user_identities (issuer, subject, user_id) VALUES (?, ?, ?)")
        .bind(issuer)
        .bind(subject)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((id, candidate))
}

/// The person left every mapped group: their account stops working now, not
/// whenever they happen to sign in again.
pub async fn disable_linked(pool: &sqlx::SqlitePool, issuer: &str, subject: &str) {
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT user_id FROM user_identities WHERE issuer = ? AND subject = ?",
    )
    .bind(issuer)
    .bind(subject)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);
    if let Some(id) = id {
        let _ = sqlx::query("UPDATE users SET disabled = 1 WHERE id = ?").bind(id).execute(pool).await;
        let _ = sqlx::query("DELETE FROM sessions WHERE user_id = ?").bind(id).execute(pool).await;
    }
}

/// Start a cast session and return the cookie value. Only its SHA-256 is stored.
pub async fn create_cast_session(pool: &sqlx::SqlitePool, name: &str, issuer: &str, subject: &str) -> String {
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut bytes);
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    if let Err(e) = sqlx::query(
        "INSERT INTO cast_sessions (token_hash, name, issuer, subject, expires_at)
         VALUES (?, ?, ?, ?, datetime('now', ?))",
    )
    .bind(crate::accounts::sha256_hex(&token))
    .bind(name)
    .bind(issuer)
    .bind(subject)
    .bind(CAST_SESSION_LIFETIME)
    .execute(pool)
    .await
    {
        tracing::error!("Failed to store a cast session: {}", e);
    }
    token
}

/// The person behind a cast session, sliding its expiry.
pub async fn cast_session_name(pool: &sqlx::SqlitePool, token: &str) -> Option<String> {
    let hash = crate::accounts::sha256_hex(token);
    let name: Option<String> = sqlx::query_scalar(
        "SELECT name FROM cast_sessions WHERE token_hash = ? AND expires_at > datetime('now')",
    )
    .bind(&hash)
    .fetch_optional(pool)
    .await
    .ok()?;
    let _ = sqlx::query("UPDATE cast_sessions SET expires_at = datetime('now', ?) WHERE token_hash = ?")
        .bind(CAST_SESSION_LIFETIME)
        .bind(&hash)
        .execute(pool)
        .await;
    name
}

/// Whether an enabled admin is linked to the provider -- the condition for
/// switching local passwords off without locking the venue out.
pub async fn sso_admin_exists(pool: &sqlx::SqlitePool) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM user_identities i JOIN users u ON u.id = i.user_id
         WHERE u.role = 'admin' AND u.disabled = 0",
    )
    .fetch_one(pool)
    .await
    .map(|n| n > 0)
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oidc::config::GroupMapping;

    fn config(everyone: bool) -> OidcConfig {
        OidcConfig {
            mapping: vec![
                GroupMapping { group: "staff".into(), target: Target::Editor },
                GroupMapping { group: "boss".into(), target: Target::Admin },
                GroupMapping { group: "members".into(), target: Target::Cast },
            ],
            everyone_casts: everyone,
            ..OidcConfig::default()
        }
    }

    fn g(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_highest_role_wins_and_cast_only_without_one() {
        assert_eq!(decide(&g(&["staff", "boss"]), &config(false)), Some(Target::Admin));
        assert_eq!(decide(&g(&["members", "staff"]), &config(false)), Some(Target::Editor));
        assert_eq!(decide(&g(&["members"]), &config(false)), Some(Target::Cast));
        assert_eq!(decide(&g(&["other"]), &config(false)), None);
        assert_eq!(decide(&g(&[]), &config(true)), Some(Target::Cast));
    }

    #[test]
    fn next_stays_on_this_host() {
        assert_eq!(safe_next(Some("/playlist.html".into())), "/playlist.html");
        assert_eq!(safe_next(Some("//evil.test/x".into())), "/admin.html");
        assert_eq!(safe_next(Some("https://evil.test/".into())), "/admin.html");
        assert_eq!(safe_next(Some("/\\evil.test".into())), "/admin.html");
        assert_eq!(safe_next(None), "/admin.html");
    }

    #[test]
    fn the_challenge_is_rfc7636() {
        // RFC 7636 appendix B.
        assert_eq!(challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
                   "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }
}
