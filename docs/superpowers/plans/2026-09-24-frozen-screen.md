# Frozen Screen and Screenshot Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Notice a screen that stopped being painted (restart its browser once, report it) and show the operator what each screen shows.

**Architecture:** The overlay runtime counts `requestAnimationFrame` ticks. `browser.rs` publishes which page is on screen on its `Display`; a per-screen watcher task (`src/freeze.rs`) samples that page's counter every 5 s through a pure `Watch` state machine, fires webhooks and asks `chromium::supervise` to kill the child it launched. A screenshot endpoint captures the same published page on request, cached 10 s.

**Tech Stack:** Rust (axum 0.8, chromiumoxide 0.9, tokio), vanilla JS, stdlib Python tests with headless Chrome.

Spec: `docs/superpowers/specs/2026-09-24-frozen-screen-design.md`.

## Global Constraints

- Heartbeat: `globalThis.__ovFrames`, top frame only, started once per document; `__ov.state().frames`.
- Sample every 5 s; frozen after `--freeze-timeout` seconds without progress (default 60).
- A counter that goes *down* is a new document: progress, not a stall. No counter (runtime missing, evaluate failed, no page) is no information: never a freeze.
- Restart only a browser the controller launched (`Display::browser_pid` is `Some`). Brake: no second restart within 30 minutes of the last.
- Webhooks: `display.frozen` `{ seconds, restarted }`, `display.recovered` `{ seconds }`.
- Screenshot: `GET /api/displays/{name}/screenshot`, JPEG quality 60, at most 640 px wide, cached 10 s, capture timeout 5 s, headers `X-Screenshot-Age`, `X-Screen-Frozen-Since` (RFC 3339, only when frozen), `Cache-Control: no-store`; `503` with no picture. In neither exemption list (needs an account like any operator read).
- `notify_one`, never `notify_waiters`. No lock held across a CDP call.
- UI: `createElement`/`textContent`; images fetched only while `document.visibilityState === 'visible'`, every 10 s. New file under `web/` → `touch src/web.rs`.
- Commits: no Claude co-author or session trailers (the user's global rule).

---

### Task 1: The heartbeat and the pure watcher

**Files:**
- Create: `src/freeze.rs`
- Modify: `web/overlay.js`, `src/main.rs` (`mod freeze;`), `src/models.rs` (`Args::freeze_timeout`)

**Interfaces:**
- Produces:
  - `pub struct Watch` (`Default`), `pub enum Event { None, Frozen { seconds: u64, restart: bool }, Recovered { seconds: u64 } }` (`Debug, PartialEq`)
  - `impl Watch { pub fn observe(&mut self, now: Instant, frames: Option<u64>, timeout: Duration, can_restart: bool) -> Event; pub fn frozen_for(&self, now: Instant) -> Option<Duration> }`
  - `pub const SAMPLE: Duration` (5 s), `pub const BRAKE: Duration` (30 min)
  - `Args::freeze_timeout: u64` (`--freeze-timeout`, default 60)

- [ ] **Step 1: The counter in `web/overlay.js`**

Directly before `globalThis.__ov = {`:

```js
  // A heartbeat: how many frames the compositor has let this page paint. It is
  // what tells a frozen screen from a still one -- a dashboard showing the same
  // numbers for an hour still ticks every frame, while a compositor that hangs
  // stops it dead even though the page's JavaScript and CDP keep answering
  // (measured on kiosk2 with Xorg stopped: 4 frames in 5 s, evaluate fine).
  // Top frame only, once per document: every seeded registration runs this.
  if (window.top === window && !globalThis.__ovFramesLoop) {
    globalThis.__ovFramesLoop = true;
    globalThis.__ovFrames = 0;
    const tick = () => {
      globalThis.__ovFrames += 1;
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  }
```

and in `state()` add `frames: typeof globalThis.__ovFrames === 'number' ? globalThis.__ovFrames : null,`.

- [ ] **Step 2: `src/freeze.rs` — the pure part, with tests**

```rust
//! A screen that stopped being painted. CDP answers normally while the
//! compositor hangs -- measured on kiosk2 and seen for thirteen hours on a Pi --
//! so the signal is the overlay runtime's frame counter, sampled from here.

use std::time::{Duration, Instant};

pub const SAMPLE: Duration = Duration::from_secs(5);
/// No second restart within this long of the last one: a panel switched off,
/// or a GPU that is gone, must not become a restart loop.
pub const BRAKE: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    None,
    Frozen { seconds: u64, restart: bool },
    Recovered { seconds: u64 },
}

#[derive(Default)]
pub struct Watch {
    /// The last sample that showed progress, and its counter.
    last: Option<(Instant, u64)>,
    frozen: bool,
    last_restart: Option<Instant>,
}

impl Watch {
    /// One sample. `frames` is `None` when there is nothing to read -- no page,
    /// no runtime, a navigation under the evaluate -- which is no information,
    /// never evidence of a freeze.
    pub fn observe(&mut self, now: Instant, frames: Option<u64>, timeout: Duration, can_restart: bool) -> Event {
        let Some(frames) = frames else { return Event::None };
        match self.last {
            // First sample, a new document (the counter starts over), or progress.
            None => {
                self.last = Some((now, frames));
                Event::None
            }
            Some((_, seen)) if frames != seen => {
                let since = self.last.map(|(at, _)| at).unwrap_or(now);
                self.last = Some((now, frames));
                if std::mem::take(&mut self.frozen) {
                    Event::Recovered { seconds: now.duration_since(since).as_secs() }
                } else {
                    Event::None
                }
            }
            Some((since, _)) => {
                let stalled = now.duration_since(since);
                if self.frozen || stalled < timeout {
                    return Event::None;
                }
                self.frozen = true;
                let restart = can_restart
                    && self.last_restart.is_none_or(|at| now.duration_since(at) >= BRAKE);
                if restart {
                    self.last_restart = Some(now);
                }
                Event::Frozen { seconds: stalled.as_secs(), restart }
            }
        }
    }

    /// How long the screen has been frozen, for the operator pages.
    pub fn frozen_for(&self, now: Instant) -> Option<Duration> {
        self.frozen.then(|| self.last.map(|(at, _)| now.duration_since(at)).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(60);

    fn at(start: Instant, secs: u64) -> Instant {
        start + Duration::from_secs(secs)
    }

    #[test]
    fn a_ticking_counter_is_fine() {
        let (mut w, s) = (Watch::default(), Instant::now());
        for (i, n) in [(0, 10), (5, 300), (10, 600), (70, 4000)] {
            assert_eq!(w.observe(at(s, i), Some(n), T, true), Event::None);
        }
    }

    #[test]
    fn a_stalled_counter_freezes_once_then_recovers() {
        let (mut w, s) = (Watch::default(), Instant::now());
        w.observe(at(s, 0), Some(100), T, true);
        assert_eq!(w.observe(at(s, 30), Some(100), T, true), Event::None);
        assert_eq!(w.observe(at(s, 60), Some(100), T, true), Event::Frozen { seconds: 60, restart: true });
        assert_eq!(w.observe(at(s, 65), Some(100), T, true), Event::None, "reported once");
        assert!(w.frozen_for(at(s, 65)).is_some());
        assert_eq!(w.observe(at(s, 90), Some(5), T, true), Event::Recovered { seconds: 90 });
        assert!(w.frozen_for(at(s, 90)).is_none());
    }

    #[test]
    fn the_brake_stops_a_second_restart() {
        let (mut w, s) = (Watch::default(), Instant::now());
        w.observe(at(s, 0), Some(1), T, true);
        assert_eq!(w.observe(at(s, 60), Some(1), T, true), Event::Frozen { seconds: 60, restart: true });
        w.observe(at(s, 70), Some(2), T, true);
        assert_eq!(w.observe(at(s, 130), Some(2), T, true), Event::Frozen { seconds: 60, restart: false });
        w.observe(at(s, 140), Some(3), T, true);
        let later = 140 + BRAKE.as_secs();
        w.observe(at(s, later), Some(4), T, true);
        assert_eq!(w.observe(at(s, later + 60), Some(4), T, true), Event::Frozen { seconds: 60, restart: true });
    }

    #[test]
    fn a_browser_not_ours_is_never_restarted() {
        let (mut w, s) = (Watch::default(), Instant::now());
        w.observe(at(s, 0), Some(1), T, false);
        assert_eq!(w.observe(at(s, 60), Some(1), T, false), Event::Frozen { seconds: 60, restart: false });
    }

    #[test]
    fn no_reading_is_no_evidence() {
        let (mut w, s) = (Watch::default(), Instant::now());
        w.observe(at(s, 0), Some(1), T, true);
        for i in [30, 60, 90, 300] {
            assert_eq!(w.observe(at(s, i), None, T, true), Event::None);
        }
    }
}
```

`src/main.rs`: `mod freeze;`. `src/models.rs`, in `Args` next to `advance_stall_timeout`:

```rust
    /// How long a screen may go without painting a frame before it counts as
    /// frozen: its browser is restarted once and `display.frozen` fires.
    #[arg(long, env, default_value_t = 60)]
    pub freeze_timeout: u64,
```

- [ ] **Step 3: Run and commit**

Run: `cargo test freeze` — 5 pass. `touch src/web.rs && cargo build`.

```bash
git add src/freeze.rs src/main.rs src/models.rs web/overlay.js
git commit -m "Count painted frames in the overlay runtime and decide when a screen is frozen"
```

---

### Task 2: The page on screen, the watcher task, the restart

**Files:**
- Modify: `src/models.rs` (`Display` fields), `src/browser.rs` (publish the page), `src/freeze.rs` (the task), `src/chromium.rs` (`supervise` takes a restart signal), `src/main.rs` (spawn), `src/webhook/mod.rs` + `src/webhook/api.rs` (events)
- Test: `tests/cast/test_freeze.py` (create)

**Interfaces:**
- Produces:
  - `Display::screen_page: Mutex<Option<chromiumoxide::Page>>`, `Display::frozen_since: Mutex<Option<chrono::DateTime<chrono::Utc>>>`, `Display::browser_restart: Arc<Notify>`
  - `pub async fn watch(state: AppState, display: Arc<Display>)` in `freeze.rs`
  - `pub async fn supervise(args, display, pid_slot, restart: Arc<Notify>)`
  - `Event::DisplayFrozen { seconds: u64, restarted: bool }`, `Event::DisplayRecovered { seconds: u64 }`

- [ ] **Step 1: Failing test**

`tests/cast/test_freeze.py`, built on `test_webhook.py`'s `Receiver` and `Display` harness (a controller with `--cdp-url` pointing at a headless Chrome the test starts, so the controller did *not* launch it):

```python
"""A frozen screen is noticed: the page's frame counter stops, the controller
says so, and says so again when it moves.

The Chrome here is the test's, not the controller's, so the controller must
report without restarting it -- the brake for a browser it did not start.
"""
import asyncio, json, os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cdp
from test_cast import check, failures, http
from test_webhook import Receiver, Display, add_hook, events_of, until_async, a_playlist, add_item


STOP = """(() => {
  // Replace the rAF loop with nothing: the counter stops, exactly as when the
  // compositor stops handing out frames. JavaScript keeps answering.
  globalThis.__ovFramesLoop = true;
  const frozen = globalThis.__ovFrames;
  Object.defineProperty(globalThis, '__ovFrames', { get: () => frozen, set: () => {}, configurable: true });
  return frozen;
})()"""

START = """(() => {
  delete globalThis.__ovFrames;
  globalThis.__ovFrames = 1;
  const tick = () => { globalThis.__ovFrames += 1; requestAnimationFrame(tick); };
  requestAnimationFrame(tick);
  return true;
})()"""


async def case_freeze():
    print("\n[170] a stalled counter is reported, and its recovery")
    with Receiver() as receiver, Display(freeze_timeout=10) as display:
        add_hook(receiver.url, ["display.frozen", "display.recovered"])
        add_item(url="about:blank#freeze", duration=600)
        ws_url, _ = cdp.page_ws(display.cdp_port)
        async with cdp.Session(ws_url) as page:
            ready = await until_async(lambda: None, timeout=1)  # let the loop settle
            for _ in range(60):
                try:
                    if await page.eval("typeof globalThis.__ovFrames === 'number' && globalThis.__ovFrames > 0"):
                        break
                except Exception:
                    pass
                await asyncio.sleep(0.5)
            await page.eval(STOP)
            check("display.frozen arrives", await until_async(
                lambda: "display.frozen" in events_of(receiver), timeout=40), events_of(receiver))
            frozen = next(b for b in receiver.bodies() if b["event"] == "display.frozen")
            check("not restarted: the controller did not start this browser",
                  frozen["data"]["restarted"] is False and frozen["data"]["seconds"] >= 10, frozen)
            listed = next(d for d in http("GET", "/api/displays")[1] if d["name"] == display.name)
            check("the operator list says since when", listed.get("frozen_since"), listed)
            await page.eval(START)
            check("display.recovered follows", await until_async(
                lambda: "display.recovered" in events_of(receiver), timeout=20), events_of(receiver))


if __name__ == "__main__":
    asyncio.run(case_freeze())
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
```

(Read `test_webhook.py`'s `Display`, `add_item`, `until_async` first and match their real signatures — the names above are theirs; if `Display` takes flags differently, pass `freeze_timeout=10` the way it passes other flags. `add_item` builds `advance` already. The `about:blank#freeze` page gets the overlay runtime like any page; if the runtime is not installed on `about:blank`, use a data page the harness already serves.)

Run: expected FAIL (no `display.frozen`).

- [ ] **Step 2: `Display` fields**

In `models.rs` `Display`:

```rust
    /// The page this screen shows right now, published by `browser_loop` for
    /// the freeze watcher and the screenshot endpoint. Cloned out, never held
    /// across a CDP call.
    pub screen_page: Mutex<Option<chromiumoxide::Page>>,
    /// Set while the heartbeat says the screen is frozen.
    pub frozen_since: Mutex<Option<chrono::DateTime<chrono::Utc>>>,
    /// Asks `chromium::supervise` to kill the browser it launched, which it
    /// then starts again.
    pub browser_restart: Arc<Notify>,
    /// The last screenshot and when it was taken.
    pub screenshot: Mutex<Option<(std::time::Instant, Vec<u8>)>>,
```

initialised in `Display::new` (`Mutex::new(None)` / `Arc::new(Notify::new())`).

- [ ] **Step 3: Publish the page in `browser.rs`**

Set `*display.screen_page.lock().await = Some(page.clone());` at each place a page becomes the one on screen:
- in the item loop, right after `active_page.bring_to_front()` succeeds (the `keep_loaded` tab or `dynamic_page`);
- in the idle branch, where it navigates or keeps `empty_playlist.html` on `dynamic_page`;
- in `run_override_loop`, for the page it navigates;
and `*display.screen_page.lock().await = None;` where the outer loop starts a reconnect (`reconnect_needed` path, before `sleep(2 s)`).

- [ ] **Step 4: The watcher task in `freeze.rs`**

```rust
use std::sync::Arc;

use crate::models::{AppState, Display};

/// Sample the page on screen every `SAMPLE` for as long as the process runs.
pub async fn watch(state: AppState, display: Arc<Display>) {
    let timeout = Duration::from_secs(state.args.freeze_timeout.max(10));
    let mut watch = Watch::default();
    let mut target: Option<String> = None;
    loop {
        tokio::time::sleep(SAMPLE).await;
        let page = display.screen_page.lock().await.clone();
        // Another page on screen (a keep_loaded tab, a reconnect) starts over.
        let id = page.as_ref().map(|p| p.target_id().inner().clone());
        if id != target {
            watch = Watch { last_restart: watch.last_restart, ..Watch::default() };
            target = id;
        }
        let frames = match &page {
            Some(page) => read_frames(page).await,
            None => None,
        };
        let can_restart = display.browser_pid.lock().await.is_some();
        let name = display.name.clone();
        match watch.observe(Instant::now(), frames, timeout, can_restart) {
            Event::None => {}
            Event::Frozen { seconds, restart } => {
                tracing::warn!(
                    "Display {} has painted nothing for {} s{}",
                    name, seconds, if restart { ", restarting its browser" } else { "" }
                );
                *display.frozen_since.lock().await =
                    Some(chrono::Utc::now() - chrono::Duration::seconds(seconds as i64));
                state.webhooks.fire(&name, crate::webhook::Event::DisplayFrozen { seconds, restarted: restart });
                if restart {
                    display.browser_restart.notify_one();
                }
            }
            Event::Recovered { seconds } => {
                tracing::info!("Display {} is painting again after {} s", name, seconds);
                *display.frozen_since.lock().await = None;
                state.webhooks.fire(&name, crate::webhook::Event::DisplayRecovered { seconds });
            }
        }
    }
}

/// The overlay runtime's counter, or `None` when there is nothing to read.
async fn read_frames(page: &chromiumoxide::Page) -> Option<u64> {
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        page.evaluate("(() => typeof globalThis.__ovFrames === 'number' ? globalThis.__ovFrames : null)()"),
    )
    .await
    .ok()?
    .ok()?;
    result.into_value::<Option<u64>>().ok().flatten()
}
```

(`last_restart` must be visible to this function: it is a private field in the same module, so the struct-update above compiles.)

`main.rs`, in the per-display loop that spawns `browser_loop`:

```rust
        let watcher_state = state.clone();
        let watched = display.clone();
        tokio::spawn(async move { freeze::watch(watcher_state, watched).await });
```

- [ ] **Step 5: `supervise` restarts on request**

`chromium::supervise` gains `restart: std::sync::Arc<tokio::sync::Notify>`; replace the trailing `tokio::time::sleep(Duration::from_secs(3)).await;` with:

```rust
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(3)) => {}
            _ = restart.notified() => {
                // Only ever our own child: a browser found already running on
                // the port is somebody else's to restart.
                if let Some(running) = child.as_mut() {
                    warn!("Restarting the browser for display '{}': the screen stopped painting", display_name);
                    let _ = running.start_kill();
                }
            }
        }
```

The next pass reaps the exited child and spawns a new one; the control loop sees its CDP connection drop and reconnects as it always has. `main.rs` passes `display.browser_restart.clone()`.

- [ ] **Step 6: Webhooks**

`webhook/mod.rs`: variants `DisplayFrozen { seconds: u64, restarted: bool }` → name `display.frozen`, data `{ "seconds", "restarted" }`; `DisplayRecovered { seconds: u64 }` → `display.recovered`, data `{ "seconds" }`. `webhook/api.rs::catalogue()`: both entries ("Ein Bildschirm zeichnet nichts mehr." / "Ein Bildschirm zeichnet wieder."), and samples in `sample()`.

- [ ] **Step 7: `frozen_since` in `GET /api/displays`**

In `display.rs::list`, per declared entry: `"frozen_since": state.display(&name) → display.frozen_since.lock().await.map(|t| t.to_rfc3339())`.

- [ ] **Step 8: Run and commit**

Run: `cargo test && cargo build && python3 tests/cast/test_freeze.py && python3 tests/cast/test_webhook.py`

```bash
git add src tests/cast/test_freeze.py
git commit -m "Watch every screen's heartbeat, report a freeze and restart our own browser once"
```

---

### Task 3: A restart of a browser the controller launched

**Files:**
- Test: `tests/cast/test_freeze.py`

- [ ] **Step 1: The case**

A controller **without** `--no-launch-browser`, launching headless Chrome itself: `--chromium-path /usr/bin/google-chrome-stable --chromium-arg=--headless=new --chromium-arg=--no-sandbox --chromium-arg=--disable-gpu`, a free CDP port through `--cdp-url` (the default display), `--freeze-timeout 10`. (Check the flag names in `models::Args`; `test_browser.py` shows how a launched Chrome is configured if it does so.) Freeze its page with `STOP`, then:

- `display.frozen` arrives with `restarted: true`;
- the browser process changes: the CDP `/json/version` `webSocketDebuggerUrl` differs after the restart (a new browser session id), within 30 s;
- `display.recovered` follows once the new page paints;
- freezing again within the test (brake window) gives `restarted: false`.

Kill the launched Chrome at the end of the case (the controller deliberately leaves it running when it stops): find it by its `--user-data-dir` from `chromium::user_data_dir` (`/tmp/miniclientcontrol-chromium-<port>`).

- [ ] **Step 2: Run and commit**

Run: `python3 tests/cast/test_freeze.py`

```bash
git add tests/cast/test_freeze.py
git commit -m "Test that a frozen screen's own browser is restarted once"
```

---

### Task 4: The screenshot endpoint

**Files:**
- Create: `src/screenshot.rs`
- Modify: `src/display.rs` (route), `src/main.rs` (`mod screenshot;`)
- Test: `tests/cast/test_freeze.py`

**Interfaces:**
- Produces: `GET /api/displays/{name}/screenshot`; `pub async fn capture(display: &Display) -> Option<(Vec<u8>, Duration)>` (bytes and age)

- [ ] **Step 1: Failing test**

```python
async def case_screenshot():
    print("\n[172] what is on screen, as a picture")
    with Display() as display:
        add_item(url="about:blank#shot", duration=600)
        name = display.name
        status, body = None, None
        for _ in range(40):
            req = urllib.request.Request(f"http://127.0.0.1:{HTTP}/api/displays/{name}/screenshot")
            try:
                with urllib.request.urlopen(req, timeout=10) as res:
                    status, body, headers = res.status, res.read(), res.headers
                    break
            except urllib.error.HTTPError as e:
                status = e.code
            await asyncio.sleep(0.5)
        check("a JPEG comes back", status == 200 and body[:3] == b"\xff\xd8\xff", status)
        check("never cached by the browser", headers.get("Cache-Control") == "no-store", dict(headers))
        with urllib.request.urlopen(req, timeout=10) as res:
            again = res.read()
        check("a second request within 10 s is the cached picture", again == body, None)
        status, _ = http("GET", "/api/displays/nope/screenshot")
        check("an unknown screen is a 404", status == 404, status)
```

and an auth case using `test_users.basic`/`Browser`: once an account exists, the endpoint is `401` without credentials. Add `import urllib.request, urllib.error` and `from test_cast import HTTP`.

- [ ] **Step 2: `screenshot.rs`**

```rust
//! What a screen shows, as a small JPEG: the page on screen with its overlay,
//! override or guest page -- which is why it is not the asset's own file.
//! Taken on request only and cached, because on a Pi every capture costs.

use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use chromiumoxide::cdp::browser_protocol::page::{CaptureScreenshotFormat, CaptureScreenshotParams, Viewport};

use crate::models::{AppState, Display};

const FRESH: Duration = Duration::from_secs(10);
/// A capture hangs on a frozen screen (measured); the last picture is the answer then.
const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_WIDTH: f64 = 640.0;

async fn take(page: &chromiumoxide::Page) -> Option<Vec<u8>> {
    let size: Vec<f64> = page
        .evaluate("[innerWidth, innerHeight]")
        .await
        .ok()?
        .into_value()
        .ok()?;
    let (width, height) = (*size.first()?, *size.get(1)?);
    let scale = (MAX_WIDTH / width).min(1.0);
    let params = CaptureScreenshotParams::builder()
        .format(CaptureScreenshotFormat::Jpeg)
        .quality(60)
        .clip(Viewport { x: 0.0, y: 0.0, width, height, scale })
        .build();
    page.screenshot(params).await.ok()
}

/// A fresh picture if one can be had within `TIMEOUT`, else the last one.
pub async fn capture(display: &Display) -> Option<(Vec<u8>, Duration)> {
    if let Some((at, bytes)) = display.screenshot.lock().await.as_ref() {
        if at.elapsed() < FRESH {
            return Some((bytes.clone(), at.elapsed()));
        }
    }
    let page = display.screen_page.lock().await.clone();
    if let Some(page) = page {
        if let Ok(Some(bytes)) = tokio::time::timeout(TIMEOUT, take(&page)).await {
            *display.screenshot.lock().await = Some((Instant::now(), bytes.clone()));
            return Some((bytes, Duration::ZERO));
        }
    }
    display.screenshot.lock().await.as_ref().map(|(at, bytes)| (bytes.clone(), at.elapsed()))
}

pub async fn handler(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let Some(display) = state.display(&name) else {
        return (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({ "error": "Unbekannter Bildschirm." })))
            .into_response();
    };
    let Some((bytes, age)) = capture(&display).await else {
        return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(serde_json::json!({ "error": "Noch kein Bild." })))
            .into_response();
    };
    let mut response = (
        [
            (header::CONTENT_TYPE, "image/jpeg".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        bytes,
    )
        .into_response();
    let headers = response.headers_mut();
    if let Ok(value) = age.as_secs().to_string().parse() {
        headers.insert("x-screenshot-age", value);
    }
    if let Some(since) = *display.frozen_since.lock().await {
        if let Ok(value) = since.to_rfc3339().parse() {
            headers.insert("x-screen-frozen-since", value);
        }
    }
    response
}
```

(If chromiumoxide's `Page::screenshot` takes `ScreenshotParams` rather than `CaptureScreenshotParams`, use `page.execute(params)` and decode `data` from base64 with the `base64` crate instead — check `chromiumoxide::Page` in the version pinned in `Cargo.lock`.)

`display.rs::routes()`: `.route("/api/displays/{name}/screenshot", get(crate::screenshot::handler))`. It is a `GET` under `/api/displays`, which the role table already treats as `Read`; add a matrix row asserting `GET /api/displays/foyer/screenshot` → `Read`.

- [ ] **Step 3: Run and commit**

Run: `cargo test roles && cargo build && python3 tests/cast/test_freeze.py`

```bash
git add src tests/cast/test_freeze.py
git commit -m "Show what a screen shows: a small cached screenshot on request"
```

---

### Task 5: The pictures on the operator pages

**Files:**
- Create: `web/screenshot.js`
- Modify: `web/admin.html`, `web/displays.html`, `web/playlist.html`, `src/web.rs` (touch)

- [ ] **Step 1: `web/screenshot.js`**

```js
// What a screen shows, on the operator pages: one small picture per screen,
// refreshed every ten seconds while the page is visible. One node per screen,
// reused, so a page that rebuilds its status line every poll does not reload
// the picture every poll.
(() => {
  'use strict';

  const nodes = new Map();
  const REFRESH = 10000;

  async function refresh(name, entry) {
    if (document.visibilityState !== 'visible') return;
    try {
      const res = await fetch(`/api/displays/${encodeURIComponent(name)}/screenshot`, { cache: 'no-store' });
      if (!res.ok) { entry.box.hidden = true; return; }
      const blob = await res.blob();
      if (entry.url) URL.revokeObjectURL(entry.url);
      entry.url = URL.createObjectURL(blob);
      entry.img.src = entry.url;
      entry.box.hidden = false;
      const since = res.headers.get('x-screen-frozen-since');
      const age = Number(res.headers.get('x-screenshot-age') || 0);
      entry.mark.hidden = !since;
      if (since) {
        entry.mark.textContent = `eingefroren seit ${new Date(since).toLocaleTimeString()}`;
      }
      entry.box.title = age > 15 ? `Bild ${age} s alt` : 'aktuell';
    } catch (_) {
      entry.box.hidden = true;
    }
  }

  // The picture for one screen: the same element every time, so it can be
  // appended into a freshly built row without starting over.
  function node(name) {
    let entry = nodes.get(name);
    if (!entry) {
      const box = document.createElement('span');
      box.className = 'screenshot';
      box.hidden = true;
      box.style.cssText = 'display:inline-block; position:relative; vertical-align:middle; margin:0 .5rem;';
      const img = document.createElement('img');
      img.alt = '';
      img.style.cssText = 'width:160px; border:1px solid #ccc; border-radius:3px; display:block;';
      const mark = document.createElement('span');
      mark.hidden = true;
      mark.style.cssText = 'position:absolute; left:0; right:0; bottom:0; background:#a11; color:#fff;'
        + 'font:12px system-ui, sans-serif; padding:1px 4px; text-align:center;';
      box.append(img, mark);
      entry = { box, img, mark, url: null };
      nodes.set(name, entry);
      refresh(name, entry);
      setInterval(() => refresh(name, entry), REFRESH);
    }
    return entry.box;
  }

  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') for (const [name, entry] of nodes) refresh(name, entry);
  });

  globalThis.Screenshot = { node };
})();
```

(`hidden` loses to the inline `display:inline-block`: set `box.style.display = entry.box.hidden ? 'none' : 'inline-block'` wherever `hidden` changes, or give `.screenshot[hidden]{display:none}` in each page's CSS — pick the CSS rule and add it to the three pages.)

- [ ] **Step 2: Use it**

- `admin.html`: load `/screenshot.js`; in the status line builder (`for (const line of lines)`), append `Screenshot.node(line.display.name)` to each row.
- `displays.html`: load it; in `buildCard`, for a declared display, append `Screenshot.node(display.name)` to the card's head grid.
- `playlist.html`: load it; in `refreshCards` (or where the `▶ läuft` badge is placed on the card whose id is `currentItemId`), append `Screenshot.node(currentScreen)` next to that badge; remove it from any other card (appending the same node moves it).

- [ ] **Step 3: Check in a browser, then commit**

Run: `touch src/web.rs && cargo build`; headless: a scratch server with a display harness, open each page, assert an `<img>` inside `.screenshot` gets a `blob:` src within 12 s and the node is the same element after a status poll (identity check via a marker property).

```bash
git add web src/web.rs
git commit -m "Show each screen's picture on the admin, displays and playlist pages"
```

---

### Task 6: On kiosk2, and documentation

- [ ] **Step 1: The real thing** — deploy to kiosk2 following the memory's routine (backup, copy binary, `setcap`, restart, check both screens come up). Then, with the user's agreement already given: stop Xorg for 90 s with a detached `SIGCONT` (`setsid sh -c 'kill -STOP <pid>; sleep 90; kill -CONT <pid>'`), and confirm from the journal that `display.frozen` fired (at the default 60 s timeout) with `restarted: true` for one screen, that the browser was restarted, and that both screens come back after `SIGCONT`. Then `GET /api/displays/left/screenshot` returns a picture. (Both screens share one Xorg: both freeze; both restart. The brake then holds for 30 min.)

- [ ] **Step 2: Docs** — `CLAUDE.md`: replace "nothing here detects a frozen screen" in the Webhooks section with the heartbeat rules (counter in the overlay runtime, top frame, no reading is no evidence, restart only our own child, brake, the measurement, what it cannot see: a panel off, a cable out); the screenshot endpoint (on request, cached, times out on a frozen screen). README: the endpoint and `--freeze-timeout`; `docs/features.md`: "Was läuft jetzt" and freeze detection for the operator; webhook table: the two events; `docs/troubleshooting.md`: "display.frozen fired" — what it means, what the brake does. Roadmap: remove the entry. Spec: `Status: implemented`.

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md README.md docs
git commit -m "Document freeze detection and the screen pictures"
```

## Final verification

- [ ] `cargo test`; every suite in `tests/cast/` one at a time (local instance stopped).
