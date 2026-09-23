# Users, Roles and Proposals Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Accounts with three roles replace the single basic-auth credential, and an editor's content writes become bundled proposals a manager approves.

**Architecture:** `src/accounts/` owns users, sessions, the role table and the auth middleware; `src/proposals/` owns recording, the draft and review APIs and replay. Replay goes back through the application's own router (kept in a `OnceLock` on `AppState`), so no content handler changes except `POST /api/playlist` returning its id and uploads learning about pending assets.

**Tech Stack:** Rust (axum 0.8, tower 0.5 `ServiceExt::oneshot`, sqlx/SQLite, `ring` for SHA-256, `rand` 0.9), vanilla HTML/JS, stdlib Python tests.

**Spec:** `docs/superpowers/specs/2026-09-24-users-and-roles-design.md`

**How this plan is written:** the feature is large, so every task specifies its **tests in full** (they are the contract) and its **interfaces exactly**; implementation code is written during execution against the compiler, following the spec's sections named in each task. No new crates.

## Global Constraints

- Roles: `admin`, `manager`, `editor`. Minimum role per route from one table (`accounts::roles::required`).
- Open mode: **no row in `users`** → every request is an admin (`Identity.open = true`).
- Session cookie `mcc_session`: `HttpOnly; SameSite=Strict; Path=/`, plus `Secure` on the TLS listener; token 32 random bytes, hex; only its SHA-256 is stored; expiry 12 h, sliding.
- Cookie-authenticated writes need `Origin` matching the request's `Host`; else `403`.
- Login lockout: 10 failures / 5 min per address → `429`, checked-and-incremented under **one** lock acquisition (the cast lockout race lesson).
- Upgrade: stored `basic_auth_user` + hash → first admin, keys deleted, one transaction, only when `users` is empty. CLI `--basic-auth-user/--basic-auth-password` = built-in admin, never stored.
- The last enabled admin cannot be deleted, disabled or demoted; the first account must be an admin.
- Replay requests carry `ConnectInfo(SocketAddr)` (loopback) — **the extractor panics without it** — and a private `ReplayIdentity` extension the middleware trusts (extensions cannot come from a client).
- Placeholders `new:N`, substituted in path segments and in any JSON string value equal to one.
- Errors `{ "error": "<German>" }`; UI via `createElement`/`textContent`, never `innerHTML` interpolation.
- Display paths (loopback) and cast public paths stay exempt exactly as today.
- No `Co-Authored-By` / `Claude-Session` trailers. Stop any local instance before Python suites.

## File Structure

| File | Responsibility |
|---|---|
| `src/accounts/mod.rs` | `Role`, `Identity`, users + sessions storage, password + token helpers, upgrade, last-admin guard |
| `src/accounts/middleware.rs` | `auth_middleware`, Origin check, lockout, HTML redirect |
| `src/accounts/roles.rs` | `required(method, path) -> Need` and its matrix test |
| `src/accounts/api.rs` | login, logout, me, own password, users CRUD |
| `src/proposals/mod.rs` | tables, draft storage, `before` snapshots, placeholder substitution |
| `src/proposals/api.rs` | draft, submit, discard, review, approve (stale check + replay), reject |
| `src/db.rs` | new tables and the `assets.pending_changeset` column |
| `src/main.rs`, `src/models.rs` | wire middleware/routes, router `OnceLock` on `AppState`, drop `auth_cache`/`basic_auth_middleware` |
| `src/settings.rs` | remove the auth fields from the settings API and `AppSettings` |
| `src/handlers.rs` | `POST /api/playlist` returns `{id}`; uploads by an editor become pending; asset list hides others' pending assets |
| `web/login.html`, `web/users.html`, `web/approvals.html`, `web/proposals.js` | pages |
| `web/admin.html`, `web/playlist.html`, `web/assets.html`, `web/displays.html` | nav, role-aware hiding, proposal bar |
| `tests/cast/test_users.py` | new suite; `test_basicauth.py` moved to the new model |

---

### Task 1: Accounts and sessions (storage only)

**Spec:** *Accounts and signing in* (tables), *Upgrade and recovery*.

**Produces:**
- `pub enum Role { Admin, Manager, Editor }` with `as_str`, `parse(&str) -> Option<Role>`, `Ord` so `Admin > Manager > Editor`.
- `pub struct Identity { pub user_id: Option<i64>, pub name: String, pub role: Role, pub open: bool }`.
- `pub async fn any_user(pool) -> bool`
- `pub async fn create_user(pool, name, password, role) -> Result<i64, AccountError>` (`AccountError::{Exists, FirstMustBeAdmin, TooShort, Db}`; password ≥ 8 chars)
- `pub async fn verify_password(pool, name, password) -> Option<Identity>` (disabled → `None`)
- `pub async fn create_session(pool, user_id) -> String` (returns cookie value), `pub async fn session_identity(pool, token) -> Option<Identity>` (checks expiry, slides it), `pub async fn end_sessions(pool, user_id)`, `pub async fn end_session(pool, token)`
- `pub async fn update_user(pool, id, role: Option<Role>, password: Option<&str>, disabled: Option<bool>) -> Result<(), AccountError>` (`AccountError::LastAdmin`), `pub async fn delete_user(pool, id) -> Result<(), AccountError>`
- `pub async fn adopt_stored_credential(pool) -> anyhow::Result<()>` (upgrade), called from `main` after `run_migrations`.

- [ ] **Step 1: Tests** in `src/accounts/mod.rs`:

```rust
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
        end_sessions(&pool, id).await;
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
        crate::db::store_setting(&pool, "basic_auth_user", "ops").await;
        crate::db::store_setting(&pool, "basic_auth_hash", &hash).await;
        adopt_stored_credential(&pool).await.unwrap();
        let who = verify_password(&pool, "ops", "hunter2!!").await.unwrap();
        assert_eq!(who.role, Role::Admin);
        assert!(crate::db::load_setting(&pool, "basic_auth_user").await.unwrap_or_default().is_empty());
        adopt_stored_credential(&pool).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users").fetch_one(&pool).await.unwrap();
        assert_eq!(count, 1);
    }
}
```

(Check the real names of `db::load_setting` / the store helper and of the hash key constant `KEY_AUTH_HASH` in `settings.rs` first; use them.)

- [ ] **Step 2:** `cargo test accounts::` → fails (module missing).
- [ ] **Step 3:** Implement: tables in `db::run_migrations` (spec SQL); `src/accounts/mod.rs`; `mod accounts;` in `main.rs`; token = 32 bytes from `rand`, hex; `token_hash` = hex SHA-256 via `ring::digest::SHA256`; expiry `datetime('now', '+12 hours')`, slid on every successful `session_identity`; `hash_password`/`verify_hash` from `settings.rs` made `pub(crate)`.
- [ ] **Step 4:** `cargo test` green.
- [ ] **Step 5:** Commit `Add accounts and sessions`.

---

### Task 2: The role table

**Spec:** *Roles*.

**Produces:** `pub enum Need { Open, Read, Admin, Manager, Content }` and `pub fn required(method: &Method, path: &str) -> Need` in `src/accounts/roles.rs`. `Open` = no account needed (login page/route); `Read` = any role; `Content` = manager+ directly, editor → proposal.

- [ ] **Step 1: Test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    #[test]
    fn every_route_has_its_minimum_role() {
        let cases: &[(Method, &str, Need)] = &[
            (Method::GET, "/login.html", Need::Open),
            (Method::POST, "/api/login", Need::Open),
            (Method::GET, "/api/me", Need::Read),
            (Method::POST, "/api/logout", Need::Read),
            (Method::PUT, "/api/me/password", Need::Read),
            (Method::GET, "/api/playlist", Need::Read),
            (Method::GET, "/playlist.html", Need::Read),
            (Method::PUT, "/api/settings", Need::Admin),
            (Method::GET, "/api/settings", Need::Read),
            (Method::POST, "/api/webhooks", Need::Admin),
            (Method::PUT, "/api/webhooks/3", Need::Admin),
            (Method::GET, "/api/webhooks", Need::Admin),
            (Method::GET, "/api/users", Need::Admin),
            (Method::POST, "/api/users", Need::Admin),
            (Method::POST, "/api/audio", Need::Admin),
            (Method::PUT, "/api/displays/foyer", Need::Admin),
            (Method::DELETE, "/api/displays/foyer/cast/session", Need::Admin),
            (Method::POST, "/api/assets", Need::Content),
            (Method::PUT, "/api/assets/4", Need::Content),
            (Method::DELETE, "/api/assets/4", Need::Content),
            (Method::POST, "/api/playlists", Need::Content),
            (Method::PUT, "/api/playlists/2", Need::Content),
            (Method::DELETE, "/api/playlists/2", Need::Content),
            (Method::POST, "/api/playlist", Need::Content),
            (Method::PUT, "/api/playlist/9", Need::Content),
            (Method::DELETE, "/api/playlist/9", Need::Content),
            (Method::POST, "/api/playlist/9/move", Need::Content),
            (Method::PUT, "/api/displays/foyer/schedule", Need::Content),
            (Method::POST, "/api/override", Need::Manager),
            (Method::DELETE, "/api/override", Need::Manager),
            (Method::POST, "/api/displays/foyer/override", Need::Manager),
            (Method::POST, "/api/control/current", Need::Manager),
            (Method::POST, "/api/displays/foyer/control/current", Need::Manager),
            (Method::GET, "/api/changesets", Need::Manager),
            (Method::POST, "/api/changesets/3/approve", Need::Manager),
            (Method::POST, "/api/changesets/3/reject", Need::Manager),
            (Method::GET, "/api/changesets/draft", Need::Read),
            (Method::POST, "/api/changesets/draft/submit", Need::Read),
            (Method::DELETE, "/api/changesets/draft", Need::Read),
        ];
        for (method, path, need) in cases {
            assert_eq!(required(method, path), *need, "{method} {path}");
        }
    }

    #[test]
    fn an_unknown_write_needs_an_admin() {
        // A route added later without a row must fail closed, not open.
        assert_eq!(required(&Method::POST, "/api/something-new"), Need::Admin);
        assert_eq!(required(&Method::GET, "/api/something-new"), Need::Read);
    }
}
```

(Webhook targets hold other people's secrets, so even reading them is admin-only.)

- [ ] **Step 2–4:** fail → implement with `match` on method and path segments → green.
- [ ] **Step 5:** Commit `Add the role table`.

---

### Task 3: The middleware, login and `/api/me`

**Spec:** *Accounts and signing in*, *The middleware, in order*, *Roles*.

**Produces:** `accounts::middleware::auth_middleware` replacing `basic_auth_middleware` on both listeners (same `from_fn_with_state` position); `Identity` inserted as a request extension; routes `POST /api/login`, `POST /api/logout`, `GET /api/me`, `PUT /api/me/password`; `web/login.html`; `AppState.login_attempts: Arc<Mutex<HashMap<IpAddr, (u32, Instant)>>>`; `AppState.basic_cache: Arc<Mutex<HashMap<String, i64>>>` (verified header → user id) replacing `auth_cache`; `AppState.router: Arc<OnceLock<Router>>` set in `main` after the router is built (for Task 6).

- [ ] **Step 1: Python** `tests/cast/test_users.py` (new; use `Server` from `test_cast`; a `session` helper keeping a cookie jar via `http.cookiejar` + `urllib.request.build_opener`; requests carry `Origin: http://127.0.0.1:<port>` for cookie writes):

```python
"""Accounts, roles and proposals, over plain HTTP.

Open until the first account exists; then a session for a browser and HTTP
Basic for a script, each with its account's role; and an editor's content
writes kept as a bundle until a manager applies it.
"""
import base64, http.cookiejar, json, os, sys, urllib.error, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_cast import Server, check, failures, http, HTTP

BASE = f"http://127.0.0.1:{HTTP}"


class Browser:
    """A cookie jar and an Origin header -- what a signed-in page sends."""
    def __init__(self):
        self.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()),
            urllib.request.HTTPRedirectHandler())
    def call(self, method, path, body=None, origin=BASE, follow=True):
        data = json.dumps(body).encode() if body is not None else None
        headers = {"Content-Type": "application/json"} if data else {}
        if origin:
            headers["Origin"] = origin
        req = urllib.request.Request(BASE + path, data=data, method=method, headers=headers)
        try:
            with self.opener.open(req, timeout=5) as res:
                raw = res.read().decode()
                return res.status, (json.loads(raw) if raw and raw[0] in "{[" else raw), res.geturl()
        except urllib.error.HTTPError as e:
            raw = e.read().decode()
            try:
                return e.code, json.loads(raw), None
            except ValueError:
                return e.code, raw, None


def basic(method, path, user, password, body=None):
    token = base64.b64encode(f"{user}:{password}".encode()).decode()
    data = json.dumps(body).encode() if body is not None else None
    headers = {"Authorization": f"Basic {token}"}
    if data:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(BASE + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=5) as res:
            raw = res.read().decode()
            return res.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        return e.code, None


def accounts_flow():
    print("\n[120] open until the first account exists")
    with Server():
        status, me = http("GET", "/api/me")
        check("with no account the operator API is open", status == 200 and me.get("open") is True, me)
        status, body = http("POST", "/api/users", {"name": "ed", "password": "longenough", "role": "editor"})
        check("the first account must be an admin", status == 400, (status, body))
        status, _ = http("POST", "/api/users", {"name": "root", "password": "longenough", "role": "admin"})
        check("an admin account is created", status in (200, 201), status)
        status, _ = http("GET", "/api/playlist")
        check("after that, no credentials is a 401", status == 401, status)
        b = Browser()
        status, body, url = b.call("GET", "/admin.html")
        check("an HTML page redirects to the login page",
              status == 200 and url and "/login.html" in url, (status, url))

        print("\n[121] signing in, the session, signing out")
        status, body, _ = b.call("POST", "/api/login", {"name": "root", "password": "wrong-one"})
        check("a wrong password is refused", status == 401, status)
        status, body, _ = b.call("POST", "/api/login", {"name": "root", "password": "longenough"})
        check("the right one signs in", status == 200 and body.get("role") == "admin", body)
        status, me, _ = b.call("GET", "/api/me")
        check("the session carries the account", me.get("name") == "root" and me.get("open") is False, me)
        status, _, _ = b.call("PUT", "/api/settings", {"locale": "de-DE"}, origin="http://evil.test")
        check("a cookie write from another origin is refused", status == 403, status)
        status, _, _ = b.call("PUT", "/api/settings", {"locale": "de-DE"})
        check("from its own origin it goes through", status == 200, status)
        b.call("POST", "/api/logout")
        status, _, _ = b.call("GET", "/api/me")
        check("after signing out the session is gone", status == 401, status)

        print("\n[122] a script signs in with HTTP Basic")
        status, me = basic("GET", "/api/me", "root", "longenough")
        check("Basic auth works against the account", status == 200 and me["role"] == "admin", (status, me))
        status, _ = basic("GET", "/api/me", "root", "nope-nope")
        check("with a wrong password it does not", status == 401, status)

        print("\n[123] each role reaches what it may")
        for name, role in (("mgr", "manager"), ("ed", "editor")):
            status, _ = basic("POST", "/api/users", "root", "longenough",
                              {"name": name, "password": "longenough", "role": role})
            check(f"the admin creates a {role}", status in (200, 201), status)
        check("a manager may not change settings",
              basic("PUT", "/api/settings", "mgr", "longenough", {"locale": "de-DE"})[0] == 403, None)
        check("a manager may pin an override",
              basic("DELETE", "/api/override", "mgr", "longenough")[0] in (200, 204), None)
        check("an editor may not pin an override",
              basic("DELETE", "/api/override", "ed", "longenough")[0] == 403, None)
        check("an editor may read the playlist",
              basic("GET", "/api/playlist", "ed", "longenough")[0] == 200, None)
        check("an editor may not read webhook targets",
              basic("GET", "/api/webhooks", "ed", "longenough")[0] == 403, None)

        print("\n[124] disabling an account, and the last admin")
        users = basic("GET", "/api/users", "root", "longenough")[1]
        ed_id = next(u["id"] for u in users if u["name"] == "ed")
        root_id = next(u["id"] for u in users if u["name"] == "root")
        basic("PUT", f"/api/users/{ed_id}", "root", "longenough", {"disabled": True})
        check("a disabled account cannot sign in",
              basic("GET", "/api/me", "ed", "longenough")[0] == 401, None)
        check("the last admin cannot be deleted",
              basic("DELETE", f"/api/users/{root_id}", "root", "longenough")[0] == 400, None)

        print("\n[125] login attempts are limited per address")
        results = [Browser().call("POST", "/api/login", {"name": "root", "password": "x" * 9})[0]
                   for _ in range(12)]
        check("after ten failures the address gets 429", results[-1] == 429, results)

    print("\n[126] the command line credential is the way back in")
    with Server(fresh=False, basic_auth_user="rescue", basic_auth_password="rescue-me-now"):
        status, me = basic("GET", "/api/me", "rescue", "rescue-me-now")
        check("the CLI credential is an admin", status == 200 and me["role"] == "admin", (status, me))
        check("and is never stored as an account",
              all(u["name"] != "rescue" for u in basic("GET", "/api/users", "rescue", "rescue-me-now")[1]),
              None)
```

(`[125]` must run last in its server block, the lockout persists for the window.)

- [ ] **Step 2:** `cargo build && python3 tests/cast/test_users.py` → fails.
- [ ] **Step 3:** Implement middleware + login/logout/me/password routes + `login.html` (a form posting JSON to `/api/login`, then `location = next`; `next` accepted only if it starts with `/` and not `//`). Remove `basic_auth_middleware`, `auth_cache`, the settings auth fields (`auth_enabled/auth_user/auth_password` in the settings API structs with `deny_unknown_fields` on the update request, `auth_user/auth_secret` in `AppSettings`, their persistence, `Locks.basic_auth` kept for the CLI flag). CLI credential: kept in `AppState.rescue: Option<(String, String)>` from args, compared with `constant_time_eq`.
- [ ] **Step 4:** Rust tests green; `[120]`–`[126]` green (`[123]`–`[124]` need Task 4's users API — implement the users API in this task's Step 3 too, spec *Users API and page*, minus the page).
- [ ] **Step 5:** Move `tests/cast/test_basicauth.py` to the new model: its credential is created as the first admin via `POST /api/users` in open mode instead of `PUT /api/settings` auth fields; keep its intent (credentials guard the operator surface, loopback display paths stay open, the CLI credential gets back in). Run all HTTP suites (`test_cast`, `test_auth`, `test_basicauth`, `test_settings`, `test_conflict`, `test_reserve`, `test_audio`, `test_limits`, `test_guestpage`, `test_pairing`) → green.
- [ ] **Step 6:** Commit `Sign in with accounts and check roles per route`.

---

### Task 4: The users page and role-aware pages

**Files:** `web/users.html` (new), `web/admin.html` (auth block → link to users page; banner in open mode; hide admin-only blocks for other roles), nav links on `playlist.html`, `assets.html`, `displays.html`, `webhooks.html` (a "Benutzer" link for admins, "Abmelden" for signed-in accounts), a small shared `web/me.js` (`await Me.load()` → `{name, role, open}`, `Me.can(need)`).

- [ ] Build; headless check like earlier UI checks: `users.html` lists, adds, disables; an editor sees no settings/webhooks controls in `admin.html`; open mode shows the banner.
- [ ] Commit `Manage accounts on a page of their own`.

---

### Task 5: Recording proposals

**Spec:** *Proposals* (tables, recording, placeholders, uploads, the editor's draft).

**Produces:** tables `changesets`, `change_requests`, column `assets.pending_changeset`; `proposals::record(state, identity, method, path, body) -> Result<Recorded, ProposalError>` called by the middleware when `required(..) == Need::Content && identity.role == Editor`; `proposals::snapshot(state, method, path) -> Option<Value>` (internal GETs through `state.router`); `proposals::substitute(value: &mut Value, ids: &HashMap<String, i64>)` and `substitute_path`; draft routes `GET /api/changesets/draft`, `DELETE /api/changesets/draft`, `DELETE /api/changesets/draft/requests/{id}`, `POST /api/changesets/draft/submit`; `POST /api/playlist` answers `201 {"id": …}`; an editor's upload stores the asset with `pending_changeset` and records an `UPLOAD` request; `GET /api/assets` hides pending assets except the author's.

- [ ] **Step 1: Rust tests** for `substitute` (path segment `new:2`, nested JSON string `"new:2"`, a non-placeholder string untouched, a number untouched) and for the write-path → snapshot-kind mapping (item, playlist, asset, schedule, move).
- [ ] **Step 2: Python** (append to `test_users.py`, same server block before `[125]`):

```python
        print("\n[127] an editor's writes wait in a draft")
        basic("PUT", f"/api/users/{ed_id}", "root", "longenough", {"disabled": False})
        pl = basic("POST", "/api/playlists", "mgr", "longenough", {"name": "Foyer"})[1]["id"]
        basic("POST", "/api/playlist", "mgr", "longenough",
              {"url": "https://a.test/", "duration": 10, "playlist_id": pl})
        item = basic("GET", f"/api/playlist?playlist_id={pl}", "mgr", "longenough")[1][0]["id"]
        status, body = basic("PUT", f"/api/playlist/{item}", "ed", "longenough", {"duration": 42})
        check("an editor's edit is accepted as a proposal", status == 202 and body.get("proposed"), (status, body))
        live = basic("GET", f"/api/playlist?playlist_id={pl}", "mgr", "longenough")[1][0]
        check("and is not live", live["duration"] == 10, live)
        status, body = basic("POST", "/api/playlists", "ed", "longenough", {"name": "Neu"})
        new_pl = body.get("placeholder")
        check("a create gets a placeholder", status == 202 and new_pl and new_pl.startswith("new:"), body)
        status, body = basic("POST", "/api/playlist", "ed", "longenough",
                             {"url": "https://b.test/", "duration": 5, "playlist_id": new_pl})
        check("which a later request may use", status == 202, (status, body))
        draft = basic("GET", "/api/changesets/draft", "ed", "longenough")[1]
        check("the draft holds all three, in order", len(draft["requests"]) == 3, draft)
        check("nobody else sees a playlist named Neu yet",
              all(p["name"] != "Neu" for p in basic("GET", "/api/playlists", "mgr", "longenough")[1]), None)
        status, _ = basic("POST", "/api/changesets/draft/submit", "ed", "longenough")
        check("the draft is submitted", status == 200, status)
```

- [ ] **Step 3–4:** implement → green (the proposal checks; approve comes in Task 6).
- [ ] **Step 5:** Commit `Record an editor's content writes as a draft`.

---

### Task 6: Review, apply, reject

**Spec:** *Applying*, *Review*.

**Produces:** `GET /api/changesets?state=`, `POST /api/changesets/{id}/approve`, `POST /api/changesets/{id}/reject`; stale check; replay via `state.router.get().unwrap().clone().oneshot(req)` with `ConnectInfo` + `ReplayIdentity(manager)` extensions; states `applied` / `stale` / `failed`; pending assets cleared on apply, deleted with their file on reject/discard.

- [ ] **Step 1: Python** (append, before `[125]`):

```python
        print("\n[128] a manager applies the bundle, and all of it is live at once")
        bundle = next(c for c in basic("GET", "/api/changesets?state=submitted", "mgr", "longenough")[1])
        check("the reviewer sees before and after", all("before" in r for r in bundle["requests"]), bundle)
        status, body = basic("POST", f"/api/changesets/{bundle['id']}/approve", "mgr", "longenough")
        check("it applies", status == 200 and body.get("state") == "applied", (status, body))
        live = basic("GET", f"/api/playlist?playlist_id={pl}", "mgr", "longenough")[1][0]
        check("the edit is live", live["duration"] == 42, live)
        made = next((p for p in basic("GET", "/api/playlists", "mgr", "longenough")[1] if p["name"] == "Neu"), None)
        check("the new playlist exists", made is not None, None)
        items = basic("GET", f"/api/playlist?playlist_id={made['id']}", "mgr", "longenough")[1]
        check("and holds the item that named it by placeholder",
              len(items) == 1 and items[0]["url"] == "https://b.test/", items)

        print("\n[129] a stale bundle is not applied")
        basic("PUT", f"/api/playlist/{item}", "ed", "longenough", {"duration": 7})
        basic("POST", "/api/changesets/draft/submit", "ed", "longenough")
        basic("PUT", f"/api/playlist/{item}", "mgr", "longenough", {"duration": 99})
        stale = next(c for c in basic("GET", "/api/changesets?state=submitted", "mgr", "longenough")[1])
        status, body = basic("POST", f"/api/changesets/{stale['id']}/approve", "mgr", "longenough")
        check("approving a bundle whose object changed is a 409", status == 409, (status, body))
        live = basic("GET", f"/api/playlist?playlist_id={pl}", "mgr", "longenough")[1][0]
        check("and the manager's change stands", live["duration"] == 99, live)
        status, _ = basic("POST", f"/api/changesets/{stale['id']}/reject", "mgr", "longenough", {"note": "veraltet"})
        check("it can still be rejected", status == 200, status)
```

plus, for uploads (multipart via `test_media.upload_parts` with a Basic header — add an optional `auth` parameter there): an editor's upload is hidden from `mgr`'s `GET /api/assets`, listed for `ed`; after a reject its file under `--assets-dir` is gone.

- [ ] **Step 2–4:** implement → green.
- [ ] **Step 5:** Commit `Apply or reject a bundle of proposals`.

---

### Task 7: Proposal UI

**Files:** `web/proposals.js` (editor bar on `playlist.html`, `assets.html`, `displays.html`: count, Einreichen, Verwerfen, list with per-request remove; "vorgeschlagen" badge on cards/rows touched by the draft; an editor's pickers include their proposed playlists/uploads with the placeholder as value), `web/approvals.html` (submitted bundles with readable lines and before/after, Freigeben / Ablehnen with note; stale shown with reason, only Ablehnen), nav link "Freigaben (N)" for managers.

- [ ] Build; headless check: an editor edits a card → badge + bar count; submits; a manager sees it on `approvals.html`, approves, card shows the new value.
- [ ] Commit `Show drafts to editors and bundles to managers`.

---

### Task 8: Docs and full run

- [ ] `CLAUDE.md`: *HTTP: three audiences* gains the account layer (identity order, open mode, `Origin` check, replay identity + `ConnectInfo` trap, role table fails closed); *Settings* loses the credential paragraph in favour of accounts; the CLI-credential recovery rule restated.
- [ ] `README.md`: accounts, login, users, changesets APIs; `POST /api/playlist` returns `{id}`; settings auth fields removed.
- [ ] `docs/features.md`, `docs/deployment.md` (recovery via CLI; upgrade turns the stored credential into the first admin), `docs/roadmap.md` (remove *Users and roles*), `tests/cast/README.md` (`test_users.py`).
- [ ] Spec → implemented.
- [ ] Run every suite (HTTP ones and the browser ones, one at a time) → green.
- [ ] Commit `Document accounts, roles and proposals`.
