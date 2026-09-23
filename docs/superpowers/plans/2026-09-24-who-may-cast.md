# Who May Cast Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Each screen says, per mode (cast, page), whether anyone, only signed-in accounts, or nobody may use it; the session remembers who cast.

**Architecture:** A new `src/cast/access.rs` holds `Access`, the pure decision and the per-screen load. The auth middleware *recognises* a session cookie on cast-public paths without requiring one. `authorize_sender` (every claim) and the socket's `present` frame apply the decision; the reservation carries the account onto the session, where webhooks, logs and the operator list read it.

**Tech Stack:** Rust (axum 0.8, sqlx/SQLite), vanilla JS, stdlib Python tests.

Spec: `docs/superpowers/specs/2026-09-24-who-may-cast-design.md`.

## Global Constraints

- `Access` JSON values: `"anyone"`, `"account"`, `"off"`; columns `displays.cast_access`, `displays.page_access` `TEXT NOT NULL DEFAULT 'anyone'`.
- Venue switches (`cast_enabled`, `guest_pages_enabled`, `--disable-cast`, `--guest-pages`) stay and are folded in: effective off if the venue switch is off.
- `off` → `403` with the venue-off wording (`"Übertragung ist derzeit deaktiviert."` / `"Webseiten sind derzeit nicht erlaubt."`).
- `account` without an account → `401` `{"error": "Zum Casten bitte anmelden.", "login": "/login.html?next=/"}`.
- The presence check (`cast_auth`) runs after, unchanged; an account never replaces it.
- Cast-public paths never answer `401` from the middleware. Identity attached only from a valid session cookie, and for non-`GET`/`HEAD` only with a same-host `Origin`. HTTP Basic is ignored there.
- Lock order stays settings → cast_attempts → cast → override_item; DB reads happen outside those locks.
- Tests about a screen use the non-primary one (`werkstatt` in `test_display.Alone`).
- `web/` is compiled in; a new file under `web/` needs `touch src/web.rs`.
- Commits: no Claude co-author or session trailers.

---

### Task 1: `Access`, its storage and the operator API

**Files:**
- Create: `src/cast/access.rs`
- Modify: `src/cast/mod.rs` (`mod access;` + `pub use`), `src/db.rs` (columns), `src/display.rs` (`UpdateDisplay`, `update`, `list`)
- Test: `tests/cast/test_castaccess.py` (create)

**Interfaces:**
- Produces:
  - `pub enum Access { Anyone, Account, Off }` (`Serialize, Deserialize`, snake_case, `Default = Anyone`)
  - `pub enum Refusal { Off, NeedsAccount }`
  - `pub fn decide(venue_on: bool, access: Access, signed_in: bool) -> Result<(), Refusal>`
  - `pub async fn load(pool: &SqlitePool, display: &str, mode: ClaimMode) -> Access`
  - `GET /api/displays` entries gain `cast_access`, `page_access`; `PUT /api/displays/{name}` accepts them.

- [ ] **Step 1: The module with unit tests**

`src/cast/access.rs`:

```rust
//! Who may use a screen, per mode: anyone, signed-in accounts only, or nobody.
//!
//! Below the venue's switches, never instead of them: `cast_enabled` and
//! `guest_pages_enabled` still stop a mode everywhere, and a screen can only
//! narrow what they allow. The presence check (`cast_auth`) runs after this,
//! unchanged -- an account says who, not that they are in the room.

use serde::{Deserialize, Serialize};

use super::ClaimMode;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    #[default]
    Anyone,
    Account,
    Off,
}

impl Access {
    fn from_column(raw: &str) -> Self {
        match raw {
            "account" => Access::Account,
            "off" => Access::Off,
            _ => Access::Anyone,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Access::Anyone => "anyone",
            Access::Account => "account",
            Access::Off => "off",
        }
    }

    /// This screen's access with the venue switch folded in: what a guest can
    /// actually do, which is what the guest page is told.
    pub fn effective(self, venue_on: bool) -> Self {
        if venue_on { self } else { Access::Off }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    Off,
    NeedsAccount,
}

pub fn decide(venue_on: bool, access: Access, signed_in: bool) -> Result<(), Refusal> {
    match access.effective(venue_on) {
        Access::Off => Err(Refusal::Off),
        Access::Account if !signed_in => Err(Refusal::NeedsAccount),
        _ => Ok(()),
    }
}

/// The screen's stored access for `mode`. A row that cannot be read is
/// `Anyone` -- the behaviour before this existed -- and logged, rather than
/// locking every guest out over a database hiccup.
pub async fn load(pool: &sqlx::SqlitePool, display: &str, mode: ClaimMode) -> Access {
    let column = match mode {
        ClaimMode::Cast => "cast_access",
        ClaimMode::Page => "page_access",
    };
    let sql = format!("SELECT COALESCE({column}, 'anyone') FROM displays WHERE name = ?");
    match sqlx::query_scalar::<_, String>(&sql).bind(display).fetch_optional(pool).await {
        Ok(raw) => raw.as_deref().map(Access::from_column).unwrap_or_default(),
        Err(e) => {
            tracing::error!("Failed to read {} of display {}: {}", column, display, e);
            Access::Anyone
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape() {
        assert_eq!(serde_json::to_value(Access::Account).unwrap(), serde_json::json!("account"));
        let parsed: Access = serde_json::from_value(serde_json::json!("off")).unwrap();
        assert_eq!(parsed, Access::Off);
    }

    #[test]
    fn the_decision() {
        assert_eq!(decide(true, Access::Anyone, false), Ok(()));
        assert_eq!(decide(true, Access::Account, true), Ok(()));
        assert_eq!(decide(true, Access::Account, false), Err(Refusal::NeedsAccount));
        assert_eq!(decide(true, Access::Off, true), Err(Refusal::Off));
        // The venue switch wins over everything the screen says.
        assert_eq!(decide(false, Access::Anyone, true), Err(Refusal::Off));
        assert_eq!(decide(false, Access::Account, true), Err(Refusal::Off));
    }
}
```

In `src/cast/mod.rs`, beside the other submodule declarations: `pub mod access;`.

- [ ] **Step 2: Columns**

In `src/db.rs`, after the `displays` `CREATE TABLE` and its existing probes, add (same probe style as the neighbours):

```rust
    // Who may use each screen, per mode. `'anyone'` is what every screen did
    // before this existed, so an upgrade changes nothing.
    for column in ["cast_access", "page_access"] {
        let has: bool = sqlx::query("SELECT count(*) FROM pragma_table_info('displays') WHERE name = ?")
            .bind(column)
            .fetch_one(pool)
            .await
            .map(|row| row.get::<i32, _>(0) > 0)
            .unwrap_or(false);
        if !has {
            sqlx::query(&format!("ALTER TABLE displays ADD COLUMN {column} TEXT NOT NULL DEFAULT 'anyone'"))
                .execute(pool)
                .await?;
        }
    }
```

- [ ] **Step 3: Failing Python test for the operator API**

`tests/cast/test_castaccess.py`:

```python
"""Who may cast: per screen and mode -- anyone, accounts only, or nobody.

Every case is about `werkstatt`, the second declared screen: a case on the
first would pass identically if the screen were never resolved (CLAUDE.md).
"""
import asyncio, base64, json, os, sys, urllib.error, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wsclient
from test_cast import check, failures, http, HTTP
from test_display import Alone
from test_users import Browser, BASE

SCREEN = "werkstatt"


def access_of(name):
    return next(d for d in http("GET", "/api/displays")[1] if d["name"] == name)


def api_flow():
    print("\n[150] every screen starts open to anyone")
    with Alone():
        check("cast is open to anyone", access_of(SCREEN)["cast_access"] == "anyone", access_of(SCREEN))
        check("and so are pages", access_of(SCREEN)["page_access"] == "anyone", access_of(SCREEN))
        status, _ = http("PUT", f"/api/displays/{SCREEN}", {"cast_access": "account", "page_access": "off"})
        check("the operator narrows one screen", status == 200, status)
        check("it is stored", access_of(SCREEN)["cast_access"] == "account"
              and access_of(SCREEN)["page_access"] == "off", access_of(SCREEN))
        check("the other screen is untouched", access_of("foyer")["cast_access"] == "anyone", access_of("foyer"))
        status, _ = http("PUT", f"/api/displays/{SCREEN}", {"cast_access": "members"})
        check("an unknown value is refused", status == 422, status)


if __name__ == "__main__":
    api_flow()
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
```

Run: `cargo build && python3 tests/cast/test_castaccess.py` — Expected: FAIL (`KeyError: 'cast_access'`).

- [ ] **Step 4: The display API**

In `src/display.rs`, `UpdateDisplay` gains:

```rust
    /// Who may cast to this screen. See `cast::access`.
    #[serde(default)]
    cast_access: Option<crate::cast::access::Access>,
    /// Who may put a web page on it.
    #[serde(default)]
    page_access: Option<crate::cast::access::Access>,
```

In `update`, after the label block:

```rust
    for (column, value) in [("cast_access", payload.cast_access), ("page_access", payload.page_access)] {
        let Some(value) = value else { continue };
        let sql = format!("UPDATE displays SET {column} = ? WHERE name = ?");
        if let Err(e) = sqlx::query(&sql).bind(value.as_str()).bind(&name).execute(&state.pool).await {
            tracing::error!("Failed to set {} for display {}: {}", column, name, e);
        }
    }
```

In `list`, read the two columns: change the query to
`"SELECT name, label, COALESCE(cast_access, 'anyone'), COALESCE(page_access, 'anyone') FROM displays ORDER BY name ASC"`
with row type `(String, Option<String>, String, String)`; carry the two strings through `entries` (a declared display without a row gets `"anyone"`), and add to each JSON object:

```rust
            "cast_access": cast_access,
            "page_access": page_access,
```

Adjust the two `rows.iter()` lookups to the 4-tuple.

- [ ] **Step 5: Run**

Run: `cargo test access && cargo build && python3 tests/cast/test_castaccess.py`
Expected: 2 Rust tests pass; ALL PASSED.

- [ ] **Step 6: Commit**

```bash
git add src/cast/access.rs src/cast/mod.rs src/db.rs src/display.rs tests/cast/test_castaccess.py
git commit -m "Store who may cast to each screen, per mode"
```

---

### Task 2: Recognise the account, decide at the claim and on the socket

**Files:**
- Modify: `src/accounts/middleware.rs`, `src/cast/api.rs` (`authorize_sender`, `claim_session`, `cast_info` signature only), `src/cast/mod.rs` (`Reservation.user`, `CastSession.user`), `src/cast/signaling.rs` (`consume_reservation`, `present`)
- Test: `tests/cast/test_castaccess.py`

**Interfaces:**
- Consumes: `access::{decide, load, Refusal}`
- Produces: `pub(super) enum SenderRefusal { Forbidden(String), NeedsAccount }`; `authorize_sender(state, display, addr, provided, mode, who: Option<&Identity>) -> Result<(), SenderRefusal>`; `CastSession::user: Option<String>`

- [ ] **Step 1: Failing tests — the matrix and the traps**

Append to `tests/cast/test_castaccess.py` before `if __name__`:

```python
def claim_as(browser, mode, origin=BASE):
    status, body, _ = browser.call("POST", "/api/cast/claim", {"display": SCREEN, "mode": mode}, origin=origin)
    return status, body


def release(browser):
    browser.call("DELETE", "/api/cast/claim", {"display": SCREEN})


def basic_claim(mode):
    token = base64.b64encode(b"member:longenough").decode()
    req = urllib.request.Request(f"{BASE}/api/cast/claim", method="POST",
                                 data=json.dumps({"display": SCREEN, "mode": mode}).encode(),
                                 headers={"Content-Type": "application/json", "Authorization": f"Basic {token}"})
    try:
        with urllib.request.urlopen(req, timeout=5) as res:
            return res.status, json.loads(res.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def with_accounts():
    """An admin (first account) and a member, and a signed-in browser for the member."""
    http("POST", "/api/users", {"name": "root", "password": "longenough", "role": "admin"})
    token = base64.b64encode(b"root:longenough").decode()
    req = urllib.request.Request(f"{BASE}/api/users", method="POST",
                                 data=json.dumps({"name": "member", "password": "longenough", "role": "editor"}).encode(),
                                 headers={"Content-Type": "application/json", "Authorization": f"Basic {token}"})
    urllib.request.urlopen(req, timeout=5).read()
    member = Browser()
    member.call("POST", "/api/login", {"name": "member", "password": "longenough"})
    return member, token


def put_access(token, **values):
    req = urllib.request.Request(f"{BASE}/api/displays/{SCREEN}", method="PUT", data=json.dumps(values).encode(),
                                 headers={"Content-Type": "application/json", "Authorization": f"Basic {token}"})
    urllib.request.urlopen(req, timeout=5).read()


def claim_flow():
    print("\n[151] the matrix: mode x access x signed in")
    with Alone(guest_pages="on"):
        member, admin = with_accounts()
        guest = Browser()
        for mode in ("cast", "page"):
            key = f"{mode}_access"
            put_access(admin, **{key: "anyone"})
            status, _ = claim_as(guest, mode)
            check(f"{mode}, anyone: a guest may", status == 200, status)
            release(guest)
            put_access(admin, **{key: "account"})
            status, body = claim_as(guest, mode)
            check(f"{mode}, account: a guest is asked to sign in",
                  status == 401 and body.get("login") == "/login.html?next=/", (status, body))
            status, _ = claim_as(member, mode)
            check(f"{mode}, account: a member may", status == 200, status)
            release(member)
            put_access(admin, **{key: "off"})
            status, body = claim_as(member, mode)
            check(f"{mode}, off: not even a member", status == 403 and "error" in body, (status, body))
            put_access(admin, **{key: "anyone"})

        print("\n[152] a cookie from another site, or a Basic header, is a guest")
        put_access(admin, cast_access="account")
        status, _ = claim_as(member, "cast", origin="http://evil.test")
        check("the member's cookie with a foreign Origin counts for nothing", status == 401, status)
        status, _ = basic_claim("cast")
        check("HTTP Basic on a claim counts for nothing either", status == 401, status)
        put_access(admin, cast_access="anyone")

    print("\n[153] an account still needs the code")
    with Alone(cast_auth="code", cast_code="ABCD"):
        member, admin = with_accounts()
        put_access(admin, cast_access="account")
        status, body = claim_as(member, "cast")
        check("signed in, but no code: refused by the code check", status == 403, (status, body))
        status, body, _ = member.call("POST", "/api/cast/claim",
                                      {"display": SCREEN, "mode": "cast", "code": "ABCD"})
        check("signed in with the code: allowed", status == 200, (status, body))

    print("\n[154] the socket applies the page access to a reused cast ticket")
    with Alone(guest_pages="on"):
        member, admin = with_accounts()
        put_access(admin, page_access="account")
        guest = Browser()
        status, body = claim_as(guest, "cast")  # what the code check does
        check("the guest holds a cast ticket", status == 200, (status, body))

        async def present():
            r, w = await wsclient.connect("127.0.0.1", HTTP,
                                          f"/api/cast/ws?role=sender&ticket={body['ticket']}", secure=False)
            await wsclient.recv_json(r)  # welcome
            await wsclient.send_json(w, {"type": "present", "url": "https://example.org/"})
            frame = await asyncio.wait_for(wsclient.recv_json(r), 5)
            w.close()
            return frame
        frame = asyncio.run(present())
        check("a page on that ticket is refused on the socket",
              frame and frame.get("type") == "error" and frame.get("code") == "page", frame)
```

and in `__main__`, call `claim_flow()` after `api_flow()`.

(Check `Server` flag names against `models::Args`: `--guest-pages on`, `--cast-auth code`, `--cast-code ABCD`; adjust the kwargs if they differ. The harness passes `--guest-pages off` by default, which is why `[151]` and `[154]` turn it on.)

Run: `cargo build && python3 tests/cast/test_castaccess.py` — Expected: FAIL at `[151]` (a guest is not asked to sign in).

- [ ] **Step 2: Recognise the account on cast paths**

In `src/accounts/middleware.rs`, replace the cast branch:

```rust
    // The guest is by definition not loopback; exempt regardless of address.
    // Exempt, but not blind: a signed-in member's session is recognised so a
    // screen can be kept for accounts (`cast::access`). Never required -- a
    // request without one proceeds exactly as a guest's -- and only from the
    // cookie: a cached Basic credential must not turn a guest into an account.
    // A write needs this host's `Origin`, or another site could claim a screen
    // with a member's cookie; a read only tells the page who is signed in.
    if !state.args.disable_cast && crate::cast::is_cast_public_path(&path) {
        if let Some(token) = session_token(request.headers()) {
            let writing = method != Method::GET && method != Method::HEAD;
            if !writing || same_origin(request.headers(), request.uri()) {
                if let Some(who) = super::session_identity(&state.pool, &token).await {
                    request.extensions_mut().insert(who);
                }
            }
        }
        return next.run(request).await;
    }
```

- [ ] **Step 3: The reservation and the session carry the account**

In `src/cast/mod.rs`: `struct Reservation` gains

```rust
    /// The account that claimed it, or `None` for a guest. Carried onto the
    /// session, where the socket's page check and "who cast" read it.
    user: Option<String>,
```

`CastSession` gains

```rust
    /// The account behind the current session, or `None` for a guest.
    user: Option<String>,
```

and where `deactivate_display` clears `session.sender_addr = None;` add `session.user = None;`. `end_session` already clears the reservation.

In `src/cast/signaling.rs::consume_reservation`, beside `session.pending_mode = mode;`: `session.user = held.user.clone();` (take it before `session.reservation = None;`).

- [ ] **Step 4: Decide in `authorize_sender`**

In `src/cast/api.rs`:

```rust
/// Why a sender is turned away. A missing account is its own case because the
/// guest page answers it with a sign-in link, not just a message.
pub(super) enum SenderRefusal {
    Forbidden(String),
    NeedsAccount,
}
```

`authorize_sender` gains the parameter `who: Option<&crate::accounts::Identity>` and returns `Result<(), SenderRefusal>`. Before `let mut attempts = state.cast_attempts.lock().await;`:

```rust
    // Read before any lock is taken: a database read has no place inside the
    // settings -> cast_attempts -> cast order.
    let access = super::access::load(&state.pool, &display.name, mode).await;
```

Replace the `if !enabled { return Err(match mode { … }) }` block with:

```rust
    match super::access::decide(enabled, access, who.is_some()) {
        Ok(()) => {}
        // The venue's wording whether the venue or this screen said no: the
        // answer must not tell a guest more than "not here".
        Err(super::access::Refusal::Off) => {
            return Err(SenderRefusal::Forbidden(match mode {
                ClaimMode::Cast => "Übertragung ist derzeit deaktiviert.".to_string(),
                ClaimMode::Page => "Webseiten sind derzeit nicht erlaubt.".to_string(),
            }));
        }
        Err(super::access::Refusal::NeedsAccount) => return Err(SenderRefusal::NeedsAccount),
    }
```

Every other `return Err(string)` / `Err(message)` in the function becomes `Err(SenderRefusal::Forbidden(…))` — the lockout message and the final `Err(message)` of the code check. Make the `outcome` match stay `Result<(), String>` internally and map at the end.

`claim_session` gains `identity: Option<axum::Extension<crate::accounts::Identity>>` (after `ConnectInfo`), and the call becomes:

```rust
    let who = identity.as_ref().map(|axum::Extension(who)| who);
    match authorize_sender(&state, &display, addr, payload.code.as_deref(), payload.mode, who).await {
        Ok(()) => {}
        Err(SenderRefusal::Forbidden(message)) => {
            return (StatusCode::FORBIDDEN, Json(json!({"error": message}))).into_response();
        }
        Err(SenderRefusal::NeedsAccount) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Zum Casten bitte anmelden.", "login": "/login.html?next=/"})),
            )
                .into_response();
        }
    }
```

The reservation construction adds `user: who.map(|w| w.name.clone()),` and the log line becomes
`info!("Cast: session reserved by {}{}", addr, who.map(|w| format!(" ({})", w.name)).unwrap_or_default());`.

Search for other callers: `grep -n "authorize_sender(" src/cast/*.rs` — adapt each (pass `None` where no identity exists).

- [ ] **Step 5: The socket's page check**

In `src/cast/signaling.rs`, the `Some("present") if role == Role::Sender` branch, right after the `if !allowed { … }` block:

```rust
            // The venue allows pages; this screen may not, or only for an
            // account. Checked here as well as at the claim, because a guest's
            // code check claims in cast mode and the page reuses that ticket.
            let access = super::access::load(&state.pool, &display.name, ClaimMode::Page).await;
            let signed_in = display.cast.lock().await.user.is_some();
            match super::access::decide(true, access, signed_in) {
                Ok(()) => {}
                Err(super::access::Refusal::Off) => {
                    refuse("Webseiten sind derzeit nicht erlaubt.").await;
                    return true;
                }
                Err(super::access::Refusal::NeedsAccount) => {
                    refuse("Zum Anzeigen einer Webseite bitte anmelden.").await;
                    return true;
                }
            }
```

(`ClaimMode` import in signaling.rs if not present.)

- [ ] **Step 6: Run**

Run: `cargo build && python3 tests/cast/test_castaccess.py && python3 tests/cast/test_cast.py && python3 tests/cast/test_castscreens.py && python3 tests/cast/test_guestpage.py && python3 tests/cast/test_pairing.py`
Expected: all pass (not concurrently).

- [ ] **Step 7: Commit**

```bash
git add src tests/cast/test_castaccess.py
git commit -m "Keep a screen for accounts, or off, at the claim and on the socket"
```

---

### Task 3: Switching a screen off ends its session; who cast

**Files:**
- Modify: `src/cast/mod.rs` (`running_mode`, `GuestPageShown` emit), `src/display.rs` (`update`, `list`), `src/cast/signaling.rs` (`CastStarted` emit), `src/webhook/mod.rs`, `src/webhook/api.rs`
- Test: `tests/cast/test_castaccess.py`

**Interfaces:**
- Produces: `pub async fn running_mode(display: &Display) -> Option<ClaimMode>`, `pub async fn session_user(display: &Display) -> Option<String>` in `cast/mod.rs`; `Event::CastStarted { sender_ip, mode, user: Option<String> }`, `Event::GuestPageShown { url, sender_ip, user: Option<String> }`; `GET /api/displays` entries gain `cast_user`.

- [ ] **Step 1: Failing tests**

Append to `tests/cast/test_castaccess.py` and call from `__main__`:

```python
def end_flow():
    print("\n[155] switching a screen off ends its session of that mode, and only there")
    with Alone():
        async def run():
            fr, fw = await wsclient.connect("127.0.0.1", HTTP, "/api/cast/ws?role=sender&ticket="
                                            + http("POST", "/api/cast/claim", {"display": "foyer"})[1]["ticket"],
                                            secure=False)
            wr, ww = await wsclient.connect("127.0.0.1", HTTP, "/api/cast/ws?role=sender&ticket="
                                            + http("POST", "/api/cast/claim", {"display": SCREEN})[1]["ticket"],
                                            secure=False)
            await wsclient.recv_json(fr)
            await wsclient.recv_json(wr)
            http("PUT", f"/api/displays/{SCREEN}", {"page_access": "off"})
            await asyncio.sleep(0.5)
            check("switching pages off leaves a cast running",
                  http("GET", f"/api/cast/state?screen={SCREEN}")[1].get("active") is not False, None)
            http("PUT", f"/api/displays/{SCREEN}", {"cast_access": "off"})
            closed = None
            try:
                while True:
                    frame = await asyncio.wait_for(wsclient.recv_json(wr), 5)
                    if frame is None:
                        closed = True
                        break
            except asyncio.TimeoutError:
                closed = False
            check("switching casting off closes that screen's sender", closed is True, closed)
            check("and the other screen's cast is untouched",
                  http("GET", "/api/cast/state?screen=foyer")[1].get("sender") == "127.0.0.1",
                  http("GET", "/api/cast/state?screen=foyer")[1])
            fw.close()
        asyncio.run(run())


def who_flow():
    print("\n[156] the session knows who cast")
    with Alone():
        member, admin = with_accounts()
        status, body = claim_as(member, "cast")
        async def run():
            r, w = await wsclient.connect("127.0.0.1", HTTP, f"/api/cast/ws?role=sender&ticket={body['ticket']}",
                                          secure=False)
            await wsclient.recv_json(r)
            await asyncio.sleep(0.3)
            token = admin
            req = urllib.request.Request(f"{BASE}/api/displays", headers={"Authorization": f"Basic {token}"})
            rows = json.loads(urllib.request.urlopen(req, timeout=5).read())
            row = next(d for d in rows if d["name"] == SCREEN)
            check("the operator list names the member", row.get("cast_user") == "member", row)
            w.close()
        asyncio.run(run())
```

(`/api/cast/state` is loopback-only and the test runs on loopback. If its JSON names the sender under a different key, read it once and use that key.)

Run: expected FAIL at `[155]`.

- [ ] **Step 2: `running_mode` and `session_user`**

In `src/cast/mod.rs`:

```rust
/// What the screen's session is doing, if anything: its reservation's mode
/// while it waits, what it shows once it runs. What `PUT /api/displays` needs to
/// end exactly the session a switched-off mode is running.
pub async fn running_mode(display: &Display) -> Option<ClaimMode> {
    let session = display.cast.lock().await;
    match &session.showing {
        Showing::Cast => return Some(ClaimMode::Cast),
        Showing::Page { .. } => return Some(ClaimMode::Page),
        Showing::Nothing => {}
    }
    if session.sender.is_some() {
        return Some(session.pending_mode);
    }
    session.live_reservation().map(|held| held.mode)
}

/// The account behind the screen's session, for the operator list.
pub async fn session_user(display: &Display) -> Option<String> {
    let session = display.cast.lock().await;
    if session.is_active() { session.user.clone() } else { None }
}
```

- [ ] **Step 3: End on switch-off, and list who cast**

In `src/display.rs::update`, after the access writes:

```rust
    // Switching a mode off on this screen ends a session of that mode here, the
    // way the venue switch ends every session; `account` does not -- the guest
    // was allowed when they started.
    if let Some(display) = state.display(&name) {
        let running = crate::cast::running_mode(&display).await;
        let off = |value: Option<crate::cast::access::Access>| value == Some(crate::cast::access::Access::Off);
        let ends = match running {
            Some(crate::cast::ClaimMode::Cast) => off(payload.cast_access),
            Some(crate::cast::ClaimMode::Page) => off(payload.page_access),
            None => false,
        };
        if ends {
            tracing::info!("Cast: {} switched off on {}, ending its session", if matches!(running, Some(crate::cast::ClaimMode::Cast)) { "casting" } else { "pages" }, name);
            crate::cast::end_session(&state, &display, "disabled").await;
        }
    }
```

(`state.display(name)` returns `Option<Arc<Display>>`; `ClaimMode` must be `pub` from `cast` — it is `pub enum` in `cast/mod.rs`; re-export if needed.)

In `list`, for each declared entry add `"cast_user": crate::cast::session_user(&display).await` (look the `Arc<Display>` up with `state.display(&name)`; `null` for undeclared rows).

- [ ] **Step 4: Webhooks name the account**

`src/webhook/mod.rs`: `CastStarted { sender_ip: String, mode: String, user: Option<String> }`, `GuestPageShown { url: String, sender_ip: String, user: Option<String> }`; their `data()` arms add `"user": user`. `src/webhook/api.rs::catalogue()`: add `"user"` to both field lists; the sample events in `sample()` get `user: None` (and one `Some("mitglied".into())` for `cast.started` so the admin page's test send shows the field filled).

Emit sites: `src/cast/signaling.rs` — in the block that computes `(mode, announce)` under the session lock, also take `let user = session.user.clone();` and pass `user` into `CastStarted`. `src/cast/mod.rs::activate_display` — read `session.user.clone()` inside the lock scope that already exists there and pass it into `GuestPageShown`.

Run `grep -n "CastStarted {\|GuestPageShown {" src -r` and fix every construction (tests in `webhook/mod.rs` included).

- [ ] **Step 5: Run**

Run: `cargo test && cargo build && python3 tests/cast/test_castaccess.py && python3 tests/cast/test_webhook.py`
Expected: pass. Add a check to `test_webhook.py`'s `cast.started` case that `data["user"] is None` for a guest.

- [ ] **Step 6: Commit**

```bash
git add src tests/cast
git commit -m "End a switched-off screen's session and name who cast"
```

---

### Task 4: The guest page

**Files:**
- Modify: `src/cast/api.rs` (`cast_info`), `web/index.html`
- Test: `tests/cast/test_castaccess.py`

- [ ] **Step 1: Failing test**

```python
def info_flow():
    print("\n[157] the guest page learns what it may do, and who it is")
    with Alone(guest_pages="on"):
        member, admin = with_accounts()
        put_access(admin, cast_access="account", page_access="off")
        info = http("GET", "/api/cast/info")[1]
        row = next((s for s in info.get("screens", []) if s["name"] == SCREEN), None)
        check("the screen is listed with its access",
              row and row["cast_access"] == "account" and row["page_access"] == "off", row)
        check("a guest is nobody", info.get("account") is None, info.get("account"))
        _, info, _ = member.call("GET", "/api/cast/info", origin=None)
        check("the member is named, even without an Origin on a GET", info.get("account") == "member", info)
        put_access(admin, cast_access="off")
        info = http("GET", "/api/cast/info")[1]
        check("off for both modes, the screen is not listed",
              all(s["name"] != SCREEN for s in info.get("screens", [])), info.get("screens"))
```

Call from `__main__`. Expected FAIL.

- [ ] **Step 2: `cast_info`**

Signature gains `identity: Option<axum::Extension<crate::accounts::Identity>>`. Before the screen loop, read both columns for all rows at once (extend the existing `SELECT name, label FROM displays` to
`SELECT name, label, COALESCE(cast_access,'anyone'), COALESCE(page_access,'anyone') FROM displays`), and inside the loop:

```rust
        use super::access::Access;
        let parse = |raw: &str| serde_json::from_value::<Access>(json!(raw)).unwrap_or_default();
        let (cast_raw, page_raw) = rows_access(&display.name); // from the extended query; "anyone" when absent
        let cast_access = parse(cast_raw).effective(enabled);
        let page_access = parse(page_raw).effective(page_enabled && !state.args.disable_cast);
        if cast_access == Access::Off && page_access == Access::Off {
            continue; // nothing a guest can do here, and listing it is enumeration
        }
```

(implement `rows_access` inline as a lookup into the query result, like `label`), add `"cast_access": cast_access, "page_access": page_access` to the screen object, and `"account": identity.map(|axum::Extension(who)| who.name)` to the top-level JSON in both branches.

- [ ] **Step 3: The page**

In `web/index.html`: near the status line, add

```html
    <p id="accountLine" class="muted" hidden></p>
    <p id="loginHint" hidden><a id="loginLink" href="/login.html?next=/">Anmelden zum Casten</a></p>
```

After the first `/api/cast/info` response is parsed (where `screens = info.screens || [];` is set), add:

```js
        // Who this browser is signed in as, if anyone: a screen kept for
        // accounts needs one, and a member should see that it counts.
        if (info.account) {
          el('accountLine').textContent = `Angemeldet als ${info.account}`;
          el('accountLine').hidden = false;
          el('loginHint').hidden = true;
        }
```

In `claim()`, inside `if (!res.ok) {` before the `return`:

```js
          // A screen kept for accounts: offer the sign-in, which brings the
          // guest back here afterwards.
          if (res.status === 401 && body.login) {
            el('loginLink').href = body.login;
            el('loginHint').hidden = false;
          }
```

`textContent` only; no `innerHTML`.

- [ ] **Step 4: Run and check in a browser**

Run: `touch src/web.rs && cargo build && python3 tests/cast/test_castaccess.py && python3 tests/cast/test_castscreens.py`
Expected: pass. Headless: on a scratch server with `werkstatt` set to `account`, claim on the guest page shows the "Anmelden zum Casten" link; after signing in via the link and returning, "Angemeldet als …" shows and the claim succeeds.

- [ ] **Step 5: Commit**

```bash
git add src/cast/api.rs web/index.html tests/cast/test_castaccess.py src/web.rs
git commit -m "Tell the guest page who may cast and offer the sign-in"
```

---

### Task 5: The operator page

**Files:**
- Modify: `web/displays.html`

- [ ] **Step 1: Two selects per card**

Find where a card's label input and save button are built (the per-card builder in `displays.html`). Add, with the page's `el`/`createElement` helper:

```js
      // Who may use this screen, per mode. Below the venue's switches: with
      // casting off everywhere the choice here is shown but cannot apply.
      const ACCESS = [['anyone', 'jeder'], ['account', 'nur Konto'], ['off', 'aus']];
      const accessSelect = (value) => {
        const select = document.createElement('select');
        for (const [v, text] of ACCESS) {
          const option = document.createElement('option');
          option.value = v;
          option.textContent = text;
          select.append(option);
        }
        select.value = value || 'anyone';
        return select;
      };
      const castAccess = accessSelect(display.cast_access);
      const pageAccess = accessSelect(display.page_access);
```

Place them as two labelled fields ("Casten", "Webseite") beside the label field; include them in the card's dirty tracking the same way the label input is; include `cast_access: castAccess.value, page_access: pageAccess.value` in the `PUT /api/displays/{name}` body the card's save sends. If `display.cast_user` is set, show `gecastet von ${display.cast_user}` (textContent) in the card head.

Read `/api/settings` once on load (the page may already); if `cast_enabled` is false disable `castAccess` with `title = 'Casten ist überall aus (Einstellungen)'`; same for `guest_pages_enabled` and `pageAccess`.

- [ ] **Step 2: Check in a browser**

Run: `cargo build`, scratch server with two screens, open `/displays.html`, set `werkstatt` to "nur Konto" / "aus", save, reload: values persist; `GET /api/displays` shows them.

- [ ] **Step 3: Commit**

```bash
git add web/displays.html
git commit -m "Choose who may cast per screen on the displays page"
```

---

### Task 6: Documentation

**Files:**
- Modify: `CLAUDE.md`, `README.md`, `docs/features.md`, `docs/casting.md`, `docs/roadmap.md`, the spec

- [ ] **Step 1: CLAUDE.md** — in "HTTP: three audiences, two predicates", after the `is_cast_public_path` bullet, add:

```markdown
- **The cast branch recognises an account but never requires one.** A valid
  session cookie attaches the `Identity` (for a write only with this host's
  `Origin`); nothing else changes, and a request without one is a guest exactly
  as before. HTTP Basic is ignored there. `cast::access` decides per screen and
  mode — in `authorize_sender` *and* in the socket's `present` frame, because
  the page mode reuses the ticket the code check claimed in cast mode. The
  venue switches stay above it; a screen can only narrow them.
```

In "Casting across screens", add that `displays.cast_access`/`page_access` are per screen while `cast_enabled`, the auth mode and guest pages stay the venue's, and that switching a screen's mode off ends that screen's session of that mode (`running_mode`).

- [ ] **Step 2: README / features / casting** — README: `PUT /api/displays/{name}` takes `cast_access`/`page_access` (`anyone|account|off`), `GET /api/displays` returns them and `cast_user`; `/api/cast/info` screens carry `cast_access`/`page_access` and the top level `account`; the claim's `401` with `login`. `docs/features.md` and `docs/casting.md`: a short operator-facing section ("Wer darf casten"). Webhook table: `user` on `cast.started` and `guest_page.shown`.

- [ ] **Step 3: Roadmap and spec** — delete the "Who may cast" entry from `docs/roadmap.md`; spec `Status: implemented`.

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md README.md docs
git commit -m "Document who may cast"
```

---

## Final verification

- [ ] `cargo test`
- [ ] Local instance on 3000 stopped (and its Chrome on 9222); then one at a time every suite in `tests/cast/` (known pre-existing: `test_public.py`/`test_managed.py` mdns timing).
- [ ] Restart the user's local instance with the release build.
