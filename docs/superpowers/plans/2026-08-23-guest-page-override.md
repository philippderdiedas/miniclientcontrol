# Guest Page Override Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a guest hand the kiosk a URL to display instead of sharing a screen, when the operator has allowed it.

**Architecture:** `CastSession` gains a `Showing` mode rather than a sibling session type, because one screen means one override slot and a second session would duplicate the claim, ticket, ping, grace and watchdog machinery it already owns. The guest declares intent at claim time, so `register_peer` knows not to pin the cast page. Downloads are refused browser-wide, unconditionally, because a URL need not be a page.

**Tech Stack:** Rust (axum, tokio, sqlx, chromiumoxide), `url` crate for parsing, vanilla HTML/JS for both UIs, stdlib-only Python for the end-to-end tests.

**Spec:** `docs/superpowers/specs/2026-08-23-guest-page-override-design.md`

## Global Constraints

- **`cargo build` is the gate for the Rust side.** There is no linting config.
- **Stop any locally running instance before the Python suite.** `test_port.py` needs 3443 free; `test_browser.py`/`test_overlay.py` launch Chrome on 9222/9232.
- **`web/` is compiled into the binary** via `include_dir!`. Any change under `web/` needs a rebuild before it has any effect.
- **UI strings are German. Code, comments, commit messages and docs are English.**
- **Build rows with `textContent`/`createElement`, never `innerHTML` interpolation.** URLs here are guest-supplied and therefore attacker-influenced.
- **Read settings from `state.settings`, never from `state.args`.**
- **Use `notify_one()`, never `notify_waiters()`** on the `AppState` notifies.
- **Handler DB errors are swallowed so the display never dies, but `error!`-logged first.**
- **Never add a dependency without checking `cross build --release --target armv7-unknown-linux-gnueabihf --target-dir target/cross-armv7`.** Anything pulling `aws-lc-rs` or a C toolchain breaks the device build.
- The `url` crate (2.5.8) is already in the tree via `chromiumoxide`; adding it as a direct dependency compiles nothing new.

---

### Task 1: Refuse downloads browser-wide

Independent of the rest and valuable on its own: today any URL served as an attachment silently writes a file, and on an SD card enough of that fills the disk.

**Files:**
- Modify: `src/browser.rs:35-42` (the handler config and connect block)
- Modify: `CLAUDE.md` (invariant), `docs/casting.md`, `docs/raspberry-pi.md` (the policy twin)

**Interfaces:**
- Consumes: nothing
- Produces: nothing other tasks call. The behaviour is global.

- [ ] **Step 1: Find the exact insertion point**

Run: `grep -n 'ignore_https_errors' -A 12 src/browser.rs`

You are looking for the block that builds `HandlerConfig` and then calls
`Browser::connect_with_config`. The new command goes *after* a successful
connect, because it needs the `browser` handle.

- [ ] **Step 2: Send `Browser.setDownloadBehavior` once per connection**

Add the import near the other `chromiumoxide::cdp` imports at the top of `src/browser.rs`:

```rust
use chromiumoxide::cdp::browser_protocol::browser::{
    SetDownloadBehaviorBehavior, SetDownloadBehaviorParams,
};
```

Immediately after the connect succeeds and before the control page is picked, add:

```rust
    // A URL need not be a page. Anything served with `Content-Disposition:
    // attachment` -- or any type Chromium will not render -- becomes a
    // *download*, and enough of those fill an SD card and take the database,
    // the certificate and the uploads with it. A guest who can set a URL could
    // do that on purpose.
    //
    // Browser-wide rather than per page, so it covers every target we ever
    // navigate, and unconditional rather than tied to the guest-page setting:
    // a guard against exhaustion that depends on a switch has the wrong shape,
    // and a signage display never wanted a download in the first place.
    //
    // `events_enabled` costs nothing here and is what lets a guest be told
    // their link was a file rather than watching the screen not change.
    if let Err(e) = browser
        .execute(
            SetDownloadBehaviorParams::builder()
                .behavior(SetDownloadBehaviorBehavior::Deny)
                .events_enabled(true)
                .build()
                .expect("behavior is set, so the builder cannot fail"),
        )
        .await
    {
        // Not fatal: an older browser without the command is still better off
        // running than not running.
        warn!("Could not disable downloads: {e}");
    }
```

Make sure `warn` is imported in that file; if not, add it to the existing `tracing` import.

- [ ] **Step 3: Build**

Run: `cargo build`
Expected: `Finished` with no errors.

- [ ] **Step 4: Verify against a real browser**

Start a Chrome on the CDP port the tests use, run the controller against it, and
navigate to something that downloads.

```bash
mkdir -p /tmp/dlspike/dl /tmp/dlspike/profile
printf 'hello' > /tmp/dlspike/serve/blob.bin 2>/dev/null || { mkdir -p /tmp/dlspike/serve && printf 'hello' > /tmp/dlspike/serve/blob.bin; }
( cd /tmp/dlspike/serve && python3 -m http.server 8099 >/dev/null 2>&1 & echo $! > /tmp/dlspike/http.pid )
google-chrome-stable --headless=new --remote-debugging-port=9242 \
  --user-data-dir=/tmp/dlspike/profile --no-first-run \
  --download-directory=/tmp/dlspike/dl about:blank >/dev/null 2>&1 &
sleep 3
./target/debug/miniclientcontrol --port 3062 --cast-tls-port 3465 \
  --cdp-url http://127.0.0.1:9242 --no-launch-browser --managed-cert off \
  --database-path /tmp/dlspike/t.db --assets-dir /tmp/dlspike/assets \
  --cast-cert-path /tmp/dlspike/cert.pem >/tmp/dlspike/log 2>&1 &
sleep 3
curl -s -X POST http://127.0.0.1:3062/api/override \
  -H 'content-type: application/json' \
  -d '{"url":"http://127.0.0.1:8099/blob.bin"}'
sleep 6
ls -la /tmp/dlspike/dl
```

Expected: `/tmp/dlspike/dl` is **empty**. Before this change it contains `blob.bin`.

To see the failure mode first, stash the change (`git stash`), rerun, observe the
file appear, then `git stash pop`.

Clean up:

```bash
pkill -f 'remote-debugging-port=9242'; kill "$(cat /tmp/dlspike/http.pid)"
pkill -f 'cast-tls-port 3465'
```

- [ ] **Step 5: Automate it in `test_browser.py`**

The Python suite's other files run with `--no-launch-browser`, so there is no
browser in them to download anything. `test_browser.py` already starts a real
Chrome, which makes it the only place this can be asserted.

Give the display Chrome a download directory it can be checked against — in the
`chrome(9222, "display-profile")` call, add:

```python
    chrome(9222, "display-profile", f"--download-directory={SP}/downloads")
```

Add near the end of `main()`, after the existing cast cases:

```python
    print("\n[12] a link that is a file downloads nothing")
    body = b"x" * 4096
    class Attachment(BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Disposition", 'attachment; filename="blob.bin"')
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        def log_message(self, *a):
            pass

    shutil.rmtree(f"{SP}/downloads", ignore_errors=True)
    os.makedirs(f"{SP}/downloads", exist_ok=True)
    blobsrv = socketserver.TCPServer(("127.0.0.1", 0), Attachment)
    threading.Thread(target=blobsrv.serve_forever, daemon=True).start()
    http("POST", "/api/override",
         {"url": f"http://127.0.0.1:{blobsrv.server_address[1]}/blob.bin"})
    await asyncio.sleep(6)
    check("nothing was written to disk", os.listdir(f"{SP}/downloads") == [],
          os.listdir(f"{SP}/downloads"))
    blobsrv.shutdown()
```

Add the imports at the top of the file, **not** as `import http.server`: that
would bind the name `http` and shadow the request helper the file already
imports.

```python
import os, shutil, socketserver, threading
from http.server import BaseHTTPRequestHandler
```

(Some of these are likely already imported; add only what is missing.)

Run: `cd tests/cast && python3 test_browser.py`
Expected: `ALL PASSED`, including the new case. Stash the `src/browser.rs` change
and rerun to watch it fail — that is what proves the test is testing something.

- [ ] **Step 6: Document the invariant**

In `CLAUDE.md`, under `## The display browser (\`src/chromium.rs\`)`, add:

```markdown
**Downloads are refused browser-wide** (`Browser.setDownloadBehavior` with
`deny`, sent once per CDP connection in `browser_loop` beside the certificate
decision). A URL served as an attachment would otherwise write a file, and
enough of those fill an SD card and take the database and the certificate with
it. Unconditional on purpose — a guard against exhaustion must not depend on a
setting. `eventsEnabled` is on so `Browser.downloadWillBegin` can tell a guest
their link was a file.
```

In `docs/raspberry-pi.md`, after the translate-policy block, add the twin:

```markdown
## Downloads

The controller refuses downloads over CDP for the browser it drives. For a
browser started outside it, the managed policy is the equivalent, and needs root
at `/etc/chromium/policies/managed/no-downloads.json`:

```json
{ "DownloadRestrictions": 3 }
```

`3` is "block all downloads". Same shape and same caveats as the no-translate
policy above.
```

- [ ] **Step 7: Commit**

```bash
git add src/browser.rs tests/cast/test_browser.py CLAUDE.md docs/raspberry-pi.md
git commit -m "Refuse downloads in the display browser

A URL need not be a page: anything served as an attachment becomes a download,
and on an SD card enough of them fill the filesystem and take the database, the
certificate and the uploads with it.

Browser.setDownloadBehavior deny, once per CDP connection beside the certificate
decision. Unconditional rather than tied to any setting -- a signage display
never wanted a download, so there is no case to keep working."
```

---

### Task 2: The setting

**Files:**
- Modify: `src/settings.rs` (key, `AppSettings`, `Locks`, resolve, persist, `UpdateRequest`, `update_settings`)
- Modify: `src/models.rs` (the `--guest-pages` flag)
- Test: `src/settings.rs` (inline `#[cfg(test)]`), `tests/cast/test_settings.py`

**Interfaces:**
- Consumes: nothing
- Produces: `AppSettings::guest_pages_enabled: bool` and `Locks::guest_pages: bool`, read by Task 4 in `authorize_sender` and reported by Task 6.

- [ ] **Step 1: Read how an existing boolean setting is threaded**

Run: `grep -n 'cast_enabled' src/settings.rs src/models.rs`

Every place that names `cast_enabled` needs a sibling for `guest_pages_enabled`,
except that its default is **false**, not true.

- [ ] **Step 2: Add the CLI flag**

In `src/models.rs`, beside `pub managed_cert: String`:

```rust
    /// Whether guests may put a web page on the display instead of casting.
    ///
    /// Off unless said otherwise: this decides whether strangers on the LAN can
    /// place content on the venue's screen. Passing the flag pins the setting,
    /// so a deployment can nail it down and the admin UI shows it as locked.
    #[arg(long, env, value_parser = ["on", "off"])]
    pub guest_pages: Option<String>,
```

No `default_value`: `None` has to mean "not given", which is what makes the lock
work.

- [ ] **Step 3: Thread it through settings**

In `src/settings.rs`, add the key beside the others:

```rust
const KEY_GUEST_PAGES: &str = "guest_pages_enabled";
```

Add to `AppSettings`:

```rust
    /// Whether a guest may put a web page on the display.
    ///
    /// Independent of `cast_enabled`: rendering a page costs the device almost
    /// nothing while WebRTC costs it a great deal, so a weak display may sensibly
    /// allow one and refuse the other.
    pub guest_pages_enabled: bool,
```

Add to `Locks`:

```rust
    pub guest_pages: bool,
```

In the resolve function, beside `stored_enabled`:

```rust
    let stored_guest_pages = crate::db::load_setting(pool, KEY_GUEST_PAGES)
        .await
        .map(|raw| raw == "true")
        .unwrap_or(false);
```

and in the struct literal it builds:

```rust
        guest_pages_enabled: match args.guest_pages.as_deref() {
            Some("on") => true,
            Some("off") => false,
            _ => stored_guest_pages,
        },
```

In the `Locks` literal:

```rust
            guest_pages: args.guest_pages.is_some(),
```

In the persist row list, beside `KEY_CAST_ENABLED`:

```rust
        (KEY_GUEST_PAGES, settings.guest_pages_enabled.to_string()),
```

In `UpdateRequest`:

```rust
    guest_pages_enabled: Option<bool>,
```

In `update_settings`, directly after the `cast_enabled` arm:

```rust
    if let Some(enabled) = payload.guest_pages_enabled {
        if enabled != next.guest_pages_enabled {
            if state.locks.guest_pages {
                return locked("--guest-pages");
            }
            next.guest_pages_enabled = enabled;
        }
    }
```

The `if enabled != next...` guard matters and is not decoration: without it,
saving the settings form while the flag is set would `409` even when the value
did not change, so the operator could not edit anything else on the page.

Wherever `AppSettings` is constructed in tests or defaults, add the field.

- [ ] **Step 4: Write the failing unit test**

Add to the `#[cfg(test)]` module in `src/settings.rs`, or create one if absent:

```rust
    #[test]
    fn the_flag_pins_guest_pages_and_the_default_is_off() {
        // The default matters: this decides whether strangers can put content on
        // the venue's screen, so it is off until somebody says otherwise.
        assert!(!resolve_guest_pages(None, None));
        assert!(!resolve_guest_pages(None, Some(false)));
        assert!(resolve_guest_pages(None, Some(true)));
        // A flag that was actually passed wins over the stored value in both
        // directions -- that is the recovery path, not just deference.
        assert!(resolve_guest_pages(Some("on"), Some(false)));
        assert!(!resolve_guest_pages(Some("off"), Some(true)));
    }
```

and extract the decision the resolve function makes into a testable helper next
to it:

```rust
/// `--guest-pages` wins over the stored value; absent both, off.
fn resolve_guest_pages(flag: Option<&str>, stored: Option<bool>) -> bool {
    match flag {
        Some("on") => true,
        Some("off") => false,
        _ => stored.unwrap_or(false),
    }
}
```

Then call `resolve_guest_pages(args.guest_pages.as_deref(), Some(stored_guest_pages))`
from the resolve function instead of the inline `match` written in Step 3, so the
tested code is the code that runs.

- [ ] **Step 5: Run the test to see it fail, then pass**

Run: `cargo test --bins the_flag_pins_guest_pages`
Expected first: a compile error naming `resolve_guest_pages`, until Step 4's
helper exists. After it exists: PASS.

- [ ] **Step 6: Extend the Python settings test**

In `tests/cast/test_settings.py`, following the shape already used for
`cast_enabled`, add cases that the setting round-trips through
`PUT`/`GET /api/settings`, that it persists across a restart, and that
`--guest-pages off` makes a `PUT` answer `409` naming the flag.

Run: `cd tests/cast && python3 test_settings.py`
Expected: `ALL PASSED`.

- [ ] **Step 7: Commit**

```bash
git add src/settings.rs src/models.rs tests/cast/test_settings.py
git commit -m "Add the guest-pages setting

Off by default, because it decides whether strangers on the LAN may put content
on the venue's screen, and pinnable with --guest-pages so a deployment can nail
it down. Independent of cast_enabled: rendering a page costs the device almost
nothing while WebRTC costs it a great deal."
```

---

### Task 3: URL validation and redaction

Pure functions, no wiring. Everything later depends on these two names.

**Files:**
- Create: `src/guest_page.rs`
- Modify: `src/main.rs` (add `mod guest_page;`), `Cargo.toml` (direct `url` dependency)

**Interfaces:**
- Consumes: nothing
- Produces:
  - `pub fn parse_guest_url(raw: &str) -> Result<url::Url, &'static str>`
  - `pub fn redact(url: &url::Url) -> String`
  - `pub const MAX_URL_LEN: usize = 2048;`

  Task 4 calls `parse_guest_url` on the `present` frame and stores the `Url`;
  Task 6 calls `redact` for `/api/cast/state` and every log line.

- [ ] **Step 1: Add the dependency**

In `Cargo.toml`, beside the other direct dependencies:

```toml
# Already in the tree via chromiumoxide; direct so guest URLs are parsed rather
# than pattern-matched.
url = "2"
```

Run: `cargo build`
Expected: `Finished`, nothing new compiled.

- [ ] **Step 2: Write the failing test**

Create `src/guest_page.rs` containing only the test module for now:

```rust
//! Validating and displaying a URL a guest asked the kiosk to open.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_and_https_are_accepted() {
        assert!(parse_guest_url("https://example.test/menu").is_ok());
        assert!(parse_guest_url("http://example.test/menu").is_ok());
        assert!(parse_guest_url("HTTPS://example.test/").is_ok());
        // The display is an output channel and the process runs as a user with
        // a filesystem.
        assert!(parse_guest_url("file:///etc/passwd").is_err());
        assert!(parse_guest_url("chrome://settings").is_err());
        assert!(parse_guest_url("javascript:alert(1)").is_err());
        assert!(parse_guest_url("data:text/html,hi").is_err());
    }

    #[test]
    fn a_url_needs_a_host_and_a_sane_length() {
        assert!(parse_guest_url("https://").is_err());
        assert!(parse_guest_url("not a url").is_err());
        assert!(parse_guest_url("").is_err());
        let long = format!("https://example.test/{}", "a".repeat(MAX_URL_LEN));
        assert!(parse_guest_url(&long).is_err());
    }

    #[test]
    fn lan_targets_are_allowed_on_purpose() {
        // Decision on record: a venue may want an internal dashboard on screen.
        assert!(parse_guest_url("http://192.168.1.1/").is_ok());
        assert!(parse_guest_url("http://127.0.0.1:3000/").is_ok());
    }

    #[test]
    fn credentials_survive_to_the_browser_but_never_to_a_log() {
        let parsed = parse_guest_url("https://admin:hunter2@wiki.intern/page").unwrap();
        // The browser gets them: refusing would break the internal-dashboard
        // case, and ?token= would be equivalent and unfilterable anyway.
        assert!(parsed.as_str().contains("hunter2"));
        // Nothing that is displayed or logged does.
        let shown = redact(&parsed);
        assert!(!shown.contains("hunter2"));
        assert!(!shown.contains("admin"));
        assert_eq!(shown, "https://wiki.intern/page");
    }

    #[test]
    fn redaction_reveals_a_deceptive_host() {
        // Reads like Google in a card that prints the raw string.
        let parsed = parse_guest_url("http://google.com@evil.test/").unwrap();
        assert_eq!(redact(&parsed), "http://evil.test/");
    }
}
```

- [ ] **Step 3: Run it to see it fail**

Run: `cargo test --bins guest_page`
Expected: FAIL — `cannot find function \`parse_guest_url\``.

You will also need `mod guest_page;` in `src/main.rs` beside the other module
declarations before the test is even compiled.

- [ ] **Step 4: Implement**

Above the test module in `src/guest_page.rs`:

```rust
use url::Url;

/// Long enough for any real link, short enough that a frame cannot be used to
/// push megabytes through the session.
pub const MAX_URL_LEN: usize = 2048;

/// Parse what a guest typed, or say why it will not do.
///
/// The error strings are shown to the guest, so they are German and say what to
/// do rather than what went wrong internally.
pub fn parse_guest_url(raw: &str) -> Result<Url, &'static str> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("Bitte eine Adresse eingeben.");
    }
    if raw.len() > MAX_URL_LEN {
        return Err("Die Adresse ist zu lang.");
    }
    let parsed = Url::parse(raw).map_err(|_| "Das ist keine gültige Adresse.")?;
    // Scheme first: `file:` and `data:` would turn the display into a reader for
    // whatever the device can reach on disk, and the kiosk has no one to refuse.
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("Nur http:// und https:// sind möglich.");
    }
    // `Url::parse` accepts a scheme with no authority for some schemes; for
    // http(s) an empty host is still worth refusing explicitly.
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err("Die Adresse hat keinen Host.");
    }
    Ok(parsed)
}

/// The URL as it may be logged or shown to the operator.
///
/// Credentials are accepted (see `parse_guest_url`) but must never be echoed: a
/// guest typing `https://admin:hunter2@wiki.intern/` would otherwise put their
/// password in journald and on the admin screen. Dropping the userinfo also
/// disposes of `http://google.com@evil.test`, which reads like Google until it
/// is reserialised without it.
pub fn redact(url: &Url) -> String {
    let mut shown = url.clone();
    let _ = shown.set_username("");
    let _ = shown.set_password(None);
    shown.to_string()
}
```

Note `is_none_or` needs a recent Rust; if the toolchain rejects it, use
`map_or(true, str::is_empty)`.

- [ ] **Step 5: Run the tests**

Run: `cargo test --bins guest_page`
Expected: PASS, 5 tests.

If `redaction_reveals_a_deceptive_host` fails on a trailing slash, adjust the
*expectation* to whatever `Url` canonicalises to — do not weaken the assertion to
a `contains`.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/guest_page.rs src/main.rs
git commit -m "Validate and redact a guest-supplied URL

http and https only, with a host and a length cap. LAN targets stay allowed on
purpose: a venue may want an internal dashboard on screen.

Credentials in a URL are accepted but never echoed. Refusing them would be
theatre, since ?token= is equivalent and cannot be filtered, and it would break
that same internal-dashboard case. The real harm is that they reach the log and
the admin card, so redaction is where it is solved -- which also unmasks
http://google.com@evil.test as the evil.test it is."
```

---

### Task 4: The session learns what it is showing

The core change. Everything here is in `src/cast.rs`.

**Files:**
- Modify: `src/cast.rs` (`Showing`, `Reservation`, `CastSession`, `activate_display`, `deactivate_display`, `watch_sender_grace`, `register_peer`, `handle_frame`, `authorize_sender`, `claim_session`, `consume_reservation`)

**Interfaces:**
- Consumes: `guest_page::parse_guest_url`, `guest_page::redact` (Task 3); `AppSettings::guest_pages_enabled` (Task 2)
- Produces: `CastSession::showing: Showing` and `pub fn showing_json(&self) -> serde_json::Value`, read by Task 6.

- [ ] **Step 1: Add the mode and the longer grace**

Beside the other constants in `src/cast.rs`:

```rust
/// Grace for a guest page, rather than the cast's five seconds.
///
/// The expected normal case here is a phone whose tab was backgrounded, not a
/// page reload. The keepalive itself survives that -- it is a protocol-level
/// ping the browser's network stack answers without waking any JavaScript -- so
/// what this covers is a tab the OS *discarded*, which takes longer to come back.
const PAGE_GRACE: Duration = Duration::from_secs(30);
```

Add the mode near `Reservation`:

```rust
/// What the session has put on the display.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Showing {
    #[default]
    Nothing,
    Cast,
    Page {
        /// Full, including any credentials -- the browser needs them.
        url: Url,
        scroll: ScrollMode,
    },
}

/// What a guest said they were going to do, decided at claim time.
///
/// It has to be known before the socket connects: `register_peer` activates the
/// display the moment a sender arrives, and a page-mode sender must not put
/// `cast_display.html` on screen on its way to the guest's URL.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaimMode {
    #[default]
    Cast,
    Page,
}
```

Add `mode: ClaimMode` to `Reservation`, and `showing: Showing` plus
`pending_mode: ClaimMode` to `CastSession`. Add `use url::Url;` to the imports.

- [ ] **Step 2: Make activate/deactivate take what they install**

Replace the hardcoded target in `activate_display` with a parameter. The
signature becomes:

```rust
async fn activate_display(state: &AppState, showing: Showing) {
```

Inside, build the `OverrideItem` from `showing` instead of always the cast page:

```rust
    let (url, scroll) = match &showing {
        Showing::Cast => (cast_display_url(state.args.port), ScrollMode::None),
        Showing::Page { url, scroll } => (url.to_string(), scroll.clone()),
        Showing::Nothing => return,
    };
    {
        let mut current = state.override_item.lock().await;
        session.previous_override = current.clone();
        *current = Some(OverrideItem {
            asset_id: None,
            url: Some(url),
            local_path: None,
            mimetype: None,
            scroll_config: scroll,
        });
    }
    session.showing = showing.clone();
```

Keep the rest as it is. Then replace the tail:

```rust
    state.override_signal.notify_one();
    match &showing {
        Showing::Cast => {
            info!("Cast: display pinned to the cast page");
            // Only a cast has a display peer to wait for. A page has none, and
            // this watchdog would tear it down after its deadline for a peer
            // that was never coming.
            watch_display_arrival(state.clone(), epoch);
        }
        Showing::Page { url, .. } => info!("Cast: guest page pinned to {}", crate::guest_page::redact(url)),
        Showing::Nothing => {}
    }
```

In `deactivate_display`, the "is it still ours" check must compare against
whatever we installed rather than always the cast page:

```rust
    let ours = match &session.showing {
        Showing::Cast => Some(cast_display_url(state.args.port)),
        Showing::Page { url, .. } => Some(url.to_string()),
        Showing::Nothing => None,
    };
    {
        let mut current = state.override_item.lock().await;
        let still_ours = ours.is_some()
            && current.as_ref().and_then(|item| item.url.as_deref()) == ours.as_deref();
        if still_ours {
            *current = session.previous_override.take();
        } else {
            debug!("Cast: override changed during the session, leaving it alone");
        }
    }
```

and clear `session.showing = Showing::Nothing;` beside the other resets.

Update the one existing call site in `register_peer` (Step 4 rewrites it anyway).

- [ ] **Step 3: Pick the grace by what is showing**

In `unregister_peer`, the grace call needs to know the mode. Capture it in the
same lock that reads `holding`:

```rust
        let grace = match session.showing {
            Showing::Page { .. } => PAGE_GRACE,
            _ => SENDER_GRACE,
        };
        (counterpart, session.epoch, session.holding_override, grace)
```

and pass it through:

```rust
fn watch_sender_grace(state: AppState, epoch: u64, grace: Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(grace).await;
```

- [ ] **Step 4: Do not pin the cast page for a page-mode sender**

At the end of `register_peer`, replace the unconditional activation:

```rust
    // The sender arriving is what puts something on screen. Doing it here rather
    // than at the HTTP upgrade means a sender that fails to establish its socket
    // never interrupts the playlist.
    //
    // A page-mode sender activates nothing yet: it has not said *what* to show.
    // Its `present` frame does that.
    if role == Role::Sender {
        let mode = state.cast.lock().await.pending_mode;
        if mode == ClaimMode::Cast {
            activate_display(state, Showing::Cast).await;
        }
    }
```

- [ ] **Step 5: Handle the `present` frame**

In `handle_frame`, beside the `limits` arm:

```rust
        // Only a sender may say this, for the same reason only a display may
        // send `limits`: it is a statement about what the guest wants shown, and
        // the display has no business making it.
        Some("present") if role == Role::Sender => {
            let allowed = state.settings.read().await.guest_pages_enabled;
            if !allowed {
                let tx = state.cast.lock().await.sender.as_ref().map(|p| p.tx.clone());
                if let Some(tx) = tx {
                    let _ = tx.send(error_frame("page", "Webseiten sind derzeit nicht erlaubt."));
                }
                return true;
            }

            let raw = value.get("url").and_then(|u| u.as_str()).unwrap_or_default();
            let parsed = match crate::guest_page::parse_guest_url(raw) {
                Ok(parsed) => parsed,
                Err(reason) => {
                    let tx = state.cast.lock().await.sender.as_ref().map(|p| p.tx.clone());
                    if let Some(tx) = tx {
                        let _ = tx.send(error_frame("page", reason));
                    }
                    return true;
                }
            };

            let scroll = match value.get("scroll").and_then(|s| s.as_str()) {
                // The operator UI's own defaults, so a guest page reads like a
                // playlist item rather than like a different feature.
                Some("slow") => ScrollMode::Continuous(ScrollOptions {
                    speed: 2.0,
                    top_delay: 2000,
                    return_delay: 2000,
                }),
                _ => ScrollMode::None,
            };

            // A second `present` replaces the first rather than being refused:
            // mistyping an address should not cost the guest their slot and a
            // fresh claim. Releasing first keeps `previous_override` pointing at
            // the playlist rather than at the guest's own previous page.
            deactivate_display(state).await;
            let showing = Showing::Page { url: parsed.clone(), scroll };
            activate_display(state, showing).await;

            let tx = state.cast.lock().await.sender.as_ref().map(|p| p.tx.clone());
            if let Some(tx) = tx {
                let _ = tx.send(Message::Text(
                    json!({"type": "presenting", "url": crate::guest_page::redact(&parsed)})
                        .to_string()
                        .into(),
                ));
            }
            true
        }
```

Import `ScrollOptions` alongside `ScrollMode` at the top of the file.

- [ ] **Step 6: Make the gate mode-aware**

In `authorize_sender`, take the mode as an argument and check the right switch:

```rust
async fn authorize_sender(
    state: &AppState,
    addr: IpAddr,
    provided: Option<&str>,
    mode: ClaimMode,
) -> Result<(), String> {
    let settings = {
        let settings = state.settings.read().await;
        (
            settings.cast_enabled,
            settings.guest_pages_enabled,
            settings.cast_auth,
            settings.cast_code.clone(),
        )
    };
    let (cast_enabled, pages_enabled, auth_mode, configured_code) = settings;
```

and replace the `if !enabled` block:

```rust
    // The two capabilities are independent: a device too weak for WebRTC can
    // still render a page, so refusing one must not refuse the other.
    let enabled = match mode {
        ClaimMode::Cast => cast_enabled,
        ClaimMode::Page => pages_enabled,
    };
    if !enabled {
        return Err(match mode {
            ClaimMode::Cast => "Übertragung ist derzeit deaktiviert.".to_string(),
            ClaimMode::Page => "Webseiten sind derzeit nicht erlaubt.".to_string(),
        });
    }
```

The code check that follows stays exactly as it is: the same door guards both.

- [ ] **Step 7: Carry the mode through claim**

Add to `ClaimRequest`:

```rust
    #[serde(default)]
    mode: ClaimMode,
```

In `claim_session`, pass `payload.mode` to `authorize_sender`, and store it:

```rust
        session.reservation = Some(Reservation {
            ticket: ticket.clone(),
            addr,
            expires_at: Instant::now() + RESERVATION_TTL,
            mode: payload.mode,
        });
```

In `consume_reservation`, when the reservation is accepted, move the mode onto
the session:

```rust
        session.pending_mode = reservation.mode;
```

- [ ] **Step 8: Expose what is showing**

Add to `impl CastSession`:

```rust
    /// What is on screen, for the operator's view. The URL is redacted: this is
    /// rendered into the admin page and written to logs.
    pub fn showing_json(&self) -> serde_json::Value {
        match &self.showing {
            Showing::Nothing => serde_json::Value::Null,
            Showing::Cast => json!("cast"),
            Showing::Page { url, .. } => json!({ "page": crate::guest_page::redact(url) }),
        }
    }
```

- [ ] **Step 9: Build and run the existing suite**

Run: `cargo build`
Expected: `Finished`. Fix every call-site error the signature changes produced.

Run, with no controller of your own running:

```bash
cd tests/cast
for t in test_cast.py test_conflict.py test_reserve.py test_auth.py test_pairing.py test_limits.py; do
  echo "== $t"; python3 "$t" | tail -2
done
```

Expected: `ALL PASSED` for each. These are the files that exercise the claim,
socket and override paths you just changed.

- [ ] **Step 10: Commit**

```bash
git add src/cast.rs
git commit -m "Let the cast session show a guest's page, not only a cast

One screen means one override slot, so this is a mode on the session rather than
a second session beside it -- a sibling would have duplicated the claim, ticket,
ping, grace and watchdogs, and needed arbitration in both directions.

The mode is decided at claim time because register_peer activates the display
the moment a sender's socket arrives. A page-mode sender must not pin the cast
page on its way to the guest's URL, and watch_display_arrival must not run for a
peer that is never coming.

is_active() needed no change and covers both, so a cast-sourced QR is dropped
while a guest page is up too -- the slot is taken either way."
```

---

### Task 5: The guest page UI

**Files:**
- Modify: `web/index.html`

**Interfaces:**
- Consumes: `POST /api/cast/claim` with `{"mode":"page"}`, the `present` frame, the `presenting` and `error` frames (Task 4); `page_enabled` from `/api/cast/info` (Task 6 — implement Task 6 first, or stub the field read as `info.page_enabled === true`)
- Produces: nothing other tasks consume.

- [ ] **Step 1: Read the existing share flow**

Run: `grep -n 'shareBlock\|shareScreen\|liveBlock\|claim' web/index.html | head -30`

Follow how `shareScreen` claims, opens the socket, and swaps `shareBlock` for
`liveBlock`. The page flow copies that structure; do not invent a second one.

- [ ] **Step 2: Add the markup**

Inside `#shareBlock`, after the camera button:

```html
      <button id="showPage" hidden>Webseite zeigen</button>
```

And a block beside `#liveBlock`:

```html
    <div id="pageBlock" hidden>
      <label for="pageUrl">Adresse</label>
      <input id="pageUrl" type="url" inputmode="url" placeholder="https://…"
             autocomplete="off" spellcheck="false" />
      <div class="qrow">
        <button type="button" id="pageScrollNone" class="qbtn">Nicht scrollen</button>
        <button type="button" id="pageScrollSlow" class="qbtn">Langsam scrollen</button>
      </div>
      <button class="primary" id="pageGo">Auf dem Bildschirm zeigen</button>
      <p class="muted" id="pageError"></p>
    </div>

    <div id="pageLiveBlock" hidden>
      <p class="muted" id="pageLiveUrl"></p>
      <p class="muted">Diese Seite offen lassen, sonst kehrt der Bildschirm zur Anzeige zurück.</p>
      <button id="pageStop">Beenden</button>
    </div>
```

- [ ] **Step 3: Wire it**

Following the existing script's style — `textContent`, no `innerHTML`:

```javascript
  let pageScroll = 'none';
  const showPage = document.getElementById('showPage');
  const pageBlock = document.getElementById('pageBlock');
  const pageLiveBlock = document.getElementById('pageLiveBlock');
  const pageError = document.getElementById('pageError');

  function selectScroll(which) {
    pageScroll = which;
    document.getElementById('pageScrollNone').classList.toggle('on', which === 'none');
    document.getElementById('pageScrollSlow').classList.toggle('on', which === 'slow');
  }
  document.getElementById('pageScrollNone').onclick = () => selectScroll('none');
  document.getElementById('pageScrollSlow').onclick = () => selectScroll('slow');
  selectScroll('none');

  showPage.onclick = () => {
    document.getElementById('shareBlock').hidden = true;
    pageBlock.hidden = false;
  };

  document.getElementById('pageGo').onclick = async () => {
    pageError.textContent = '';
    const url = document.getElementById('pageUrl').value.trim();
    if (!url) { pageError.textContent = 'Bitte eine Adresse eingeben.'; return; }
    // Claim first, exactly like a cast: the code is checked and the slot taken
    // before anything else, so nobody gets turned away after committing.
    const claimed = await claimSession('page');
    if (!claimed) return;
    await openSocket();
    send({ type: 'present', url, scroll: pageScroll });
  };

  document.getElementById('pageStop').onclick = () => {
    send({ type: 'stop' });
  };
```

Handle the two new inbound frames where the socket's other message types are
handled:

```javascript
      if (msg.type === 'presenting') {
        pageBlock.hidden = true;
        pageLiveBlock.hidden = false;
        // textContent, not innerHTML: this string came from a guest.
        document.getElementById('pageLiveUrl').textContent = 'Auf dem Bildschirm: ' + msg.url;
        return;
      }
      if (msg.type === 'error' && msg.code === 'page') {
        pageError.textContent = msg.message;
        return;
      }
```

`claimSession` currently sends only the code. Give it a mode:

```javascript
  async function claimSession(mode) {
    const body = { code: currentCode || undefined, mode: mode || 'cast' };
    const res = await fetch('/api/cast/claim', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    });
    const data = await res.json().catch(() => ({}));
    if (!res.ok) {
      // Refused before the guest committed to anything, which is the point of
      // claiming first.
      (mode === 'page' ? pageError : status).textContent = data.error || 'Abgelehnt.';
      return null;
    }
    ticket = data.ticket;
    return ticket;
  }
```

Update the two existing share buttons to call `claimSession('cast')`. Match the
surrounding code where it differs from the sketch above — the variable names for
the code and the ticket are whatever the file already uses.

Reveal the button from `/api/cast/info`, where the page already reacts to that
payload:

```javascript
    showPage.hidden = !info.page_enabled;
```

Casting being off hides the two share buttons and leaves this one, which is the
independent-switch case working.

- [ ] **Step 4: Rebuild and look at it**

Run: `cargo build`

`web/` is baked in by `include_dir!`, so nothing you changed exists until this
runs.

Then start an instance with the setting on and open the guest page in a browser:

```bash
./target/debug/miniclientcontrol --port 3063 --cast-tls-port 3466 \
  --guest-pages on --managed-cert off --no-launch-browser \
  --database-path /tmp/gp/t.db --assets-dir /tmp/gp/assets --cast-cert-path /tmp/gp/cert.pem
```

Expected: the third button appears; with `--guest-pages off` it does not.

- [ ] **Step 5: Commit**

```bash
git add web/index.html
git commit -m "Offer a guest the choice of showing a page

A third button beside the two share buttons, revealing an address field and the
two scroll choices. It claims the slot before anything else, the same order the
share buttons use, so a guest is never turned away after committing to
something."
```

---

### Task 6: Operator surfaces

**Files:**
- Modify: `src/cast.rs` (`cast_info`, `cast_state`), `web/admin.html`

**Interfaces:**
- Consumes: `CastSession::showing_json` (Task 4), `AppSettings::guest_pages_enabled` and `Locks::guest_pages` (Task 2)
- Produces: `page_enabled` on `/api/cast/info`, consumed by Task 5.

- [ ] **Step 1: Add `page_enabled` to the public info**

In `cast_info`, extend the settings read and the JSON:

```rust
    let (enabled, page_enabled, auth) = {
        let settings = state.settings.read().await;
        (settings.cast_enabled, settings.guest_pages_enabled, settings.cast_auth)
    };
```

and add `"page_enabled": page_enabled,` to the object it returns. Add **nothing
else** there: that endpoint is public and reaches the whole LAN.

- [ ] **Step 2: Add `showing` to the operator state**

In `cast_state`, add to the object it builds:

```rust
        "showing": session.showing_json(),
```

`showing_json` redacts, so this is safe to render into the admin page.

- [ ] **Step 3: Admin UI**

In `web/admin.html`, in the card that holds the casting settings, add a checkbox
following the exact shape of the existing `cast_enabled` control, including the
lock span:

```html
      <label><input type="checkbox" id="setGuestPages" /> Gäste dürfen Webseiten zeigen</label>
      <span class="lock" id="lockGuestPages" hidden>per Kommandozeile festgelegt</span>
```

Register it in the settings-to-element map the page already keeps
(`guest_pages_enabled: ['setGuestPages', 'lockGuestPages']`), so it saves and
locks with everything else.

In the cast status card, render what is showing:

```javascript
        const showing = state.showing;
        if (showing && showing.page) {
          // textContent: a guest supplied this.
          showingEl.textContent = 'Gast zeigt: ' + showing.page;
        } else if (showing === 'cast') {
          showingEl.textContent = 'Gast überträgt den Bildschirm';
        } else {
          showingEl.textContent = '';
        }
```

- [ ] **Step 4: Rebuild and check both**

Run: `cargo build`

```bash
curl -s http://127.0.0.1:3063/api/cast/info | python3 -m json.tool
curl -s http://127.0.0.1:3063/api/cast/state | python3 -m json.tool
```

Expected: `page_enabled` present in the first, `showing` in the second.

- [ ] **Step 5: Commit**

```bash
git add src/cast.rs web/admin.html
git commit -m "Show the operator what a guest put on the screen

The switch beside the casting options, and the status card naming the page. The
URL is redacted before it leaves the session, so a guest's credentials do not
land on the operator's screen."
```

---

### Task 7: End-to-end tests

**Files:**
- Create: `tests/cast/test_guestpage.py`
- Modify: `tests/cast/README.md`

**Interfaces:**
- Consumes: everything above.
- Produces: nothing.

- [ ] **Step 1: Read the harness**

Run: `sed -n '1,80p' tests/cast/test_cast.py`

Note `Server(**flags)`, `http()`, `check()`, `failures`, `claim()`, and `ws()`.
`Server` already defaults `managed_cert="off"`; pass `guest_pages="on"` the same
way. `ws()` claims for senders — you will need a variant that claims with
`mode="page"`, so extend `claim()` with an optional mode rather than writing a
second one.

- [ ] **Step 2: Write the test file**

```python
"""A guest putting a web page on the display instead of casting."""
import asyncio, json, os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_cast import Server, http, check, failures, claim, ws, HTTP

MENU = "http://example.test/menu"


def put_settings(body):
    """`cast_enabled` is a runtime setting, not a Server flag -- same as test_settings."""
    return http("PUT", "/api/settings", body)


def override():
    return http("GET", "/api/override")[1]


async def present(sock, url, scroll="none"):
    await sock.send(json.dumps({"type": "present", "url": url, "scroll": scroll}))


async def until(predicate, timeout=6.0):
    """Poll until the loop has caught up; the override is written by a task."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        await asyncio.sleep(0.1)
    return False


async def main():
    print("\n[41] the setting gates it, at claim time")
    with Server(guest_pages="off"):
        status, body = claim(mode="page")
        check("a page claim is refused when the setting is off",
              status == 403 and "Webseite" in body.get("error", ""), (status, body))
        # Casting is unaffected: the two switches are independent.
        check("casting still works", claim()[0] == 200)

    print("\n[42] a guest pins a page")
    with Server(guest_pages="on"):
        sock = await ws("sender", mode="page")
        # Nothing on screen yet: claiming and connecting must not interrupt the
        # playlist before the guest has said what to show.
        check("the socket alone pins nothing", override().get("active") is not True, override())

        await present(sock, MENU)
        check("the override points at the page",
              await until(lambda: override().get("url") == MENU), override())

        state = http("GET", "/api/cast/state")[1]
        check("the operator sees the page", state["showing"]["page"] == MENU, state["showing"])
        await sock.close()

    print("\n[43] scrolling comes through")
    with Server(guest_pages="on"):
        sock = await ws("sender", mode="page")
        await present(sock, MENU, scroll="slow")
        check("slow scrolling becomes a continuous override",
              await until(lambda: (override().get("scroll_config") or {}).get("type") == "Continuous"),
              override())
        await sock.close()

    print("\n[44] a bad URL costs nothing")
    with Server(guest_pages="on"):
        sock = await ws("sender", mode="page")
        await present(sock, "file:///etc/passwd")
        reply = json.loads(await sock.recv())
        check("a file:// URL is refused with a reason",
              reply.get("type") == "error" and reply.get("code") == "page", reply)
        check("and nothing was pinned", override().get("active") is not True, override())
        # The slot is still the guest's -- a typo must not cost a fresh claim.
        await present(sock, MENU)
        check("a corrected URL is accepted on the same socket",
              await until(lambda: override().get("url") == MENU), override())
        await sock.close()

    print("\n[45] credentials never reach the operator")
    with Server(guest_pages="on"):
        sock = await ws("sender", mode="page")
        await present(sock, "http://admin:hunter2@example.test/wiki")
        await until(lambda: override().get("url") is not None)
        state = http("GET", "/api/cast/state")[1]
        check("the admin view is redacted", "hunter2" not in json.dumps(state), state["showing"])
        check("the browser still gets them", "hunter2" in (override().get("url") or ""))
        await sock.close()

    print("\n[46] the screen comes back")
    with Server(guest_pages="on"):
        sock = await ws("sender", mode="page")
        await present(sock, MENU)
        await until(lambda: override().get("url") == MENU)
        status, _ = http("DELETE", "/api/cast/session")
        check("the operator can end it",
              await until(lambda: override().get("active") is not True), override())
        await sock.close()

    print("\n[47] one guest at a time, either way round")
    with Server(guest_pages="on"):
        sock = await ws("sender", mode="page")
        await present(sock, MENU)
        await until(lambda: override().get("url") == MENU)
        check("a cast cannot start while a page is up", claim()[0] == 409, claim())
        await sock.close()

    with Server(guest_pages="on"):
        caster = await ws("sender")
        check("and a page cannot start while a cast is up",
              claim(mode="page")[0] == 409, claim(mode="page"))
        await caster.close()

    print("\n[48] pages work with casting switched off")
    with Server(guest_pages="on"):
        put_settings({"cast_enabled": False})
        check("a cast claim is refused", claim()[0] == 403)
        sock = await ws("sender", mode="page")
        await present(sock, MENU)
        check("a page claim still works",
              await until(lambda: override().get("url") == MENU), override())
        await sock.close()

    print("\n[49] only a sender may present")
    with Server(guest_pages="on"):
        display = await ws("display")
        await present(display, MENU)
        await asyncio.sleep(1.0)
        check("a display-role present is ignored", override().get("active") is not True, override())
        await display.close()


    print("\n[50] the overlay treats a guest page like a cast")
    with Server(guest_pages="on"):
        # No code change makes this work -- `overlay_payload` keys on
        # `is_active()`, which covers both. Tested because it is claimed.
        http("PUT", "/api/settings", {"overlay": {
            "enabled": True, "text": "Haus-Notiz", "hide_during_cast": True,
        }})
        layers = http("GET", "/api/overlay")[1]["layers"]
        check("the overlay is up with nobody presenting", len(layers) == 1, layers)

        sock = await ws("sender", mode="page")
        await present(sock, MENU)
        await until(lambda: override().get("url") == MENU)
        layers = http("GET", "/api/overlay")[1]["layers"]
        check("and stands down while a guest page is up", layers == [], layers)
        await sock.close()

    print("\n[51] a guest who walks away hands the screen back")
    with Server(guest_pages="on"):
        sock = await ws("sender", mode="page")
        await present(sock, MENU)
        await until(lambda: override().get("url") == MENU)
        await sock.close()
        # PAGE_GRACE is 30s, deliberately longer than the cast's 5s. Slow, but
        # this is the whole liveness contract of the feature.
        check("still up during the grace period",
              override().get("url") == MENU, override())
        check("and released after it",
              await until(lambda: override().get("active") is not True, timeout=45),
              override())

asyncio.run(main())
print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
sys.exit(1 if failures else 0)
```

`cast_enabled=False` may need to be set through `PUT /api/settings` rather than a
flag, depending on what the harness supports — check how `test_settings.py` turns
casting off and follow that.

- [ ] **Step 3: Extend `claim()` and `ws()` in the harness**

In `tests/cast/test_cast.py`:

```python
def claim(code=None, port=None, mode=None):
    """POST /api/cast/claim -> (status, body). This is where a code is checked."""
    body = {}
    if code:
        body["code"] = code
    if mode:
        body["mode"] = mode
    return http("POST", "/api/cast/claim", body, port=port)
```

and give `ws()` a `mode=None` parameter that it passes to `claim`.

- [ ] **Step 4: Run it**

Run: `cd tests/cast && python3 test_guestpage.py`
Expected: `ALL PASSED`.

Where a check fails, fix the code rather than the assertion, unless the
assertion encodes something the spec did not ask for.

- [ ] **Step 5: Run the whole suite**

With no controller of your own running:

```bash
cd tests/cast
for t in test_cast.py test_pairing.py test_conflict.py test_settings.py test_auth.py \
         test_reserve.py test_basicauth.py test_port.py test_public.py test_limits.py \
         test_audio.py test_overlay.py test_managed.py test_guestpage.py test_browser.py; do
  printf '%-22s ' "$t"
  timeout 400 python3 "$t" >/tmp/gp-$t.log 2>&1 && echo PASS || echo FAIL
done
```

Expected: 15 PASS.

Add the new file to the list in `tests/cast/README.md` with a one-line
description, following the existing entries.

- [ ] **Step 6: Commit**

```bash
git add tests/cast/test_guestpage.py tests/cast/test_cast.py tests/cast/README.md
git commit -m "Cover the guest page end to end

Includes the two that would be easy to get wrong and invisible if broken: that
connecting the socket alone pins nothing, and that a rejected URL leaves the
guest holding their slot so a typo does not cost a fresh claim."
```

---

### Task 8: Documentation

**Files:**
- Modify: `docs/casting.md`, `docs/features.md`, `docs/troubleshooting.md`, `README.md`, `CLAUDE.md`

**Interfaces:** none.

- [ ] **Step 1: `docs/casting.md`**

Add a section after "A real certificate, for a private address":

```markdown
## A guest showing a page

Sharing a screen is more machinery than some moments need. A guest who wants the
kiosk to show a menu, a schedule or a link can hand it the address instead, and
the kiosk loads the page itself — no video codec, no laptop pinned to the room,
and it looks better than a re-encoded screenshot of the same page.

It is off until an operator turns it on, and it is a switch of its own rather
than part of casting. Rendering a page costs the device almost nothing while
WebRTC costs it a great deal, so a display too weak to receive a cast can still
be given a page.

Everything else is the cast's: the same code, the same claim before anything
happens, one guest at a time, and the operator's stop button. The guest holds a
socket for as long as the page is up, and closing it hands the screen back after
a grace period — thirty seconds here rather than the cast's five, because a phone
whose tab was backgrounded is the expected case and not a fault.

Only `http` and `https`, and addresses on the local network are allowed
deliberately: a venue may want its own dashboard on the screen. Credentials in an
address are accepted for the same reason, but are stripped everywhere the address
is logged or shown — so a guest's password does not end up in the journal or on
the operator's screen, and `http://google.com@evil.test` is displayed as the
`evil.test` it is.

**Note what a guest can therefore see.** With operator authentication switched
off, a guest can point the kiosk at its own admin page and read the cast code off
the screen. They cannot operate it — a kiosk has no keyboard — but it is legible.
Turning authentication on is the answer; carving out one address would leave
every other internal page reachable anyway.
```

- [ ] **Step 2: `docs/features.md`**

In the screen-casting section, after the first paragraph:

```markdown
A guest may also simply hand the kiosk a **web address** to open, when the
operator has allowed it — often what someone actually wanted, and far cheaper for
the device than a video stream. See [casting.md](casting.md#a-guest-showing-a-page).
```

- [ ] **Step 3: `docs/troubleshooting.md`**

```markdown
**A guest's page never appears.**
Check the setting first: guest pages are off by default and refused at claim
time, so the guest should have seen a reason rather than nothing. If the address
was accepted and the screen did not change, it was probably a file rather than a
page — downloads are refused browser-wide, so nothing is written and nothing is
shown.

**The screen went back to the playlist while the guest was still there.**
Their page stopped holding its socket. On a phone that usually means the tab was
discarded rather than merely backgrounded; the keepalive itself survives
backgrounding.
```

- [ ] **Step 4: `README.md`**

Add `--guest-pages <on|off>` to the flag list with the note that it defaults to
off, add `mode` to the `POST /api/cast/claim` line, and add `page_enabled` to the
`/api/cast/info` line.

- [ ] **Step 5: `CLAUDE.md`**

In the casting section:

```markdown
### Guest pages

- **The claim carries the mode** (`cast` or `page`), because `register_peer`
  activates the display the moment a sender's socket arrives. A page-mode sender
  must not pin `cast_display.html` on its way to the guest's URL, and
  **`watch_display_arrival` must not run for a page** — there is no display peer
  coming.
- **`authorize_sender` gates on the capability the mode asks for**, not on
  `cast_enabled` alone. The two switches are independent.
- `activate_display`/`deactivate_display` take what they install; the
  still-ours check on teardown compares against `session.showing`, not against
  the cast page.
- Credentials in a guest URL reach the browser and **nothing else**. Everything
  that logs or displays one goes through `guest_page::redact`.
- The grace period is chosen by what is showing: `PAGE_GRACE`, not
  `SENDER_GRACE`.
```

- [ ] **Step 6: Verify the anchors resolve**

```bash
grep -ohE '\]\(([A-Za-z./-]*)#([a-z0-9-]+)\)' CLAUDE.md README.md docs/*.md \
  | sed 's/](//;s/)//' | sort -u
```

Check each target heading exists; `## A guest showing a page` becomes
`#a-guest-showing-a-page`.

- [ ] **Step 7: Commit**

```bash
git add docs README.md CLAUDE.md
git commit -m "Document the guest page

Including the consequence worth stating plainly: with operator authentication
off, a guest can put the admin page on the screen and read the cast code. The
remedy already exists, so this is written down rather than special-cased."
```

---

### Task 9: Device build

**Files:** none changed unless something breaks.

- [ ] **Step 1: Cross build**

Run:

```bash
cross build --release --target armv7-unknown-linux-gnueabihf --target-dir target/cross-armv7
```

Expected: `Finished`. Docker must be running.

- [ ] **Step 2: Check nothing native crept in**

```bash
readelf -d target/cross-armv7/armv7-unknown-linux-gnueabihf/release/miniclientcontrol | grep NEEDED
```

Expected exactly: `libgcc_s.so.1`, `librt.so.1`, `libpthread.so.0`, `libm.so.6`,
`libdl.so.2`, `libc.so.6`, `ld-linux-armhf.so.3`. Anything else means a
dependency pulled in native code and must be replaced.

- [ ] **Step 3: Commit if anything changed**

Only if a fix was needed. Otherwise this task produces no commit.
