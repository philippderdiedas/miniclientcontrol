//! Accounts: who may sign in, with which role, and their sessions.
//!
//! Storage only; the middleware that turns a request into an [`Identity`] is
//! `middleware.rs`, the table of which role a route needs is `roles.rs`.

pub mod api;
pub mod middleware;
pub mod roles;

use rand::RngCore;
use serde::Serialize;

/// The three roles, ordered so a higher one includes a lower one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Editor,
    Manager,
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Manager => "manager",
            Role::Editor => "editor",
        }
    }

    pub fn parse(raw: &str) -> Option<Role> {
        match raw {
            "admin" => Some(Role::Admin),
            "manager" => Some(Role::Manager),
            "editor" => Some(Role::Editor),
            _ => None,
        }
    }
}

/// Who a request is. `user_id` is `None` for the command-line recovery admin
/// and in open mode, where `open` says no account exists at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Identity {
    #[serde(skip)]
    pub user_id: Option<i64>,
    pub name: String,
    pub role: Role,
    pub open: bool,
}

impl Identity {
    /// What every request is while no account exists -- exactly as a device
    /// without credentials behaves today.
    pub fn open_mode() -> Identity {
        Identity { user_id: None, name: String::new(), role: Role::Admin, open: true }
    }

    pub fn rescue(name: &str) -> Identity {
        Identity { user_id: None, name: name.to_string(), role: Role::Admin, open: false }
    }
}

#[derive(Debug)]
pub enum AccountError {
    Exists,
    FirstMustBeAdmin,
    TooShort,
    LastAdmin,
    Unknown,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for AccountError {
    fn from(e: sqlx::Error) -> Self {
        AccountError::Db(e)
    }
}

impl AccountError {
    /// What the operator is told.
    pub fn message(&self) -> &'static str {
        match self {
            AccountError::Exists => "Ein Konto mit diesem Namen gibt es schon.",
            AccountError::FirstMustBeAdmin => "Das erste Konto muss ein Admin sein.",
            AccountError::TooShort => "Das Passwort muss mindestens 8 Zeichen haben.",
            AccountError::LastAdmin => "Das letzte aktive Admin-Konto kann nicht entfernt, deaktiviert oder herabgestuft werden.",
            AccountError::Unknown => "Dieses Konto gibt es nicht.",
            AccountError::Db(_) => "Konto konnte nicht gespeichert werden.",
        }
    }
}

const MIN_PASSWORD: usize = 8;
const SESSION_LIFETIME: &str = "+12 hours";

fn sha256_hex(text: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, text.as_bytes());
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

pub async fn any_user(pool: &sqlx::SqlitePool) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users")
        .fetch_one(pool)
        .await
        .map(|n| n > 0)
        // An unreadable table must not open the device: fail closed.
        .unwrap_or(true)
}

async fn enabled_admins(conn: &mut sqlx::SqliteConnection) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM users WHERE role = 'admin' AND disabled = 0")
        .fetch_one(conn)
        .await
}

pub async fn create_user(
    pool: &sqlx::SqlitePool,
    name: &str,
    password: &str,
    role: Role,
) -> Result<i64, AccountError> {
    let name = name.trim();
    if password.chars().count() < MIN_PASSWORD {
        return Err(AccountError::TooShort);
    }
    let mut tx = pool.begin().await?;
    let existing: i64 = sqlx::query_scalar("SELECT count(*) FROM users").fetch_one(&mut *tx).await?;
    if existing == 0 && role != Role::Admin {
        return Err(AccountError::FirstMustBeAdmin);
    }
    let inserted = sqlx::query(
        "INSERT INTO users (name, password_hash, role) VALUES (?, ?, ?) ON CONFLICT(name) DO NOTHING",
    )
    .bind(name)
    .bind(crate::settings::hash_password(password))
    .bind(role.as_str())
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(AccountError::Exists);
    }
    let id = inserted.last_insert_rowid();
    tx.commit().await?;
    Ok(id)
}

fn identity_from(id: i64, name: String, role: &str) -> Option<Identity> {
    Some(Identity { user_id: Some(id), name, role: Role::parse(role)?, open: false })
}

pub async fn verify_password(pool: &sqlx::SqlitePool, name: &str, password: &str) -> Option<Identity> {
    let row: (i64, String, String, String) = sqlx::query_as(
        "SELECT id, name, password_hash, role FROM users WHERE name = ? AND disabled = 0",
    )
    .bind(name.trim())
    .fetch_optional(pool)
    .await
    .ok()??;
    let (id, name, hash, role) = row;
    crate::settings::verify_hash(&hash, password).then_some(())?;
    identity_from(id, name, &role)
}

/// Start a session and return the cookie value. Only its SHA-256 is stored.
pub async fn create_session(pool: &sqlx::SqlitePool, user_id: i64) -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    if let Err(e) = sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, expires_at) VALUES (?, ?, datetime('now', ?))",
    )
    .bind(sha256_hex(&token))
    .bind(user_id)
    .bind(SESSION_LIFETIME)
    .execute(pool)
    .await
    {
        tracing::error!("Failed to store a session: {}", e);
    }
    token
}

/// The account behind a session cookie, sliding its expiry. Expired sessions
/// are dropped on the way.
pub async fn session_identity(pool: &sqlx::SqlitePool, token: &str) -> Option<Identity> {
    let hash = sha256_hex(token);
    let row: (i64, String, String) = sqlx::query_as(
        "SELECT u.id, u.name, u.role FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token_hash = ? AND s.expires_at > datetime('now') AND u.disabled = 0",
    )
    .bind(&hash)
    .fetch_optional(pool)
    .await
    .ok()??;
    let _ = sqlx::query("UPDATE sessions SET expires_at = datetime('now', ?) WHERE token_hash = ?")
        .bind(SESSION_LIFETIME)
        .bind(&hash)
        .execute(pool)
        .await;
    identity_from(row.0, row.1, &row.2)
}

pub async fn end_session(pool: &sqlx::SqlitePool, token: &str) {
    let _ = sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
        .bind(sha256_hex(token))
        .execute(pool)
        .await;
}

/// Change an account. Refused when it would leave no enabled admin -- a device
/// only the command line could open again.
pub async fn update_user(
    pool: &sqlx::SqlitePool,
    id: i64,
    role: Option<Role>,
    password: Option<&str>,
    disabled: Option<bool>,
) -> Result<(), AccountError> {
    if let Some(password) = password {
        if password.chars().count() < MIN_PASSWORD {
            return Err(AccountError::TooShort);
        }
    }
    let mut tx = pool.begin().await?;
    let current: Option<String> = sqlx::query_scalar("SELECT role FROM users WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(current) = current else {
        return Err(AccountError::Unknown);
    };
    if let Some(role) = role {
        sqlx::query("UPDATE users SET role = ? WHERE id = ?").bind(role.as_str()).bind(id).execute(&mut *tx).await?;
    }
    if let Some(password) = password {
        sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
            .bind(crate::settings::hash_password(password))
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(disabled) = disabled {
        sqlx::query("UPDATE users SET disabled = ? WHERE id = ?").bind(disabled).bind(id).execute(&mut *tx).await?;
    }
    // Checked after the writes, inside the transaction: the question is whether
    // the state this would commit still has an admin.
    if current == "admin" && enabled_admins(&mut tx).await? == 0 {
        return Err(AccountError::LastAdmin);
    }
    sqlx::query("DELETE FROM sessions WHERE user_id = ?").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn delete_user(pool: &sqlx::SqlitePool, id: i64) -> Result<(), AccountError> {
    let mut tx = pool.begin().await?;
    let deleted = sqlx::query("DELETE FROM users WHERE id = ?").bind(id).execute(&mut *tx).await?;
    if deleted.rows_affected() == 0 {
        return Err(AccountError::Unknown);
    }
    if enabled_admins(&mut tx).await? == 0 {
        return Err(AccountError::LastAdmin);
    }
    tx.commit().await?;
    Ok(())
}

/// The credential stored before accounts existed becomes the first admin, once:
/// only while no account exists, and the settings keys go in the same
/// transaction, so a restart can neither repeat it nor lose it half-way.
pub async fn adopt_stored_credential(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    let existing: i64 = sqlx::query_scalar("SELECT count(*) FROM users").fetch_one(&mut *tx).await?;
    if existing > 0 {
        return Ok(());
    }
    let user: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = 'basic_auth_user'")
        .fetch_optional(&mut *tx).await?;
    let hash: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = 'basic_auth_hash'")
        .fetch_optional(&mut *tx).await?;
    let (Some(user), Some(hash)) = (user, hash) else {
        return Ok(());
    };
    if user.trim().is_empty() || hash.is_empty() {
        return Ok(());
    }
    sqlx::query("INSERT INTO users (name, password_hash, role) VALUES (?, ?, 'admin')")
        .bind(user.trim())
        .bind(&hash)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM settings WHERE key IN ('basic_auth_user', 'basic_auth_hash')")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!("The stored operator credential '{}' is now the first admin account", user.trim());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool(name: &str) -> sqlx::SqlitePool {
        let pool = sqlx::SqlitePool::connect(&format!("sqlite:file:{name}?mode=memory&cache=shared"))
            .await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn the_first_account_must_be_an_admin() {
        let pool = pool("acc_first_admin").await;
        assert!(!any_user(&pool).await);
        assert!(matches!(create_user(&pool, "ed", "longenough", Role::Editor).await,
                         Err(AccountError::FirstMustBeAdmin)));
        create_user(&pool, "root", "longenough", Role::Admin).await.unwrap();
        assert!(any_user(&pool).await);
        create_user(&pool, "ed", "longenough", Role::Editor).await.unwrap();
        assert!(matches!(create_user(&pool, "ed", "longenough", Role::Editor).await,
                         Err(AccountError::Exists)));
        assert!(matches!(create_user(&pool, "x", "short", Role::Editor).await,
                         Err(AccountError::TooShort)));
    }

    #[tokio::test]
    async fn a_password_verifies_and_a_disabled_account_does_not() {
        let pool = pool("acc_verify").await;
        let id = create_user(&pool, "root", "longenough", Role::Admin).await.unwrap();
        let ed = create_user(&pool, "ed", "longenough", Role::Editor).await.unwrap();
        let who = verify_password(&pool, "root", "longenough").await.unwrap();
        assert_eq!((who.user_id, who.role, who.open), (Some(id), Role::Admin, false));
        assert!(verify_password(&pool, "root", "wrong-password").await.is_none());
        update_user(&pool, ed, None, None, Some(true)).await.unwrap();
        assert!(verify_password(&pool, "ed", "longenough").await.is_none());
    }

    #[tokio::test]
    async fn a_session_resolves_until_it_is_ended() {
        let pool = pool("acc_session").await;
        let id = create_user(&pool, "root", "longenough", Role::Admin).await.unwrap();
        let token = create_session(&pool, id).await;
        assert_eq!(token.len(), 64);
        assert_eq!(session_identity(&pool, &token).await.unwrap().user_id, Some(id));
        assert!(session_identity(&pool, "not-a-token").await.is_none());
        // Stored hashed: the value itself is nowhere in the table.
        let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE token_hash = ?")
            .bind(&token).fetch_one(&pool).await.unwrap();
        assert_eq!(stored, 0);
        end_session(&pool, &token).await;
        assert!(session_identity(&pool, &token).await.is_none());
    }

    #[tokio::test]
    async fn an_expired_session_is_gone() {
        let pool = pool("acc_expired").await;
        let id = create_user(&pool, "root", "longenough", Role::Admin).await.unwrap();
        let token = create_session(&pool, id).await;
        sqlx::query("UPDATE sessions SET expires_at = datetime('now', '-1 minute')")
            .execute(&pool).await.unwrap();
        assert!(session_identity(&pool, &token).await.is_none());
    }

    #[tokio::test]
    async fn changing_a_password_or_role_ends_that_accounts_sessions() {
        let pool = pool("acc_change_ends").await;
        create_user(&pool, "root", "longenough", Role::Admin).await.unwrap();
        let ed = create_user(&pool, "ed", "longenough", Role::Editor).await.unwrap();
        let token = create_session(&pool, ed).await;
        update_user(&pool, ed, Some(Role::Manager), None, None).await.unwrap();
        assert!(session_identity(&pool, &token).await.is_none());
    }

    #[tokio::test]
    async fn the_last_admin_cannot_be_removed_disabled_or_demoted() {
        let pool = pool("acc_last_admin").await;
        let root = create_user(&pool, "root", "longenough", Role::Admin).await.unwrap();
        assert!(matches!(delete_user(&pool, root).await, Err(AccountError::LastAdmin)));
        assert!(matches!(update_user(&pool, root, None, None, Some(true)).await, Err(AccountError::LastAdmin)));
        assert!(matches!(update_user(&pool, root, Some(Role::Manager), None, None).await, Err(AccountError::LastAdmin)));
        let second = create_user(&pool, "two", "longenough", Role::Admin).await.unwrap();
        delete_user(&pool, root).await.unwrap();
        assert!(matches!(delete_user(&pool, second).await, Err(AccountError::LastAdmin)));
    }

    #[tokio::test]
    async fn a_stored_credential_becomes_the_first_admin() {
        let pool = pool("acc_adopt").await;
        let hash = crate::settings::hash_password("hunter2!!");
        crate::db::save_setting(&pool, "basic_auth_user", "ops").await.unwrap();
        crate::db::save_setting(&pool, "basic_auth_hash", &hash).await.unwrap();
        adopt_stored_credential(&pool).await.unwrap();
        let who = verify_password(&pool, "ops", "hunter2!!").await.unwrap();
        assert_eq!(who.role, Role::Admin);
        assert!(crate::db::load_setting(&pool, "basic_auth_user").await.unwrap_or_default().is_empty());
        adopt_stored_credential(&pool).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users").fetch_one(&pool).await.unwrap();
        assert_eq!(count, 1);
    }
}
