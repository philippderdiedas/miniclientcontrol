//! API tokens: a bearer credential bound to an account, for scripts and the
//! MCP endpoint. Only the SHA-256 is stored, like a session's. A token carries
//! no role of its own -- it is its account, with the role that account has at
//! the moment of each request.

use rand::RngCore;
use serde::Serialize;

use super::{identity_from, sha256_hex, Identity};

/// What every token starts with, so one pasted into a log or a chat is
/// recognisable for what it is.
pub const PREFIX: &str = "mcc_";
const MAX_NAME: usize = 100;
/// The longest lifetime a token may be given; `None` means it does not expire.
const MAX_DAYS: u32 = 3650;

#[derive(Debug, Serialize)]
pub struct TokenInfo {
    pub id: i64,
    pub name: String,
    /// Whose it is -- only in the admin's list of every token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub created_at: Option<String>,
    pub last_used_at: Option<String>,
    pub expires_at: Option<String>,
}

#[derive(Debug)]
pub enum TokenError {
    EmptyName,
    BadLifetime,
    Db(sqlx::Error),
}

impl TokenError {
    pub fn message(&self) -> &'static str {
        match self {
            TokenError::EmptyName => "Der Token braucht einen Namen.",
            TokenError::BadLifetime => "Die Laufzeit muss zwischen 1 und 3650 Tagen liegen.",
            TokenError::Db(_) => "Token konnte nicht gespeichert werden.",
        }
    }
}

fn clean_name(name: &str) -> Result<String, TokenError> {
    let name: String = name.trim().chars().take(MAX_NAME).collect();
    if name.is_empty() {
        return Err(TokenError::EmptyName);
    }
    Ok(name)
}

fn lifetime(days: Option<u32>) -> Result<Option<String>, TokenError> {
    if days.is_some_and(|d| d == 0 || d > MAX_DAYS) {
        return Err(TokenError::BadLifetime);
    }
    Ok(days.map(|d| format!("+{d} days")))
}

/// Mint a token for `user_id` and return `(id, secret)`. The secret is shown
/// once and never again.
pub async fn create(
    pool: &sqlx::SqlitePool,
    user_id: i64,
    name: &str,
    days: Option<u32>,
) -> Result<(i64, String), TokenError> {
    let name = clean_name(name)?;
    let expires = lifetime(days)?;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let secret = format!("{PREFIX}{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let inserted = sqlx::query(
        "INSERT INTO api_tokens (user_id, name, token_hash, expires_at)
         VALUES (?, ?, ?, CASE WHEN ? IS NULL THEN NULL ELSE datetime('now', ?) END)",
    )
    .bind(user_id)
    .bind(&name)
    .bind(sha256_hex(&secret))
    .bind(&expires)
    .bind(&expires)
    .execute(pool)
    .await
    .map_err(TokenError::Db)?;
    Ok((inserted.last_insert_rowid(), secret))
}

/// Which tokens a call may touch: one account's, or -- for an admin -- all.
#[derive(Debug, Clone, Copy)]
pub enum Scope {
    Own(i64),
    All,
}

impl Scope {
    /// The `user_id` to match, `NULL` meaning any: `user_id = coalesce(?, user_id)`.
    fn owner(self) -> Option<i64> {
        match self {
            Scope::Own(id) => Some(id),
            Scope::All => None,
        }
    }
}

pub async fn list(pool: &sqlx::SqlitePool, scope: Scope) -> Result<Vec<TokenInfo>, sqlx::Error> {
    let rows: Vec<(i64, String, String, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT t.id, t.name, u.name, t.created_at, t.last_used_at, t.expires_at
         FROM api_tokens t JOIN users u ON u.id = t.user_id
         WHERE t.user_id = coalesce(?, t.user_id) ORDER BY u.name ASC, t.id ASC",
    )
    .bind(scope.owner())
    .fetch_all(pool)
    .await?;
    let all = matches!(scope, Scope::All);
    Ok(rows
        .into_iter()
        .map(|(id, name, owner, created_at, last_used_at, expires_at)| TokenInfo {
            id,
            name,
            owner: all.then_some(owner),
            created_at,
            last_used_at,
            expires_at,
        })
        .collect())
}

/// Rename a token and/or give it a new lifetime, counted from now (`Some(None)`
/// for never). `Ok(false)` when the scope holds no such token -- someone
/// else's id is not found, rather than revealed.
pub async fn update(
    pool: &sqlx::SqlitePool,
    scope: Scope,
    id: i64,
    name: Option<&str>,
    days: Option<Option<u32>>,
) -> Result<bool, TokenError> {
    let name = name.map(clean_name).transpose()?;
    let expires = days.map(lifetime).transpose()?;
    // One statement, so a rename and a new lifetime land together or not at all.
    let changed = sqlx::query(
        "UPDATE api_tokens SET
            name = coalesce(?, name),
            expires_at = CASE WHEN ? THEN
                (CASE WHEN ? IS NULL THEN NULL ELSE datetime('now', ?) END)
                ELSE expires_at END
         WHERE id = ? AND user_id = coalesce(?, user_id)",
    )
    .bind(&name)
    .bind(expires.is_some())
    .bind(expires.as_ref().and_then(|e| e.as_ref()))
    .bind(expires.as_ref().and_then(|e| e.as_ref()))
    .bind(id)
    .bind(scope.owner())
    .execute(pool)
    .await
    .map_err(TokenError::Db)?;
    Ok(changed.rows_affected() > 0)
}

/// Revoke a token within `scope`. `false` when the scope holds no such token.
pub async fn revoke(pool: &sqlx::SqlitePool, scope: Scope, id: i64) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query("DELETE FROM api_tokens WHERE id = ? AND user_id = coalesce(?, user_id)")
        .bind(id)
        .bind(scope.owner())
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected() > 0)
}

/// The account behind a bearer token, if the token is live and its account
/// enabled. Not cached, so a revocation is honoured on the very next request.
pub async fn identity(pool: &sqlx::SqlitePool, secret: &str) -> Option<Identity> {
    if !secret.starts_with(PREFIX) {
        return None;
    }
    let hash = sha256_hex(secret);
    let row: (i64, String, String) = sqlx::query_as(
        "SELECT u.id, u.name, u.role FROM api_tokens t JOIN users u ON u.id = t.user_id
         WHERE t.token_hash = ? AND u.disabled = 0
           AND (t.expires_at IS NULL OR t.expires_at > datetime('now'))",
    )
    .bind(&hash)
    .fetch_optional(pool)
    .await
    .ok()??;
    // At most one write a minute per token: an LLM client makes many calls in
    // a row, and the database lives on an SD card. A matching-nothing UPDATE
    // writes no page.
    let _ = sqlx::query(
        "UPDATE api_tokens SET last_used_at = datetime('now') WHERE token_hash = ?
         AND (last_used_at IS NULL OR last_used_at < datetime('now', '-1 minute'))",
    )
    .bind(&hash)
    .execute(pool)
    .await;
    identity_from(row.0, row.1, &row.2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{create_user, update_user, delete_user, Role};

    async fn pool(name: &str) -> sqlx::SqlitePool {
        let pool = sqlx::SqlitePool::connect(&format!("sqlite:file:{name}?mode=memory&cache=shared"))
            .await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn a_token_is_its_account_with_the_role_it_has_now() {
        let pool = pool("tok_role").await;
        create_user(&pool, "root", "longenough", Role::Admin).await.unwrap();
        let ed = create_user(&pool, "llm", "longenough", Role::Editor).await.unwrap();
        let (_, secret) = create(&pool, ed, "claude", None).await.unwrap();
        assert!(secret.starts_with(PREFIX));
        let who = identity(&pool, &secret).await.unwrap();
        assert_eq!((who.user_id, who.role), (Some(ed), Role::Editor));
        update_user(&pool, ed, Some(Role::Manager), None, None).await.unwrap();
        assert_eq!(identity(&pool, &secret).await.unwrap().role, Role::Manager);
        // Stored hashed.
        let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM api_tokens WHERE token_hash = ?")
            .bind(&secret).fetch_one(&pool).await.unwrap();
        assert_eq!(stored, 0);
    }

    #[tokio::test]
    async fn a_disabled_or_deleted_account_takes_its_tokens_with_it() {
        let pool = pool("tok_disabled").await;
        create_user(&pool, "root", "longenough", Role::Admin).await.unwrap();
        let ed = create_user(&pool, "llm", "longenough", Role::Editor).await.unwrap();
        let (_, secret) = create(&pool, ed, "claude", None).await.unwrap();
        update_user(&pool, ed, None, None, Some(true)).await.unwrap();
        assert!(identity(&pool, &secret).await.is_none());
        update_user(&pool, ed, None, None, Some(false)).await.unwrap();
        assert!(identity(&pool, &secret).await.is_some());
        delete_user(&pool, ed).await.unwrap();
        assert!(identity(&pool, &secret).await.is_none());
    }

    #[tokio::test]
    async fn a_revoked_or_expired_token_is_refused() {
        let pool = pool("tok_revoke").await;
        let root = create_user(&pool, "root", "longenough", Role::Admin).await.unwrap();
        let other = create_user(&pool, "other", "longenough", Role::Editor).await.unwrap();
        let (id, secret) = create(&pool, root, "a", Some(30)).await.unwrap();
        assert!(identity(&pool, &secret).await.is_some());
        // Another account cannot revoke it, and is not told it exists.
        assert!(!revoke(&pool, Scope::Own(other), id).await.unwrap());
        assert!(revoke(&pool, Scope::Own(root), id).await.unwrap());
        assert!(identity(&pool, &secret).await.is_none());

        let (_, expiring) = create(&pool, root, "b", Some(1)).await.unwrap();
        sqlx::query("UPDATE api_tokens SET expires_at = datetime('now', '-1 minute')")
            .execute(&pool).await.unwrap();
        assert!(identity(&pool, &expiring).await.is_none());
    }

    #[tokio::test]
    async fn an_admin_scope_reaches_every_token_and_an_own_scope_only_its_own() {
        let pool = pool("tok_scope").await;
        let root = create_user(&pool, "root", "longenough", Role::Admin).await.unwrap();
        let ed = create_user(&pool, "ed", "longenough", Role::Editor).await.unwrap();
        let (mine, _) = create(&pool, root, "mine", None).await.unwrap();
        let (theirs, secret) = create(&pool, ed, "theirs", None).await.unwrap();

        let own = list(&pool, Scope::Own(ed)).await.unwrap();
        assert_eq!(own.len(), 1);
        assert!(own[0].owner.is_none(), "an own list does not name the owner");
        let all = list(&pool, Scope::All).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all.iter().find(|t| t.id == theirs).unwrap().owner.as_deref(), Some("ed"));

        assert!(!update(&pool, Scope::Own(ed), mine, Some("x"), None).await.unwrap());
        assert!(update(&pool, Scope::All, theirs, Some("renamed"), Some(Some(7))).await.unwrap());
        let row = list(&pool, Scope::Own(ed)).await.unwrap().remove(0);
        assert_eq!(row.name, "renamed");
        assert!(row.expires_at.is_some());
        // A rename alone keeps the lifetime; `Some(None)` clears it.
        assert!(update(&pool, Scope::Own(ed), theirs, Some("again"), None).await.unwrap());
        assert!(list(&pool, Scope::Own(ed)).await.unwrap()[0].expires_at.is_some());
        assert!(update(&pool, Scope::Own(ed), theirs, None, Some(None)).await.unwrap());
        assert!(list(&pool, Scope::Own(ed)).await.unwrap()[0].expires_at.is_none());
        assert!(matches!(update(&pool, Scope::All, theirs, Some(" "), None).await, Err(TokenError::EmptyName)));

        assert!(revoke(&pool, Scope::All, theirs).await.unwrap());
        assert!(identity(&pool, &secret).await.is_none());
    }

    #[tokio::test]
    async fn a_token_needs_a_name_and_a_sane_lifetime() {
        let pool = pool("tok_validate").await;
        let root = create_user(&pool, "root", "longenough", Role::Admin).await.unwrap();
        assert!(matches!(create(&pool, root, "  ", None).await, Err(TokenError::EmptyName)));
        assert!(matches!(create(&pool, root, "x", Some(0)).await, Err(TokenError::BadLifetime)));
        assert!(matches!(create(&pool, root, "x", Some(5000)).await, Err(TokenError::BadLifetime)));
        assert!(identity(&pool, "not-a-token").await.is_none());
    }
}
