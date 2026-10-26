# Native cast SP1: casttarget library extraction — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn `casttarget` from a binary into a library crate (`casttarget` lib) plus a thin binary, with a `start(ReceiverConfig, access, events)` entry point, so `miniclientcontrol` can depend on it in SP2 — without regressing the standalone binary.

**Architecture:** The protocol stack (`cast/`, `certstore`) and the state types (`AppState`/`SessionInfo`/`StreamStats`) move into `src/lib.rs`. A new `start()` wraps today's `main()` body (db resolution, identity load, state build, refetch thread, `Service::start`) and takes an injected `device_id` and `app_id`, an `AccessHook`, and a `CastEvent` sender. The binary (`src/main.rs`) becomes: parse args → build `ReceiverConfig` → `start()` with an allow-all hook and a logging event sink → run the UI. The `winit`/`softbuffer`/`fontdue`/`ureq` UI deps move behind a default-on `ui` feature so the library builds without them.

**Tech Stack:** Rust 2021, `rusqlite` (bundled), `rustls` (ring), no tokio (sync/threads), `winit`/`softbuffer`/`fontdue` (UI, feature-gated).

**Working dir:** `/home/philipp/Projects/_fablab/casttarget` (its own git repo, remote `git@git.fablab-luebeck.de:it/casttarget.git`). Remote git goes through `git-wrapper.sh` (YubiKey); local commits do not.

## Global Constraints

- **Minimal-invasive:** reuse the existing protocol code verbatim; this is a structural extraction, not a rewrite.
- **Standalone binary must keep working** — `scripts/ui-test.sh` green, same CLI (`--db`/`--name`/`--port`/`--fetch`/`--no-ui`/`--fakesender`).
- **The library must build with no UI deps** (`cargo build --lib --no-default-features`).
- **`app_id` is accepted and stored in SP1 but NOT applied to the players** — applying it (and the ffplay→mpv decision) is SP4.
- **`device_id` becomes injectable**, defaulting to today's derived value when not given.
- **rusqlite pin-awareness:** do not bump `rusqlite`/`libsqlite3-sys`; SP2 links it beside mc's `sqlx` and both must stay on `libsqlite3-sys 0.30.x`.
- Commit messages carry **no** `Co-Authored-By: Claude` / `Claude-Session` trailer.

---

### Task 1: Cargo manifest — `[lib]` + `[[bin]]`, UI deps behind a feature

**Files:**
- Modify: `Cargo.toml`

**Interfaces:**
- Produces: a crate that builds both as a library (`--no-default-features`, no UI) and as the binary (default features, with UI). Later tasks add modules to the lib.

- [ ] **Step 1: Add explicit lib and bin targets, and a `ui` feature**

Edit `Cargo.toml`. After `[package]` add targets and a feature; make the UI deps optional:

```toml
[lib]
name = "casttarget"
path = "src/lib.rs"

[[bin]]
name = "casttarget"
path = "src/main.rs"

[features]
default = ["ui"]
ui = ["dep:winit", "dep:softbuffer", "dep:fontdue"]
```

In `[dependencies]`, mark the UI crates optional (leave every other dependency untouched — `rusqlite`, `rustls`, etc. stay exactly as they are):

```toml
winit = { version = "0.30", optional = true }
softbuffer = { version = "0.4", optional = true }
fontdue = { version = "0.9", optional = true }
```

- [ ] **Step 2: A placeholder lib so the manifest resolves**

`src/lib.rs` does not exist yet; create a one-line stub so `cargo` can resolve the `[lib]` target (Task 2 fills it):

```rust
//! casttarget library — native cast receiver. Filled in by SP1.
```

- [ ] **Step 3: Both build shapes compile**

Run: `cargo build --no-default-features --lib`
Expected: compiles — proves the lib target has no UI deps yet (it is still a stub).

Run: `cargo build`
Expected: compiles with the binary and UI (default feature `ui`).

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml src/lib.rs
git commit -m "casttarget: lib+bin targets, UI deps behind default 'ui' feature"
```

---

### Task 2: Move the state types and helpers into the library

**Files:**
- Modify: `src/lib.rs` (fill the stub), `src/main.rs`

**Interfaces:**
- Consumes: `cast`, `certstore` modules (exist).
- Produces: `casttarget::{AppState, SessionInfo, StreamStats}` and the `cast`/`certstore` modules, reachable from the binary as `casttarget::…`.

- [ ] **Step 1: Make `lib.rs` own the modules and state types**

Replace `src/lib.rs` with the module declarations and the three structs **moved verbatim** from `src/main.rs:20-45` (`SessionInfo`, `StreamStats`, `AppState`), plus the two helpers moved verbatim from `src/main.rs:218-240` (`hostname`, `local_addresses`, made `pub`):

```rust
//! casttarget library — native cast receiver (Chromecast; AirPlay/Miracast later).

pub mod cast;
pub mod certstore;

use std::time::Instant;

pub struct SessionInfo {
    pub sender: String,
    pub state: String,
}

pub struct StreamStats {
    pub app_id: String,
    pub started: Instant,
    pub packets: u64,
    pub bytes: u64,
    pub last_rate_bps: f64,
    pub last_activity: Instant,
}

pub struct AppState {
    pub name: String,
    pub hostname: String,
    pub addresses: Vec<String>,
    pub port: u16,
    pub db_generated: Option<i64>,
    pub cert_valid_until: Option<i64>,
    pub fetch_enabled: bool,
    pub egress_allowed: bool,
    pub sessions: Vec<SessionInfo>,
    pub stream: Option<StreamStats>,
}

pub fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname").ok()
        .map(|s| s.trim().to_string())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "localhost".to_string())
}

pub fn local_addresses() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            let ip = iface.ip();
            if ip.is_ipv4() && !ip.is_loopback() {
                out.push(ip.to_string());
            }
        }
    }
    if out.is_empty() {
        out.push("127.0.0.1".to_string());
    }
    out
}
```

- [ ] **Step 2: In `main.rs`, drop the moved items and the `mod cast/certstore` lines; import from the crate**

In `src/main.rs`: delete the `SessionInfo`/`StreamStats`/`AppState` struct definitions, the `hostname`/`local_addresses` fns, and the `mod cast;`/`mod certstore;` lines. Keep `mod fakesender;` and `mod ui;` (binary-only). Add at the top:

```rust
use casttarget::{cast, certstore, hostname, local_addresses, AppState, SessionInfo, StreamStats};
```

`cast` and `certstore` used inside `main.rs` now resolve to the library's.

- [ ] **Step 3: `cast`/`certstore` internal `use crate::…` references**

Inside `src/cast/*.rs` and `src/certstore.rs`, any `use crate::{AppState, …}` still resolves — they are now in the crate root of the **library**, which is the same crate. No change expected. If any referenced `super::`/`crate::` path for `AppState` breaks, repoint it to `crate::AppState`.

- [ ] **Step 4: Both shapes compile**

Run: `cargo build --no-default-features --lib`
Expected: compiles — the library now carries the protocol and state, still no UI deps.

Run: `cargo build`
Expected: compiles — binary sees the types via `casttarget::…`.

- [ ] **Step 5: Existing unit tests pass**

Run: `cargo test --lib`
Expected: `certstore`'s tests (`egress_default_blockiert`, `egress_nur_explizites_opt_in`) and any others pass.

- [ ] **Step 6: Commit**

```bash
git add src/lib.rs src/main.rs
git commit -m "casttarget: move AppState/SessionInfo/StreamStats + helpers into the library"
```

---

### Task 3: Injectable `device_id` on `cast::Service::start`

**Files:**
- Modify: `src/cast/mod.rs`

**Interfaces:**
- Consumes: `Service::start(identity, name, port, state)` (today, `src/cast/mod.rs:27-32`), which derives `device_id` at `:37` via `stable_device_id`.
- Produces: `Service::start(identity, name, port, device_id: String, state)` — the caller supplies the id; `stable_device_id` stays as the default the binary computes.

- [ ] **Step 1: Add `device_id` as a parameter**

In `src/cast/mod.rs`, change `Service::start`'s signature to take `device_id: String` (before `state`) and delete the internal `let device_id = stable_device_id(&identity.device_cert_der);` at `:37` — use the parameter instead. Keep `stable_device_id` as a `pub fn` (the binary calls it to produce the default).

- [ ] **Step 2: Compiles**

Run: `cargo build`
Expected: fails at the `Service::start(...)` call in `main.rs` (missing argument) — that call is fixed in Task 5 when `start()` lands. For now verify the library target alone:

Run: `cargo build --lib --no-default-features`
Expected: compiles — the signature change is self-consistent inside the lib.

- [ ] **Step 3: Commit**

```bash
git add src/cast/mod.rs
git commit -m "casttarget: Service::start takes an injected device_id"
```

---

### Task 4: The library API — `ReceiverConfig`, `CastEvent`, `AccessHook`, `start()`

**Files:**
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `certstore::load_identity`/`fetch_db`/`egress_allowed`, `cast::Service::start` (Task 3), `cast::stable_device_id`.
- Produces: the entry point SP2's `native_cast::supervise` calls.

- [ ] **Step 1: Add the API types and `start()` to `lib.rs`**

Append to `src/lib.rs`. `start()` is today's `main()` body (db resolution → identity → state → refetch thread → `Service::start`) with the injected fields; it returns a handle that keeps the `Service` alive and stops it on drop:

```rust
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::SyncSender;
use std::time::Duration;
use anyhow::{Context, Result};

pub struct ReceiverConfig {
    pub name: String,            // mDNS friendly name
    pub port: u16,               // TLS port (8009 + index)
    pub device_id: String,       // injected per display
    pub app_id: String,          // Wayland app_id for the player window(s); stored, applied in SP4
    pub cast_db: PathBuf,
    pub fetch: bool,
}

pub enum CastEvent {
    Started { app_id: String },
    Ended,
}

pub trait AccessHook: Send + Sync {
    /// Asked before a sender's LAUNCH is accepted. Blocking is fine — runs on the
    /// receiver's own thread, never a tokio worker.
    fn may_cast(&self, sender: &SenderInfo) -> bool;
}

/// What the hook is told about a would-be sender. Fields come from CASTV2.
pub struct SenderInfo {
    pub sender_id: String,
    pub addr: std::net::IpAddr,
}

/// Keeps the receiver alive; dropping it stops the service.
pub struct ReceiverHandle {
    _service: cast::Service,
}

pub fn start(
    cfg: ReceiverConfig,
    access: Arc<dyn AccessHook>,
    events: SyncSender<CastEvent>,
) -> Result<ReceiverHandle> {
    let db_path = cfg.cast_db.to_string_lossy().to_string();
    let identity = certstore::load_identity(&db_path)
        .with_context(|| "cast.db konnte nicht geladen/entschluesselt werden")?;

    let state = Arc::new(Mutex::new(AppState {
        name: cfg.name.clone(),
        hostname: hostname(),
        addresses: local_addresses(),
        port: cfg.port,
        db_generated: Some(identity.db_generated),
        cert_valid_until: Some(identity.valid_until),
        fetch_enabled: cfg.fetch,
        egress_allowed: certstore::egress_allowed(),
        sessions: Vec::new(),
        stream: None,
    }));

    if cfg.fetch {
        let state = Arc::clone(&state);
        let db_path = db_path.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(6 * 3600));
            if !certstore::egress_allowed() { continue; }
            let generated = state.lock().map(|s| s.db_generated).ok().flatten();
            if let Some(g) = generated {
                if chrono::Utc::now().timestamp() - g > 30 * 24 * 3600 {
                    if let Err(e) = certstore::fetch_db(&db_path) {
                        eprintln!("[!] refetch fehlgeschlagen: {e:#}");
                    }
                }
            }
        });
    }

    let service = cast::Service::start(
        Arc::new(identity),
        &cfg.name,
        cfg.port,
        cfg.device_id.clone(),
        Arc::clone(&state),
    )
    .context("chromecast-dienst konnte nicht gestartet werden")?;

    // access + events are threaded into the protocol in Tasks 5 and 6;
    // they are carried on AppState (added there) so the session/webrtc code can reach them.
    let _ = (&access, &events, &cfg.app_id);

    Ok(ReceiverHandle { _service: service })
}
```

- [ ] **Step 2: Library compiles without UI**

Run: `cargo build --lib --no-default-features`
Expected: compiles. (`access`/`events`/`app_id` are parked by the `let _ = …` until Tasks 5–7; this is deliberate scaffolding, not a placeholder in the plan sense — the wiring tasks remove it.)

- [ ] **Step 3: Commit**

```bash
git add src/lib.rs
git commit -m "casttarget: library start() + ReceiverConfig/CastEvent/AccessHook API"
```

---

### Task 5: Carry access hook + event sender + app_id on `AppState`, call the hook at LAUNCH

**Files:**
- Modify: `src/lib.rs` (`AppState`, `start()`), `src/cast/mod.rs` (pass them through), `src/cast/session.rs` (the LAUNCH handler)

**Interfaces:**
- Consumes: `AccessHook`, `SenderInfo`, `CastEvent` (Task 4); the LAUNCH handler `handle_receiver_ns` in `src/cast/session.rs` (the reviewer located the accept path there).
- Produces: a sender refused by the hook gets no session and an idle/error `RECEIVER_STATUS`.

- [ ] **Step 1: Add the three fields to `AppState`**

In `src/lib.rs`, add to `AppState`:

```rust
    pub access: Arc<dyn AccessHook>,
    pub events: SyncSender<CastEvent>,
    pub app_id: String,
```

and set them in `start()`'s `AppState { … }` from `access.clone()`, `events.clone()`, `cfg.app_id.clone()`; remove the `let _ = (&access, &events, &cfg.app_id);` line. `Service::start` already receives the `Arc<Mutex<AppState>>`, so `cast/` can reach them.

- [ ] **Step 2: Call the hook where a LAUNCH is accepted**

In `src/cast/session.rs`, in `handle_receiver_ns` (the handler that today creates the `AppSession` on LAUNCH), before creating the session: read `state.lock()`'s `access`, build a `SenderInfo { sender_id, addr }` from the connection, and if `access.may_cast(&info)` is `false`, **do not create the session** and reply with an idle/error `RECEIVER_STATUS` (the same status shape sent when no app is running). Add the sender id/peer address to the call site if not already in scope.

- [ ] **Step 3: Library compiles**

Run: `cargo build --lib --no-default-features`
Expected: compiles.

- [ ] **Step 4: A refusing hook blocks a session (unit test)**

Add a test in `src/cast/session.rs` (or a `tests/` file) that builds an `AppState` with an allow-all vs deny-all `AccessHook`, drives `handle_receiver_ns` with a synthetic LAUNCH, and asserts a session is created in the first case and not the second. Use a tiny in-test `struct AllowAll; struct DenyAll;` implementing `AccessHook`.

Run: `cargo test --lib`
Expected: the new test passes.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/cast/mod.rs src/cast/session.rs
git commit -m "casttarget: AccessHook consulted at LAUNCH, carried on AppState"
```

---

### Task 6: Emit `CastEvent::Started`/`Ended`

**Files:**
- Modify: `src/cast/webrtc.rs` (stream start/stop), possibly `src/cast/session.rs`

**Interfaces:**
- Consumes: `AppState.events`, `AppState.app_id` (Task 5); the stream-start site in `src/cast/webrtc.rs` (where `state.stream` is set) and the stop site (where it is cleared).
- Produces: one `Started { app_id }` per cast begin, one `Ended` per cast end, on the channel.

- [ ] **Step 1: Send `Started` where the stream begins**

In `src/cast/webrtc.rs`, where `state.stream` is set to `Some(StreamStats{..})` (cast begins), also send on `state.events`: `let _ = state.events.try_send(CastEvent::Started { app_id: state.app_id.clone() });` (`try_send` so a full channel never blocks the receiver thread — drop-to-log).

- [ ] **Step 2: Send `Ended` where the stream clears**

Where `state.stream` is set back to `None` (cast ends), send `CastEvent::Ended` the same way. If teardown has more than one path, pick the single place that always runs (the session drop or the stop handler) to avoid double `Ended`.

- [ ] **Step 3: Library compiles**

Run: `cargo build --lib --no-default-features`
Expected: compiles.

- [ ] **Step 4: Events observed over a fake cast (test or harness)**

Extend the existing `scripts/ui-test.sh` path or a `--fakesender` run: start a receiver with a channel, run `--fakesender` against it, assert one `Started` then (on stop) one `Ended` arrive. If a full integration test is too heavy here, assert the send sites compile and are reached by a unit test that drives the stream-begin function with a stub channel.

Run: `cargo test --lib`
Expected: passes.

- [ ] **Step 5: Commit**

```bash
git add src/cast/webrtc.rs src/cast/session.rs
git commit -m "casttarget: emit CastEvent Started/Ended on the receiver channel"
```

---

### Task 7: Thin binary — build `ReceiverConfig`, allow-all hook, logging sink, keep the UI

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `casttarget::{start, ReceiverConfig, CastEvent, AccessHook, SenderInfo}`, `cast::stable_device_id`.
- Produces: the standalone binary, same CLI and behaviour as before.

- [ ] **Step 1: Rewrite `main()` to call the library**

In `src/main.rs`, keep `parse_args`, `mod fakesender`, `mod ui`, and the `--fakesender` early return. Replace the body from the db-resolution down with:
- resolve `db_path` with the existing `~/.local/share/casttarget/cast.db` → research-copy fallback (unchanged logic; move it into a local `fn resolve_db_path(cli: Option<String>) -> Option<String>`);
- load identity once only to derive the default device id: `let device_id = cast::stable_device_id(&certstore::load_identity(&db_path)?.device_cert_der);`
- build an allow-all hook and a logging event sink:

```rust
struct AllowAll;
impl AccessHook for AllowAll {
    fn may_cast(&self, _s: &SenderInfo) -> bool { true }
}

let (tx, rx) = std::sync::mpsc::sync_channel::<CastEvent>(32);
std::thread::spawn(move || {
    while let Ok(ev) = rx.recv() {
        match ev {
            CastEvent::Started { app_id } => println!("[cast] start ({app_id})"),
            CastEvent::Ended => println!("[cast] ende"),
        }
    }
});

let handle = casttarget::start(
    ReceiverConfig {
        name: args.name.clone(),
        port: args.port,
        device_id,
        app_id: format!("casttarget-{}", args.name),
        cast_db: db_path.into(),
        fetch: args.fetch,
    },
    std::sync::Arc::new(AllowAll),
    tx,
)?;
let _handle = handle; // keep alive
```
then the existing `--no-ui` loop and `ui::run(...)` tail (UI still gated by the `ui` feature at the crate level; `ui.rs`/`fakesender.rs` are binary modules and may use `casttarget::AppState` — but `ui::run` needs the `Arc<Mutex<AppState>>`; expose it from `start()` by returning it on the handle if the UI needs live state, or keep the UI reading a handle-provided `Arc`). **If the UI needs the live `Arc<Mutex<AppState>>`,** add `pub state: std::sync::Arc<std::sync::Mutex<AppState>>` to `ReceiverHandle` and hand `ui::run(handle.state.clone())`.

- [ ] **Step 2: Binary builds and the standalone shape works**

Run: `cargo build`
Expected: compiles with UI.

Run: `cargo build --no-default-features`
Expected: the **library** still builds with no UI; the binary needs the `ui` feature, so `--no-default-features` builds the lib target only (the bin is skipped or, if it fails for lack of UI, gate `mod ui;`/`ui::run` behind `#[cfg(feature = "ui")]` and give a headless fallback — the existing `--no-ui` loop).

- [ ] **Step 3: UI smoke test still green**

Run: `./scripts/ui-test.sh`
Expected: passes as before (see `docs/ui-testing.md`).

- [ ] **Step 4: Headless run still serves**

Run: `./target/debug/casttarget --db <path-to-cast.db> --no-ui` and confirm from the log it announces mDNS + TLS on the port, same as before the refactor. Stop with Ctrl-C.
Expected: `[+] chromecast: mDNS + TLS auf port 8009` (or the given port).

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/lib.rs
git commit -m "casttarget: thin binary over the library (allow-all hook, logging sink)"
```

---

### Task 8: Gate — the SP1 acceptance

**Files:** none (verification).

- [ ] **Step 1: Library builds with no UI deps**

Run: `cargo build --lib --no-default-features`
Expected: compiles; `cargo tree --no-default-features -i winit` reports winit is **not** in the tree.

- [ ] **Step 2: Binary + UI build and smoke test**

Run: `cargo build && ./scripts/ui-test.sh`
Expected: both green.

- [ ] **Step 3: Unit tests pass**

Run: `cargo test`
Expected: green, including the new access-hook test.

- [ ] **Step 4: Cross-compile sanity (the SP2 link concern starts here)**

Run: `cargo build --lib --no-default-features --target armv7-unknown-linux-gnueabihf` if the `cross`/target is set up; otherwise note it for SP2.
Expected: compiles, or a recorded note that it is deferred to SP2's `cross build`.

- [ ] **Step 5: Push the branch for SP2 to depend on**

```bash
git-wrapper.sh -C /home/philipp/Projects/_fablab/casttarget push origin HEAD
```
(Remote push needs the YubiKey via `git-wrapper.sh`.) SP2 adds this commit as a submodule of miniclientcontrol.
