# Native cast SP3: external-window override + freeze — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A native cast (SP2's `CastStarted`/`CastEnded` events) installs an **external-window override** on its display: the controller freezes that display's Chromium (`Page.setWebLifecycleState("frozen")`), suspends the freeze *detection* for it, and the standing sway rule (SP4) brings the player window fullscreen in front; on cast end it unfreezes and clears the override.

**Architecture:** `OverrideItem` gains one field, `external_window: bool`. `run_override_loop` grows a **separate freeze path** taken when that flag is set — it freezes the page and parks, instead of navigating; the existing navigate path is untouched (its invariants stay intact). `freeze.rs`'s watcher reads a new per-`Display` `freeze_suspended` flag as `Reading::Nothing` (no evidence), so a deliberately frozen screen is never restarted. `native_cast::{activate,deactivate}` set/clear the external-window override with a "still ours" check; SP2's event-drain calls them.

**Tech Stack:** Rust, tokio, chromiumoxide (`SetWebLifecycleStateParams`, present in `chromiumoxide_cdp` 0.9.0), the `native-cast` feature.

**Working dir:** `/home/philipp/Projects/_fablab/kiosk-pxe/miniclientcontrol`.

## Global Constraints

- **The navigate path of `run_override_loop` must not change behaviour** — the external-window path is a separate branch at the top of the outer loop. Its invariants (CLAUDE.md): `notify_one` never `notify_waiters`; a redundant signal on an *unchanged* override must not re-navigate / re-freeze; teardown restores "only if still ours"; an overlay edit reaches a standing override.
- **`OverrideItem` stays `PartialEq`** — the new field is part of it, so the existing "unchanged override → continue" check keeps working, and the external-window override compares unequal to any URL override.
- **The freeze premise is unproven** (spec risk): `setWebLifecycleState("frozen")` on a foreground fullscreen tab must be *measured* to actually drop CPU/GPU on the target device. This plan's device gate (Task 7) does that; if it does not free resources, the fallback is "no freeze, verdecken only" — one line in the freeze path, no structural change.
- **Feature `native-cast` OFF changes nothing** — all new call sites into `native_cast` are behind the feature; `OverrideItem.external_window` and the freeze path compile unconditionally (they are tiny and harmless with the feature off, since nothing sets the flag) **unless** that pulls `native_cast` symbols — keep the `OverrideItem` field and the `run_override_loop` branch feature-free, and only `activate`/`deactivate` and the event-drain wiring behind `#[cfg(feature = "native-cast")]`.
- No `Co-Authored-By: Claude` / `Claude-Session` trailer.

---

### Task 1: `OverrideItem.external_window` field

**Files:**
- Modify: `src/models.rs` (`OverrideItem`, ~line 329)

**Interfaces:**
- Produces: `OverrideItem { …, external_window: bool }`, `#[serde(default)]`, `false` for every existing construction.

- [ ] **Step 1: Add the field**

In `src/models.rs`, in `OverrideItem` (after the last field), add:
```rust
    /// A native cast (Chromecast/AirPlay): no URL to navigate; the controller
    /// freezes this display's Chromium and the player window is placed in front.
    #[serde(default)]
    pub external_window: bool,
```

- [ ] **Step 2: Default it in every existing constructor**

Build and let the compiler list every `OverrideItem { … }` literal missing the field; add `external_window: false` to each (the WebRTC-cast override in `src/cast/mod.rs`, any test builders). Do **not** use `..Default::default()` unless `OverrideItem` already derives `Default`.

Run: `cargo build`
Expected: compiles once every constructor sets the field.

- [ ] **Step 3: Tests still pass**

Run: `cargo test`
Expected: green — `OverrideItem`'s `PartialEq` now includes the field; a URL override (`external_window: false`) still compares equal to itself (the unchanged-skip) and unequal to an external-window override.

- [ ] **Step 4: Commit**

```bash
git add src/models.rs src/cast/mod.rs
git commit -m "mc: OverrideItem.external_window - native cast override kind"
```

---

### Task 2: `Display.freeze_suspended` + freeze-detection coordination

**Files:**
- Modify: `src/models.rs` (`Display` struct + its constructor), `src/freeze.rs` (`watch`, ~line 128-131)

**Interfaces:**
- Produces: `Display.freeze_suspended: std::sync::atomic::AtomicBool`; `freeze::watch` reads it as `Reading::Nothing`.

- [ ] **Step 1: Add the flag to `Display`**

In `src/models.rs`, in `Display`, add:
```rust
    /// Set while a native-cast override deliberately froze this screen's page, so
    /// the freeze *watcher* treats "not painting" as no evidence, never a fault.
    pub freeze_suspended: std::sync::atomic::AtomicBool,
```
and in `Display`'s constructor, initialise `freeze_suspended: std::sync::atomic::AtomicBool::new(false)`.

- [ ] **Step 2: Honour it in the watcher**

In `src/freeze.rs::watch`, where it reads frames (~line 128-131: `match … { Some(page) => read_frames(page).await, None => Reading::Nothing }`), guard it:
```rust
let reading = if display.freeze_suspended.load(std::sync::atomic::Ordering::Relaxed) {
    Reading::Nothing
} else {
    match display.screen_page.lock().await.as_ref() {
        Some(page) => read_frames(page).await,
        None => Reading::Nothing,
    }
};
```
(keep the existing variable name the surrounding code uses). `Reading::Nothing` is already "no information, never a freeze" (`freeze.rs:54-58`), so a suspended screen is never restarted and, on resume, the first real sample starts its clock fresh.

- [ ] **Step 3: Build**

Run: `cargo build`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add src/models.rs src/freeze.rs
git commit -m "mc: Display.freeze_suspended - freeze detection yields to deliberate freeze"
```

---

### Task 3: CDP freeze/unfreeze helper

**Files:**
- Modify: `src/browser.rs` (a small helper near the other page helpers)

**Interfaces:**
- Produces: `async fn set_lifecycle(page: &Page, frozen: bool) -> Result<(), CdpError>`.

- [ ] **Step 1: Write the helper**

In `src/browser.rs`, add (import the params type at the top: `use chromiumoxide::cdp::browser_protocol::page::{SetWebLifecycleStateParams, SetWebLifecycleStateState};`):
```rust
/// Freeze or thaw a page via the lifecycle API (Chrome's background-tab freeze):
/// frozen stops rAF/timers/rendering and frees CPU/GPU; active restores it with
/// the page state intact. Used for a native cast, where the player window is in
/// front and the browser need not paint.
async fn set_lifecycle(page: &Page, frozen: bool) -> Result<(), chromiumoxide::error::CdpError> {
    let state = if frozen {
        SetWebLifecycleStateState::Frozen
    } else {
        SetWebLifecycleStateState::Active
    };
    page.execute(SetWebLifecycleStateParams { state }).await?;
    Ok(())
}
```

- [ ] **Step 2: Build**

Run: `cargo build`
Expected: compiles (helper unused yet — allowed; Task 4 calls it).

- [ ] **Step 3: Commit**

```bash
git add src/browser.rs
git commit -m "mc: set_lifecycle - CDP page freeze/thaw helper"
```

---

### Task 4: The external-window branch in `run_override_loop`

**Files:**
- Modify: `src/browser.rs` (`run_override_loop`, ~line 857-955)

**Interfaces:**
- Consumes: `OverrideItem.external_window`, `Display.freeze_suspended`, `set_lifecycle`.
- Produces: an override whose `external_window` is set freezes the page and parks, instead of navigating; the navigate path is unchanged.

- [ ] **Step 1: Branch at the top of the outer loop**

In `run_override_loop`, immediately inside `loop {` (before `let target_url = override_target_url(...)`), add the freeze path. It mirrors the navigate path's *control* (unchanged-skip, still-ours, lost→reconnect) but freezes instead of navigating and runs its own small select so the existing inner loop is untouched:
```rust
loop {
    if override_item.external_window {
        // Native cast: freeze this page, let the player window stand in front.
        *display.screen_page.lock().await = Some(page.clone());
        display.freeze_suspended.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Err(e) = set_lifecycle(page, true).await {
            warn!("native-cast: freeze failed on {}: {}", display.name, e);
        }

        // Park until the override changes or clears. Overlay edits are ignored
        // here: the frozen page is hidden behind the player window.
        let next = loop {
            tokio::select! {
                _ = lost.notified() => {
                    display.freeze_suspended.store(false, std::sync::atomic::Ordering::Relaxed);
                    let _ = set_lifecycle(page, false).await; // best effort before reconnect
                    return Err("the CDP connection closed".into());
                }
                _ = display.override_signal.notified() => {}
            }
            let current = { display.override_item.lock().await.clone() };
            match current {
                Some(n) if n == override_item => continue, // unchanged: notify_one permit
                other => break other,
            }
        };

        // Leaving the freeze: thaw and lift the detection suspension.
        display.freeze_suspended.store(false, std::sync::atomic::Ordering::Relaxed);
        if let Err(e) = set_lifecycle(page, false).await {
            warn!("native-cast: thaw failed on {}: {}", display.name, e);
        }
        match next {
            Some(n) => { override_item = n; continue; } // next override (freeze or navigate)
            None => return Ok(()),                       // cleared: back to the playlist
        }
    }

    let target_url = override_target_url(state, &override_item);
    // ... unchanged navigate path ...
```
(The `loop {` and the navigate body below stay exactly as they are.)

- [ ] **Step 2: Build**

Run: `cargo build`
Expected: compiles.

- [ ] **Step 3: The navigate path is byte-for-byte unchanged**

Run: `git diff src/browser.rs` and confirm the only change is the inserted `if override_item.external_window { … }` block; nothing below it moved semantically.
Expected: a clean insert.

- [ ] **Step 4: Tests pass**

Run: `cargo test`
Expected: green — no existing override test regresses (they all use `external_window: false`).

- [ ] **Step 5: Commit**

```bash
git add src/browser.rs
git commit -m "mc: run_override_loop - external-window override freezes the page"
```

---

### Task 5: `native_cast::activate` / `deactivate`

**Files:**
- Modify: `src/native_cast/mod.rs`

**Interfaces:**
- Consumes: `Display.{override_item, override_signal}`, `OverrideItem`.
- Produces: `pub async fn activate(display: &Display)` sets the external-window override; `pub async fn deactivate(display: &Display)` clears it **only if still ours**.

- [ ] **Step 1: Write both**

Append to `src/native_cast/mod.rs`:
```rust
use crate::models::{Display, OverrideItem};

/// Pin the display to the external-window override (native cast started).
pub async fn activate(display: &Display) {
    let item = OverrideItem {
        external_window: true,
        // the rest is irrelevant for an external window; build the "empty" item.
        ..OverrideItem::empty()
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
/// external-window override — an operator who set a different override during the
/// cast made a newer decision.
pub async fn deactivate(display: &Display) {
    {
        let mut lock = display.override_item.lock().await;
        match lock.as_ref() {
            Some(o) if o.external_window => { *lock = None; }
            _ => return, // not ours (or already gone)
        }
    }
    display.override_signal.notify_one();
}
```

- [ ] **Step 2: `OverrideItem::empty()`**

If no all-default constructor exists, add one in `src/models.rs` (next to `OverrideItem`): a `pub fn empty() -> Self` with every field at its zero/None/default and `external_window: false` (the caller sets it true). If `OverrideItem` can `#[derive(Default)]`, derive it and use `OverrideItem { external_window: true, ..Default::default() }` instead — check its field types allow `Default`.

- [ ] **Step 3: Build with the feature**

Run: `cargo build --features native-cast`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add src/native_cast/mod.rs src/models.rs
git commit -m "mc: native_cast activate/deactivate - set/clear external-window override (still-ours)"
```

---

### Task 6: Wire the event-drain to activate/deactivate

**Files:**
- Modify: `src/native_cast/mod.rs` (`supervise`'s drain task)

**Interfaces:**
- Consumes: `activate`/`deactivate`, the per-display `Arc<Display>` from `state.displays`.

- [ ] **Step 1: Resolve the display and call in/out**

In `supervise`, the drain task currently logs. Give it the display's `Arc<Display>` (find it in `state.displays` by `name == screen`, cloned before the loop) and call the override in/out. Because the drain runs on a `spawn_blocking` thread, hop onto the runtime with the captured `Handle` for the async `activate`/`deactivate`:
```rust
// before spawning the drain: resolve this screen's Display
let display = state.displays.iter().find(|d| d.name == screen).cloned();
let handle = tokio::runtime::Handle::current();
let drain_screen = screen.clone();
std::thread::spawn(move || {
    while let Ok(ev) = rx.recv() {
        let Some(display) = display.as_ref() else { continue };
        match ev {
            casttarget::CastEvent::Started { app_id } => {
                tracing::info!("native-cast: start auf {} (app_id={})", drain_screen, app_id);
                handle.block_on(activate(display));
            }
            casttarget::CastEvent::Ended => {
                tracing::info!("native-cast: ende auf {}", drain_screen);
                handle.block_on(deactivate(display));
            }
        }
    }
});
```
(Replace the old `tokio::task::spawn_blocking(move || …)` drain with this `std::thread::spawn`, so `block_on` runs on a non-runtime thread.)

- [ ] **Step 2: Build with the feature**

Run: `cargo build --features native-cast`
Expected: compiles.

- [ ] **Step 3: Commit**

```bash
git add src/native_cast/mod.rs
git commit -m "mc: native-cast events drive the external-window override on/off"
```

---

### Task 7: Gate — SP3 acceptance (incl. the freeze measurement on a device)

**Files:** none (verification).

- [ ] **Step 1: Feature off unchanged**

Run: `cargo build && cargo test`
Expected: green, identical to pre-SP3 (the external-window branch is never entered; nothing sets the flag without the feature).

- [ ] **Step 2: Feature on builds and tests**

Run: `cargo build --features native-cast && cargo test --features native-cast`
Expected: green.

- [ ] **Step 3: Override install/teardown (integration, device or local two-display mc)**

With the feature on and two displays, drive a native cast (a real sender, or casttarget's `--fakesender` against the receiver). Confirm: `CastStarted` installs the external-window override (the display's `override_item` is `Some` with `external_window: true`, the page goes `frozen` via CDP), `CastEnded` thaws and clears it, and the playlist resumes. An operator override set during the cast is **not** clobbered on cast end.

- [ ] **Step 4: Freeze detection does not restart a frozen screen**

While a cast holds the freeze, confirm `freeze.rs` does **not** fire `display.frozen`/restart (it reads `freeze_suspended` as `Nothing`). Let it stand several minutes (past the freeze timeout).

- [ ] **Step 5: THE FREEZE MEASUREMENT (the unproven premise)**

On the target kiosk hardware, measure CPU/GPU of the display's Chromium before and during a cast:
```bash
# before cast: note the chromium render process CPU%
top -b -n1 | grep chromium
# during cast (page frozen): measure again
```
Expected: the frozen Chromium's CPU drops materially (and ideally GPU). **If it does not** (Wayland may give no occlusion signal, so only timers stop): record it, and fall back — in Task 4's freeze path, keep the override + `freeze_suspended` (so detection still yields) but skip the `set_lifecycle(page, true/false)` calls ("verdecken only"). No structural change; the player window still stands in front.

- [ ] **Step 6: Cross-compile under the feature**

Run: `cross build --features native-cast --target armv7-unknown-linux-gnueabihf`
Expected: compiles (SP2 already proved the link; this confirms SP3 added nothing that breaks it).

- [ ] **Step 7: Push**

```bash
git-wrapper.sh -C /home/philipp/Projects/_fablab/kiosk-pxe/miniclientcontrol push origin master
```

## Deferred to SP4

- The **sway placement rule** for `casttarget-<display>` (fullscreen, in front) in kiosk-pxe's `kiosk-session`.
- **cast.db on the USB data partition** (the `--cast-db` default already points beside `miniclient.db`; SP4 confirms the kiosk-session passes the data dir).
- The **editable `label`** as the mDNS friendly name (SP2 uses `cfg.name`).
- Holding `ReceiverHandle`s in state for clean teardown.
