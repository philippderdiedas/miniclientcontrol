# Native cast SP2: mc feature-gated receivers-per-display — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** miniclientcontrol pulls `casttarget` as an optional submodule dependency behind feature `native-cast`, and `native_cast::supervise` starts one native-cast receiver per declared display — with an access hook that reuses `cast::access` and an event channel that is drained (events are logged in SP2; SP3 wires them to the override).

**Architecture:** A new feature-gated module `src/native_cast/` holds `NativeCastHook` (implements `casttarget::AccessHook`, resolves the screen's access through `cast::access::load` + `decide` via `Handle::block_on`, since the hook runs on casttarget's own thread) and `supervise` (one `casttarget::start` per `DisplayConfig`, port `8009+index`, a per-display `device_id` derived from the shared cast.db identity, `app_id = casttarget-<display.name>`, a bounded event channel drained by a tokio task). `main` calls `supervise` after the per-display loops when the feature is on; with the feature off, nothing links and nothing changes.

**Tech Stack:** Rust 2021, tokio (mc), `sqlx` (mc db), `casttarget` lib (sync/threads), feature flags.

**Working dir:** `/home/philipp/Projects/_fablab/kiosk-pxe/miniclientcontrol` (its own git repo; it is itself a submodule of kiosk-pxe). Remote git via `git-wrapper.sh` (YubiKey); local commits do not.

## Global Constraints

- **Feature `native-cast` is default OFF.** With it off, the crate links no `casttarget` and behaves exactly as today — every `tests/cast/` suite passes unchanged (`CLAUDE.md`: the gate for the Rust side is `cargo build`).
- **The access hook treats `account` as `off` for native casts** (decided in the spec): `decide(venue_on, access, signed_in=false)` returns `Err` for both `Off` and `Account`, which the hook maps to refuse. Not parity — documented.
- **One receiver per declared display**, `name` from the display, port `8009 + declaration index`, device id unique per display, `app_id = casttarget-<display.name>`.
- **SP2 does not touch the override or freeze** — events are received and logged only; SP3 wires them.
- **`rusqlite` (casttarget) + `sqlx` (mc) must stay on `libsqlite3-sys 0.30.x`** — do not bump either; verify the link under the feature (`cargo build --features native-cast`).
- Commit messages carry **no** `Co-Authored-By: Claude` / `Claude-Session` trailer.

---

### Task 1: Submodule + Cargo feature

**Files:**
- Modify: `Cargo.toml`, `.gitmodules` (created by the submodule add)
- Add: `casttarget/` (submodule)

**Interfaces:**
- Produces: `casttarget` as an optional dependency gated by feature `native-cast`.

- [ ] **Step 1: Add casttarget as a submodule**

Run (remote, needs the YubiKey):
```bash
git-wrapper.sh -C /home/philipp/Projects/_fablab/kiosk-pxe/miniclientcontrol submodule add git@git.fablab-luebeck.de:it/casttarget.git casttarget
```
Expected: `casttarget/` cloned, `.gitmodules` created, pinned at SP1's HEAD (`9f33dd9`).

- [ ] **Step 2: Declare the optional dep and the feature**

In `Cargo.toml`, in `[dependencies]`, add (the lib with **no** default features, so its UI deps never link):
```toml
casttarget = { path = "casttarget", optional = true, default-features = false }
```
Add a features table (there is none today):
```toml
[features]
native-cast = ["dep:casttarget"]
```

- [ ] **Step 3: Both feature shapes build**

Run: `cargo build`
Expected: compiles, no `casttarget` in the tree (`cargo tree -i casttarget` → not found).

Run: `cargo build --features native-cast`
Expected: compiles, `casttarget` present (`cargo tree -i casttarget` → shows it), and `cargo tree --features native-cast -i winit` → **not found** (the lib's UI deps stay out).

- [ ] **Step 4: libsqlite3-sys stays single**

Run: `cargo tree --features native-cast -i libsqlite3-sys`
Expected: exactly one version (0.30.x), proving `rusqlite` and `sqlx` dedupe to one C build.

- [ ] **Step 5: Commit**

```bash
git add .gitmodules casttarget Cargo.toml
git commit -m "mc: casttarget als optionales Submodule hinter Feature native-cast"
```

---

### Task 2: `--cast-db` argument

**Files:**
- Modify: `src/models.rs` (the `Args` struct), wherever args are parsed (clap derive on `Args`)

**Interfaces:**
- Produces: `args.cast_db: Option<PathBuf>`; the resolved path defaults to the data dir beside `miniclient.db`.

- [ ] **Step 1: Add the arg**

In `src/models.rs`, in `Args` (near `database_path` / `assets_dir`), add a clap field:
```rust
/// Path to the Chromecast certificate DB (cast.db). Defaults to cast.db beside
/// the database (the USB data partition). Only used with feature native-cast.
#[arg(long, env)]
pub cast_db: Option<std::path::PathBuf>,
```

- [ ] **Step 2: A resolver next to it (a free fn, so both feature shapes compile)**

Add to `src/models.rs`:
```rust
impl Args {
    /// cast.db location: the flag, else cast.db in the directory of database_path.
    pub fn cast_db_path(&self) -> std::path::PathBuf {
        self.cast_db.clone().unwrap_or_else(|| {
            std::path::Path::new(&self.database_path)
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("cast.db")
        })
    }
}
```

- [ ] **Step 3: Build**

Run: `cargo build`
Expected: compiles (arg present, unused without the feature — allowed).

- [ ] **Step 4: Commit**

```bash
git add src/models.rs
git commit -m "mc: --cast-db arg (default: cast.db neben miniclient.db)"
```

---

### Task 3: `NativeCastHook` — access via `cast::access`

**Files:**
- Create: `src/native_cast/mod.rs`
- Modify: `src/main.rs` (declare `#[cfg(feature = "native-cast")] mod native_cast;`)

**Interfaces:**
- Consumes: `cast::access::{load, decide, Access}`, `cast::mod::ClaimMode::Cast`, `AppState.{pool, settings}`, `casttarget::{AccessHook, SenderInfo}`.
- Produces: `NativeCastHook` implementing `casttarget::AccessHook`.

- [ ] **Step 1: The module and the hook**

Create `src/native_cast/mod.rs`:
```rust
//! Native cast receivers (Chromecast) wired into the per-display framework.
//! Feature-gated: nothing here links without `native-cast`.

use std::sync::Arc;

use crate::cast::access::{self, Access};
use crate::cast::ClaimMode;
use crate::settings::AppSettings;
use tokio::sync::RwLock;

/// Access decision for a native sender on one screen. Runs on casttarget's own
/// std::thread, so `block_on` is safe (mc's runtime has other worker threads and
/// this is not one of them). `account` resolves to refuse: a native sender has no
/// mc session, so `decide(.., signed_in=false)` returns Err for Account and Off.
pub struct NativeCastHook {
    pub handle: tokio::runtime::Handle,
    pub pool: sqlx::SqlitePool,
    pub screen: String,
    pub settings: Arc<RwLock<AppSettings>>,
}

impl casttarget::AccessHook for NativeCastHook {
    fn may_cast(&self, _sender: &casttarget::SenderInfo) -> bool {
        self.handle.block_on(async {
            let venue_on = self.settings.read().await.cast_enabled;
            let access: Access = access::load(&self.pool, &self.screen, ClaimMode::Cast).await;
            access::decide(venue_on, access, false).is_ok()
        })
    }
}
```

- [ ] **Step 2: Declare the module in main.rs**

In `src/main.rs`, beside the other `mod` lines:
```rust
#[cfg(feature = "native-cast")]
mod native_cast;
```

- [ ] **Step 3: Build with the feature**

Run: `cargo build --features native-cast`
Expected: compiles.

- [ ] **Step 4: Unit-test the access mapping (no pool needed)**

The DB-reading `load` needs a pool, but the *mapping* that defines the policy is
`access::decide` — assert the native semantics directly in `src/native_cast/mod.rs`:
```rust
#[cfg(test)]
mod tests {
    use crate::cast::access::{decide, Access, Refusal};
    // Native casts pass signed_in=false. The policy SP2 commits to:
    #[test]
    fn anyone_accepts_native() {
        assert!(decide(true, Access::Anyone, false).is_ok());
    }
    #[test]
    fn account_refuses_native_like_off() {
        assert_eq!(decide(true, Access::Account, false), Err(Refusal::NeedsAccount));
        assert_eq!(decide(true, Access::Off, false), Err(Refusal::Off));
    }
    #[test]
    fn venue_off_refuses_even_anyone() {
        assert_eq!(decide(false, Access::Anyone, false), Err(Refusal::Off));
    }
}
```

Run: `cargo test --features native-cast native_cast::`
Expected: the three tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/native_cast/mod.rs src/main.rs
git commit -m "mc: NativeCastHook - native cast access via cast::access (account=off)"
```

---

### Task 4: `supervise` — one receiver per display

**Files:**
- Modify: `src/native_cast/mod.rs`

**Interfaces:**
- Consumes: `AppState`, `Vec<DisplayConfig>`, `casttarget::{start, ReceiverConfig, certstore, cast::stable_device_id}`, `Args::cast_db_path`.
- Produces: `pub fn supervise(state: AppState, configured: Vec<crate::display::DisplayConfig>)`.

- [ ] **Step 1: Write `supervise`**

Append to `src/native_cast/mod.rs`:
```rust
use crate::models::AppState;

const BASE_CAST_PORT: u16 = 8009;

/// Start one native-cast receiver per declared display. Receivers live until the
/// process ends (like the browser the controller does not kill), so their handles
/// are deliberately leaked with `std::mem::forget` — SP3+ may hold them in state
/// for teardown.
pub fn supervise(state: AppState, configured: Vec<crate::display::DisplayConfig>) {
    let cast_db = state.args.cast_db_path();
    let db_str = cast_db.to_string_lossy().to_string();

    // One shared identity -> a base device id; each display gets a unique suffix,
    // so the mDNS device ids differ while the cast identity is shared (see spec risk).
    let base = match casttarget::certstore::load_identity(&db_str) {
        Ok(id) => casttarget::cast::stable_device_id(&id.device_cert_der),
        Err(e) => {
            tracing::error!("native-cast: cast.db ({}) nicht ladbar: {:#} — Receiver aus", db_str, e);
            return;
        }
    };

    for (index, cfg) in configured.into_iter().enumerate() {
        let screen = cfg.name.clone();
        let port = BASE_CAST_PORT + index as u16;
        let app_id = format!("casttarget-{}", cfg.name);
        let device_id = format!("{base}{index}");

        let hook = Arc::new(NativeCastHook {
            handle: tokio::runtime::Handle::current(),
            pool: state.pool.clone(),
            screen: screen.clone(),
            settings: state.settings.clone(),
        });

        let (tx, rx) = std::sync::mpsc::sync_channel::<casttarget::CastEvent>(32);

        // Drain the channel off the runtime; SP3 turns these into overrides.
        let drain_screen = screen.clone();
        tokio::task::spawn_blocking(move || {
            while let Ok(ev) = rx.recv() {
                match ev {
                    casttarget::CastEvent::Started { app_id } => {
                        tracing::info!("native-cast: start auf {} (app_id={})", drain_screen, app_id)
                    }
                    casttarget::CastEvent::Ended => {
                        tracing::info!("native-cast: ende auf {}", drain_screen)
                    }
                }
            }
        });

        let fetch = index == 0; // only one receiver refetches the shared cast.db
        match casttarget::start(
            casttarget::ReceiverConfig {
                name: cfg.name.clone(),
                port,
                device_id,
                app_id,
                cast_db: cast_db.clone(),
                fetch,
            },
            hook,
            tx,
        ) {
            Ok(handle) => {
                tracing::info!("native-cast: '{}' auf Port {} announced", screen, port);
                std::mem::forget(handle); // lives until process exit
            }
            Err(e) => tracing::error!("native-cast: Receiver '{}' nicht gestartet: {:#}", screen, e),
        }
    }
}
```

- [ ] **Step 2: Build with the feature**

Run: `cargo build --features native-cast`
Expected: compiles.

- [ ] **Step 3: Commit**

```bash
git add src/native_cast/mod.rs
git commit -m "mc: native_cast::supervise - ein Receiver je Display, Access+Events verdrahtet"
```

---

### Task 5: Call `supervise` from main

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `native_cast::supervise`, the `state` and `configured` in scope after the per-display loops are spawned (`src/main.rs` ~line 261–272).

- [ ] **Step 1: Spawn the receivers after the per-display loops**

In `src/main.rs`, after the `for display in state.displays.iter()` block that spawns `browser_loop`/`freeze::watch` (and before the HTTP server starts), add:
```rust
#[cfg(feature = "native-cast")]
native_cast::supervise(state.clone(), configured.clone());
```
`state` is `AppState` (Clone — it is all `Arc`s) and `configured` is the
`Vec<DisplayConfig>` still in scope. If `configured` was moved earlier, clone it
at the call site instead.

- [ ] **Step 2: Feature-off build and tests unchanged**

Run: `cargo build`
Expected: compiles; the `supervise` call is `#[cfg]`-compiled out.

Run: `cargo test` (feature off)
Expected: the existing suite passes exactly as before — nothing in the default build changed.

- [ ] **Step 3: Feature-on build**

Run: `cargo build --features native-cast`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "mc: native-cast Receiver beim Start spawnen (feature-gated)"
```

---

### Task 6: Gate — the SP2 acceptance

**Files:** none (verification).

- [ ] **Step 1: Feature off changes nothing**

Run: `cargo build && cargo test`
Expected: green, identical to pre-SP2 (no `casttarget` linked).

Run: `cargo tree -i casttarget`
Expected: not in the tree.

- [ ] **Step 2: Feature on builds and the native tests pass**

Run: `cargo build --features native-cast && cargo test --features native-cast`
Expected: green, including `native_cast::tests`.

- [ ] **Step 3: One receiver per display, access enforced (integration, on a device)**

Build with the feature, run mc declaring two displays with a real cast.db on the
data dir, and from another machine:
```bash
# the device announces two Google Cast targets:
avahi-browse -rt _googlecast._tcp | grep -c casttarget   # or the display names
```
Expected: two receivers, one per declared display, on ports 8009 and 8010.
Set one screen's `cast_access` to `off` (or `account`) via the admin and confirm a
native sender is refused there and accepted on an `anyone` screen. (The window
placement + override is SP3/SP4; here only announce + access are checked.)

- [ ] **Step 4: Cross-compile under the feature (the SP1-deferred link check)**

Run: `cross build --features native-cast --target armv7-unknown-linux-gnueabihf`
Expected: compiles — `rusqlite` (bundled) + `sqlx` link once against the cross
image's C toolchain. If `cross` is not set up, record it for the deployment build.

- [ ] **Step 5: Push (needs the YubiKey)**

```bash
git-wrapper.sh -C /home/philipp/Projects/_fablab/kiosk-pxe/miniclientcontrol push origin master
```
And the kiosk-pxe superproject's submodule pointer is bumped in its own commit
when SP4 wires the sway rule and cast.db placement.

## Deferred to SP3/SP4 (named so this plan's boundaries are clear)

- The mDNS **friendly name from the editable `label`** (DB), not `cfg.name`. SP2
  uses `cfg.name` (the declared identity); loading the label is a small block_on
  query that belongs with the admin-visible naming in SP4.
- Turning `CastStarted`/`CastEnded` into the **external-window override + freeze**
  (SP3).
- The **sway placement rule** and **cast.db on the data partition** in kiosk-pxe
  (SP4).
- Holding `ReceiverHandle`s in state for **clean teardown** (SP2 leaks them).
