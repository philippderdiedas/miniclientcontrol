# Per-display casting Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every declared screen its own cast session, so two guests can cast to two screens at once and each guest says which screen they mean.

**Architecture:** `CastSession` moves out of `AppState` and becomes a field of `Display`, one per declared screen; every function in the cast subsystem takes the `Arc<Display>` it is acting on instead of resolving one. Guest-facing routes keep their literal paths and name the screen in a body field or query parameter — the two auth exemption predicates stay exact-string matches — while the one operator-only route gains a `/api/displays/{name}/…` path. Room audio stays a single venue resource with an owner.

**Tech Stack:** Rust 2021, axum 0.8, tokio, sqlx/SQLite, chromiumoxide (CDP), vanilla HTML/JS compiled into the binary via `include_dir!`, stdlib-only Python 3 integration tests.

## Global Constraints

- The spec is `docs/superpowers/specs/2026-09-16-per-display-casting-design.md`. Where this plan and the spec disagree, the spec wins — say so in the task report rather than silently following one.
- `cargo build` must be **warning-clean** after every task. `cargo test` must pass. 104 tests pass at the base commit.
- **No `Co-Authored-By: Claude` or `Claude-Session:` trailer on any commit, ever.**
- `web/` is compiled into the binary (`include_dir!`, `src/web.rs`). **Rebuild after every change under `web/`** — an unrebuilt change looks exactly like a change that did not work.
- **Never widen `is_cast_public_path` or `is_display_path` beyond exact string literals.** Both are flat `matches!` lists. A screen name is a body field or a query parameter, never a path segment, on any route in either list.
- **Lock order is `settings` (RwLock) → `cast` (Mutex) → `Display::override_item` (Mutex)**, and is never inverted today. Preserve it. Two sites deliberately hold the cast guard across `.await`: `activate_display` and `deactivate_display`, both across `override_item.lock().await`.
- `Dispatcher::fire` stays synchronous, infallible and returning `()`.
- German for every operator- and guest-facing string, matching the existing pages.
- Stop any locally running instance before the Python suites. `test_display.py` and `test_webhook.py` both use CDP 9242 — never run them concurrently.
- Run the Python suites named in a task **before** committing that task. A task that changes an endpoint fixes the suite cases it breaks, in the same commit.

## File Structure

**New files**

| File | Responsibility |
|---|---|
| `src/cast/mod.rs` | Types, `CastSession` + impl, constants, `routes()`, `activate_display`/`deactivate_display`/`end_session`, the three `watch_*`, existing tests |
| `src/cast/signaling.rs` | The socket: `cast_ws`, `handle_socket`, `handle_frame`, `consume_reservation`, `register_peer`, `unregister_peer`, `ice_servers` |
| `src/cast/api.rs` | HTTP handlers: `cast_state`, `cast_info`, `claim_session`, `release_session`, `stop_cast`, `start_pairing`, `authorize_sender` |
| `src/cast/room_audio.rs` | `caster_only`, `cast_process_ids`, `read_audio`, `apply_audio`, `control_audio`. Named `room_audio` so the tree does not carry two modules called `audio` |
| `src/cast/url.rs` | `sender_url`, `cast_qr`, `qr_matrix`, `qr_svg`, `cast_display_url` |
| `tests/cast/test_castscreens.py` | The integration suite, cases `[80]`–`[89]` |

**Deleted:** `src/cast.rs` (moved), `AppState::cast_display` (`src/models.rs:449-455`), `Args::cast_display` (`src/models.rs:51-59`), `display::check_cast_display` (`src/display.rs:169-184`).

**Heavily modified:** `src/models.rs` (`Display` gains `cast`), `web/index.html` (bind/chooser/switcher), `web/admin.html` (per-screen cast lines), `web/cast.js` (socket URL carries the screen), `web/cast_display.html`, `web/empty_playlist.html`, `src/browser.rs` (idle URL carries the screen), `src/settings.rs` (`overlay_payload` per display).

---

### Task 1: Split `cast.rs` into a `cast/` directory — a pure move

**Files:**
- Create: `src/cast/mod.rs`, `src/cast/signaling.rs`, `src/cast/api.rs`, `src/cast/room_audio.rs`, `src/cast/url.rs`
- Delete: `src/cast.rs`
- Modify: nothing else. `src/main.rs`'s `mod cast;` already resolves to a directory module.

**Interfaces:**
- Produces: the same public surface as today, unchanged — `SharedCastSession`, `CastSession::is_active`, `is_cast_public_path`, `sender_url`, `qr_matrix`, `end_session`, `cast_process_ids`, `apply_audio`, `routes`. Every one re-exported from `mod.rs` so no caller outside `src/cast/` changes.

**This task changes no behaviour at all.** Follow `src/webhook/` — the house precedent, whose own comment says it was split "only for file size".

- [ ] **Step 1: Create the directory and move the regions**

`src/cast.rs` is 2001 lines. Move whole regions, do not retype them:

| New file | Lines from `src/cast.rs` | Contents |
|---|---|---|
| `mod.rs` | 1-771 (minus the URL block), 1573, 1574-2001 | module docs, constants 54-83, `Role`/`Peer`/`Pairing`/`Reservation`/`ClaimMode`/`Showing`/`Attempts`/`DisplayLimits`/`CastSession` 85-215, `routes` 263-276, `is_cast_public_path` 284-304, `activate_display` 428-498, `deactivate_display` 500-591, `end_session` 593-610, `watch_sender_grace` 612-626, `watch_display_arrival` 628-648, `watch_pairing_expiry` 650-666, `generate_code`/`generate_ticket`/`codes_match` 391-410, `error_frame` 774-776, `SharedCastSession`, `mod tests` |
| `url.rs` | 312-389 | `sender_url`, `cast_qr`, `qr_matrix`, `qr_svg`, `cast_display_url` |
| `signaling.rs` | 772-1242 whole (minus `error_frame`) | `ice_servers`, `CastWsQuery`, `cast_ws`, `handle_socket`, `handle_frame`, `consume_reservation`, `register_peer`, `unregister_peer` |
| `api.rs` | 668-770 (`authorize_sender`), 1244-1440, 1512-1571 | `PairingView`, `CastStateResponse`, `cast_state`, `cast_info`, `ClaimRequest`, `claim_session`, `release_session`, `stop_cast`, `start_pairing`, `authorize_sender` |
| `room_audio.rs` | 1439-1510 | `caster_only`, `cast_process_ids`, `read_audio`, `apply_audio`, `control_audio` |

`mod.rs` declares and re-exports:

```rust
mod api;
mod room_audio;
mod signaling;
mod url;

pub use api::{cast_info, cast_state, claim_session, release_session, start_pairing, stop_cast};
pub use room_audio::{apply_audio, cast_process_ids, control_audio, read_audio};
pub use signaling::cast_ws;
pub use url::{cast_qr, qr_matrix, sender_url};
```

- [ ] **Step 2: Widen exactly the visibility the move requires, and no more**

Twelve of `CastSession`'s fourteen fields are touched from more than one of the new files. Change **only those** from private to `pub(super)`, leaving any field used in one file alone. Same for the private functions that cross a file boundary: `activate_display`, `deactivate_display`, `end_session`, `error_frame`, `authorize_sender`, `caster_only`, `cast_display_url`, `generate_code`, `generate_ticket`, `codes_match`, and the `CastSession` methods `is_taken`, `taken_by_other`, `live_reservation` become `pub(super)`.

Write this comment above the struct, because it is the price of the split and a later reader must be able to see it:

```rust
/// Every field is `pub(super)` rather than private: the module is a directory,
/// so the compiler's "nobody outside this file can corrupt the session"
/// guarantee now covers `src/cast/` rather than one file. That is what the split
/// cost. Nothing outside this directory may touch a field -- `is_active` and
/// `showing_json` are the whole outside surface.
```

- [ ] **Step 3: Build and test**

Run: `cargo build 2>&1 | tail -20`
Expected: `Finished`, **no warnings**. An unused-import warning means a `use` did not follow its code; fix it rather than allowing it.

Run: `cargo test 2>&1 | tail -5`
Expected: `104 passed; 0 failed`.

- [ ] **Step 4: Prove the move changed nothing, by diffing the code itself**

A binary comparison is not available here — paths and line numbers are baked into panic messages, so the two builds differ for reasons that say nothing about the move. Compare the source instead:

```bash
git show HEAD:src/cast.rs | grep -v '^\s*$' | sed 's/^\s*//' | sort > /tmp/cast-before.txt
cat src/cast/*.rs | grep -v '^\s*$' | sed 's/^\s*//' | sort > /tmp/cast-after.txt
diff /tmp/cast-before.txt /tmp/cast-after.txt
```

Expected: every line in the diff is one you deliberately added — `mod`/`use`/`pub use` lines, the `pub(super)` visibility changes from Step 2, and the new struct comment. **Nothing else may appear**, in either direction. Paste that diff into the task report; it is the evidence the move was pure, and a line you cannot explain is a line you changed by accident.

- [ ] **Step 5: Run the cast suites unchanged**

```bash
python3 tests/cast/test_cast.py
python3 tests/cast/test_pairing.py
python3 tests/cast/test_reserve.py
python3 tests/cast/test_conflict.py
python3 tests/cast/test_guestpage.py
python3 tests/cast/test_limits.py
python3 tests/cast/test_audio.py
```

Expected: every one ALL PASSED, with **no edits to any of them**. That is the proof the move was pure.

- [ ] **Step 6: Commit**

```bash
git add -A src/cast.rs src/cast
git commit -m "Split casting into a directory module"
```

---

### Task 2: One `CastSession` per display

**Files:**
- Modify: `src/models.rs` (`Display` gains `cast`; `AppState::cast` removed), `src/cast/mod.rs`, `src/cast/signaling.rs`, `src/cast/api.rs`, `src/cast/room_audio.rs`, `src/settings.rs:730`, `src/main.rs:275`

**Interfaces:**
- Consumes: Task 1's `src/cast/` layout.
- Produces:
  - `Display::cast: crate::cast::SharedCastSession`
  - `activate_display(state: &AppState, display: &Arc<Display>, showing: Showing, sender_ip: Option<IpAddr>)`
  - `deactivate_display(state: &AppState, display: &Arc<Display>, reason: &'static str)`
  - `end_session(state: &AppState, display: &Arc<Display>, reason: &'static str)`
  - `cast_process_ids(state: &AppState, display: &Arc<Display>) -> Vec<u32>`
  - `apply_audio(state: &AppState, display: &Arc<Display>, command: AudioCommand) -> Response`
  - `watch_sender_grace(state: AppState, display: Arc<Display>, epoch: u64, grace: Duration)`, and the same shape for `watch_display_arrival` and `watch_pairing_expiry`

This is the one unavoidably large task: the tree does not compile between the first line of it and the last. Do not try to split it.

- [ ] **Step 1: Move the session onto `Display`**

In `src/models.rs`, delete `pub cast: crate::cast::SharedCastSession` from `AppState` (line 414) and add to `Display`:

```rust
    /// This screen's cast session. One per display: two guests casting to two
    /// screens share no state, no timers and no reservation. `attempts` is the
    /// deliberate exception and stays on `AppState` -- see `CastSession`.
    pub cast: crate::cast::SharedCastSession,
```

Delete `AppState::cast_display` (`src/models.rs:449-455`) — its four call sites all disappear in this task.

In `src/main.rs`, remove `cast: Arc::new(Mutex::new(Default::default())),` from the `AppState` literal (line 275) and initialise each `Display`'s `cast` where displays are built.

- [ ] **Step 2: Move `attempts` off the session**

`attempts: HashMap<IpAddr, Attempts>` must **not** go per screen: per screen it multiplies by the number of displays and hands an attacker N tries at a four-digit code instead of one. Move it to `AppState`:

```rust
    /// Failed pairing-code attempts per source address, controller-wide.
    ///
    /// Deliberately not per display: five tries is five tries for the venue, not
    /// five per screen. `tests/cast/test_pairing.py` case [8] is what pins this.
    pub cast_attempts: Arc<Mutex<HashMap<IpAddr, Attempts>>>,
```

`authorize_sender` reads and writes it there instead of through the session.

`Attempts` is a private type in the cast module today (`struct Attempts` — no `pub`). Naming it in an `AppState` field makes it part of the crate surface, so it becomes `pub struct Attempts` with `pub(crate)` fields. That is a real widening; do not widen anything else while you are there.

**`display_limits` stays a field of `CastSession`** and keeps its existing exemption from the teardown reset. The spec says it belongs "on `Display` but outside the session"; now that the session *is* per display and lives exactly as long as the `Display` does, leaving it in place achieves the same thing with no move — it already survives a session because `deactivate_display` deliberately does not clear it. Say so in the task report, since it is a deliberate departure from the spec's wording and a reviewer will check.

- [ ] **Step 3: Thread the display through every function that touches a session**

Each function named in the Interfaces block gains an `&Arc<Display>` (or `Arc<Display>` for the spawned watchers) and uses `display.cast` instead of `state.cast`. The four former `cast_display()` sites become the parameter:

- `activate_display` (was `src/cast.rs:441`) — the `override_item`, `override_signal` and the three webhook fires all take the passed display.
- `deactivate_display` (was `:512`) — same, four fires.
- `register_peer`'s `cast.started` fire (was `:1198`) — the display it just registered against.
- `cast_process_ids` (was `:1449`) — `display.browser_pid`.

`handle_socket` and `handle_frame` carry the display from admission onward, so a socket can never act on a session other than the one it was admitted to.

- [ ] **Step 4: `settings.rs` asks a display, not the controller**

`src/settings.rs:730` is the only use of `state.cast` outside the cast module:

```rust
let casting = state.cast.lock().await.is_active();
```

`overlay_payload` must become display-aware, because `hide_during_cast` and the cast-QR drop are now per screen. Give it the display and read `display.cast`. Every caller of `overlay_payload` already has one — `browser.rs` is inside `browser_loop(state, display)`, and the admin preview resolves through `display::resolve`.

- [ ] **Step 5: Fix the Rust tests**

`state_for_displays` loses its `cast_display: Option<&str>` parameter (was `src/cast.rs:1679-1683`, `:1710`), which changes `state_for` too. Delete `a_cast_pins_the_display_the_deployment_chose` (`src/cast.rs:1819-1863`) — it pins a flag that is being removed in Task 8; do not try to salvage it.

Add this test in its place, which is the property that replaces it:

```rust
#[tokio::test]
async fn two_screens_hold_independent_sessions() {
    let state = state_for_displays(&["foyer", "werkstatt"]).await;
    let foyer = state.display("foyer").unwrap();
    let werkstatt = state.display("werkstatt").unwrap();

    activate_display(&state, &foyer, Showing::Cast, Some("10.0.0.5".parse().unwrap())).await;

    assert!(foyer.cast.lock().await.is_active(), "the screen that was activated");
    assert!(
        !werkstatt.cast.lock().await.is_active(),
        "the other screen must be untouched -- one session per display is the whole feature"
    );
    assert!(foyer.override_item.lock().await.is_some());
    assert!(
        werkstatt.override_item.lock().await.is_none(),
        "activating one screen must not pin an override on another"
    );
}
```

- [ ] **Step 6: Build, test, mutation-check**

Run: `cargo build 2>&1 | tail -20` → `Finished`, no warnings.
Run: `cargo test 2>&1 | tail -5` → all pass.

Mutation-check the new test: make `activate_display` write to `state.displays[0]` instead of the passed display, confirm `two_screens_hold_independent_sessions` fails, restore. Quote the failure in the report.

- [ ] **Step 7: Run the cast suites**

```bash
python3 tests/cast/test_cast.py
python3 tests/cast/test_pairing.py
python3 tests/cast/test_reserve.py
python3 tests/cast/test_conflict.py
python3 tests/cast/test_guestpage.py
python3 tests/cast/test_limits.py
python3 tests/cast/test_audio.py
```

All seven declare **one** display, so all seven must still pass **unedited** — the session is per display, and with one display there is one session. If a case fails here, the threading is wrong, not the test. Report any failure rather than editing the suite.

- [ ] **Step 8: Commit**

```bash
git add src/models.rs src/main.rs src/cast src/settings.rs
git commit -m "Give every screen its own cast session"
```

---

### Task 3: Resolve the screen a guest names

**Files:**
- Modify: `src/cast/api.rs`, `src/cast/signaling.rs`
- Modify: `tests/cast/test_cast.py`, `tests/cast/test_pairing.py`

**Interfaces:**
- Consumes: Task 2's per-display sessions.
- Produces: `ClaimRequest { code, mode, display: Option<String> }`; `PairRequest { display: Option<String> }`; a ticket that names its screen.

- [ ] **Step 1: Reuse `display::resolve`, do not write a second resolver**

`display::resolve(state: &AppState, name: Option<&str>) -> Result<Arc<Display>, Response>` (`src/display.rs:212`) already implements exactly the rule the spec asks for: `Some(name)` resolves or `404`s naming the declared screens; `None` resolves while one display is declared and answers `409` listing them once several are. `test_display.py` `[75]` pins it.

`claim_session` and `start_pairing` call it with the body's `display` field. Do not add a cast-specific copy — two resolvers drift, and this one is already tested.

- [ ] **Step 2: `claim` and `pair` take a display**

```rust
#[derive(Deserialize)]
pub struct ClaimRequest {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    mode: ClaimMode,
    /// Which screen the guest wants. Omitted resolves while exactly one display
    /// is declared and refuses with `409` once several are -- `display::resolve`,
    /// the same rule the playback routes follow.
    #[serde(default)]
    display: Option<String>,
}
```

`start_pairing` gains the same field through a new `PairRequest`. It is currently `pub async fn start_pairing(State(state): State<AppState>) -> Response` with no body; it becomes `Json(payload): Json<PairRequest>`. **`web/index.html:664` posts no body today** — it must send `{}` at minimum; that edit belongs to Task 10, so until then make the extractor tolerate an absent body with `Option<Json<PairRequest>>`, and say in the report that Task 10 removes the tolerance.

- [ ] **Step 3: The ticket names its screen**

`Reservation` lives inside a per-display session now, so a ticket minted for `foyer` is simply absent from `werkstatt`'s session. That is *almost* enough. Make it explicit so the refusal is legible rather than incidental: add `display: String` to `Reservation`, and in `consume_reservation` refuse a ticket whose `display` is not this session's screen with `"Dieses Ticket gehört zu einem anderen Bildschirm."`.

The socket learns its screen from the ticket, not from a parameter — a sender's `?screen=` would be a claim the ticket already settles.

- [ ] **Step 4: The display socket says which screen it is**

`cast_ws` accepts `?role=display&screen=foyer`. `CastWsQuery` gains `screen: Option<String>`, resolved with `display::resolve`. The display role is already loopback-only (`src/cast/signaling.rs`, was `cast.rs:796-827` admission step 3), so this parameter is only ever set by our own page.

- [ ] **Step 5: Fix the two suites this breaks**

- `test_pairing.py` `[7]`: `POST /api/cast/pair` now needs a body. Send `{}`.
- `test_cast.py` `[1]` line 141 asserts "a second sender is refused 409". With one declared display it still passes — but add a second case asserting the refusal is **per screen**, which is what this task makes true. Both suites declare one display, so nothing else moves.

- [ ] **Step 6: Build, test, run the suites**

Run: `cargo build` → clean. `cargo test` → pass.

```bash
python3 tests/cast/test_cast.py
python3 tests/cast/test_pairing.py
python3 tests/cast/test_reserve.py
python3 tests/cast/test_guestpage.py
```
Expected: ALL PASSED.

- [ ] **Step 7: Commit**

```bash
git add src/cast tests/cast/test_cast.py tests/cast/test_pairing.py
git commit -m "Let a guest name the screen they want"
```

---

### Task 4: `info` lists the screens, `state` takes one

**Files:**
- Modify: `src/cast/api.rs`
- Modify: `tests/cast/test_limits.py`, `tests/cast/test_port.py`

**Interfaces:**
- Produces: `GET /api/cast/info` → `{enabled, page_enabled, auth, screens: [{name, label, busy, max_edge}]}`; `GET /api/cast/state?screen=<name>`.

- [ ] **Step 1: `cast_info` returns the screen list**

Per screen: `name`, `label` (the operator's, from the `displays` table), `busy` (that session's `is_taken()`), and `max_edge` (that session's `display_limits`, or `null`).

`max_edge` is in the list because a sender constrains capture **before** it has a socket — the reason `display_limits` outlives a session — and with one session per screen there is no single limit to report.

When `settings.cast_enabled` is false or `args.disable_cast` is set, return `enabled: false` and **no** `screens` key at all. A switched-off feature must not enumerate the venue.

- [ ] **Step 2: `cast_state` takes `?screen=`**

`#[derive(Deserialize)] pub struct ScreenQuery { #[serde(default)] screen: Option<String> }`, resolved with `display::resolve`. The path stays `/api/cast/state` — it is in `is_display_path`, which must remain a literal list.

- [ ] **Step 3: Fix the suites**

- `test_limits.py` `[30]`–`[34]`: all five read the global `display_limits` from `/api/cast/info`. Rewrite them against `screens[0].max_edge`. These are the cases that prove a limit survives a session, so keep that assertion — just read it from the new place.
- `test_port.py` uses `/api/cast/state` as a liveness probe. With one display declared it still resolves, so confirm by running it; if the probe now needs a parameter, give it one rather than changing the route.
- `test_cast.py` `[4]` reads a global `info["busy"]`; point it at `screens[0].busy`.

- [ ] **Step 4: Build, test, run the suites**

```bash
cargo build && cargo test
python3 tests/cast/test_limits.py
python3 tests/cast/test_port.py
python3 tests/cast/test_cast.py
```
Expected: all ALL PASSED.

- [ ] **Step 5: Commit**

```bash
git add src/cast tests/cast
git commit -m "Tell a guest which screens there are"
```

---

### Task 5: Room audio has an owner

**Files:**
- Modify: `src/models.rs` (`AppState::audio_owner`), `src/cast/room_audio.rs`
- Modify: `tests/cast/test_audio.py`

**Interfaces:**
- Produces: `AppState::audio_owner: Arc<Mutex<Option<String>>>`, holding a display name.

- [ ] **Step 1: Add the owner**

```rust
    /// Which screen's cast currently owns the room audio, if any.
    ///
    /// The venue has one speaker pair, so audio is one resource however many
    /// screens are casting. Claimed by the first cast to turn sound on and
    /// released on that cast's teardown. Validated against that display's
    /// `is_active()` on every read, so a cast that dies without a clean teardown
    /// frees the audio by itself rather than leaving the room mute until a
    /// restart.
    pub audio_owner: Arc<Mutex<Option<String>>>,
```

- [ ] **Step 2: Claim, refuse, release**

In `control_audio`: a caster turning audio **on** claims the owner if it is free or already theirs; if another screen holds it, answer `409` with `{"error": "Ton läuft gerade auf „<label>"."}` using that display's operator label, not its internal name.

**A guest may not take audio from another guest** — that is a stranger silencing someone mid-presentation. **The operator may**, through `/api/audio`, which is the authenticated twin and already ends in the same `apply_audio`. So the refusal above applies to `/api/cast/audio` only.

Release in `deactivate_display` when the owner is this display.

On every read of the owner, drop it if that display's session is no longer `is_active()`.

- [ ] **Step 3: `caster_only` gains the screen**

`caster_only(state, peer)` is today `sender.is_some() && sender_addr == Some(peer)` against the single session. It becomes per display. Note `test_audio.py` `[31]` sends no ticket on its HTTP audio calls and asserts against source IP — keep the source-IP check as it is and scope it to the display the request names, rather than switching to tickets; switching the credential is a separate change and this task does not need it.

- [ ] **Step 4: Write the test first**

```rust
#[tokio::test]
async fn the_second_screen_is_told_who_has_the_audio() {
    let state = state_for_displays(&["foyer", "werkstatt"]).await;
    let foyer = state.display("foyer").unwrap();
    let werkstatt = state.display("werkstatt").unwrap();
    activate_display(&state, &foyer, Showing::Cast, Some("10.0.0.5".parse().unwrap())).await;
    activate_display(&state, &werkstatt, Showing::Cast, Some("10.0.0.6".parse().unwrap())).await;

    *state.audio_owner.lock().await = Some("foyer".to_string());
    assert_eq!(
        state.audio_owner.lock().await.as_deref(),
        Some("foyer"),
        "the first caster to ask holds it"
    );

    // The owner's cast ends; the audio must free itself.
    deactivate_display(&state, &foyer, "test").await;
    assert_eq!(
        *state.audio_owner.lock().await,
        None,
        "a cast that ends must release the room audio, or the next guest is mute"
    );

    // ... and the screen that never had it is unaffected either way.
    assert!(werkstatt.cast.lock().await.is_active());
}
```

- [ ] **Step 5: Run it, then the suite**

Run: `cargo test the_second_screen_is_told_who_has_the_audio -- --nocapture`
Expected: FAIL before Step 2's release logic, PASS after.

Run: `python3 tests/cast/test_audio.py` → ALL PASSED.

- [ ] **Step 6: Commit**

```bash
git add src/models.rs src/cast tests/cast/test_audio.py
git commit -m "Give the room audio one owner at a time"
```

---

### Task 6: The QR and the idle screen carry their display

**Files:**
- Modify: `src/cast/url.rs`, `src/browser.rs:302`, `src/browser.rs:883-885`, `web/empty_playlist.html`, `web/cast_display.html`, `web/cast.js`

**Interfaces:**
- Produces: `sender_url(state, display: Option<&Display>) -> String`; `empty_playlist_url(port: u16, display: &str) -> String`; `cast_display_url(port: u16, display: &str) -> String`.

- [ ] **Step 1: The guest URL can name a screen**

`sender_url` gains an optional display. With one, it appends `?screen=<name>`; without, it returns the bare root exactly as today. Bare root is what a chooser QR encodes.

- [ ] **Step 2: The idle page knows which screen it is**

`empty_playlist_url(port)` becomes `empty_playlist_url(port, display)` and appends `?screen=<name>`. Its sole call site is `src/browser.rs:302`, inside `browser_loop(state, display)`, where `display.name` is already bound at `src/browser.rs:28` and used three lines later — **no plumbing needed**.

The `already_showing` guard at `src/browser.rs:303-306` is exact string equality against the same binding, so it keeps working — but say so in the report, because a reader will wonder.

`web/empty_playlist.html` reads the parameter and calls `/api/cast/state?screen=<name>`, so the code and the invitation it draws are its own screen's.

- [ ] **Step 3: The receiver page knows which screen it is**

`cast_display_url(port)` becomes `cast_display_url(port, display)`. `web/cast_display.html` reads `?screen=` and passes it to `Cast.openSocket('display', null, {…})` — the socket URL is built at `web/cast.js:21-22`, which is the single chokepoint, so add an optional screen there and both callers benefit.

`cast_display.html:137`'s `fetch('/api/cast/state')` becomes `?screen=<name>`.

- [ ] **Step 4: Rebuild before testing anything**

Run: `cargo build`
`web/` is `include_dir!`-ed into the binary. Testing without this rebuild tests the old pages.

- [ ] **Step 5: Verify in a real browser, not by reasoning**

Start a controller with two declared screens and a headless Chrome on each (`tests/cast/test_display.py` has the exact pattern). Confirm over CDP: each screen's idle page shows its **own** screen name in the URL it fetches, and a cast pinned to one screen navigates that screen's `cast_display.html?screen=<name>`.

- [ ] **Step 6: Run the suites**

```bash
python3 tests/cast/test_cast.py
python3 tests/cast/test_overlay.py
python3 tests/cast/test_display.py
```
Expected: ALL PASSED. `test_overlay.py` matters here — it drives the idle page.

- [ ] **Step 7: Commit**

```bash
git add src/cast src/browser.rs web/
git commit -m "Point each screen's QR and idle page at itself"
```

---

### Task 7: The operator stops a cast, per screen

**Files:**
- Modify: `src/cast/mod.rs` (`routes`), `src/cast/api.rs`
- Modify: `tests/cast/test_cast.py`, `tests/cast/test_reserve.py`, `tests/cast/test_conflict.py`, `tests/cast/test_guestpage.py`

**Interfaces:**
- Produces: `DELETE /api/displays/{name}/cast/session`. Removes: `DELETE /api/cast/session`.

- [ ] **Step 1: Move the route**

Register `/api/displays/{name}/cast/session` and delete `/api/cast/session`. The new path is **operator-only** — it appears in neither `is_cast_public_path` nor `is_display_path`, which is why it may be a path segment at all.

- [ ] **Step 2: Fix the four suites**

Every case using `DELETE /api/cast/session` moves to the scoped path: `test_cast.py` `[3]`, `test_reserve.py` `[21]`, `test_conflict.py` `[12]` and `[13]`, `test_guestpage.py` `[46]`.

`test_reserve.py` `[18b]` **needs rewriting rather than patching** — its whole subject is the global per-asker `busy`, which no longer exists. Rewrite it to assert the same property per screen, and say in the report what the rewritten case now proves.

- [ ] **Step 3: Build, test, run the four suites**

```bash
cargo build && cargo test
python3 tests/cast/test_cast.py
python3 tests/cast/test_reserve.py
python3 tests/cast/test_conflict.py
python3 tests/cast/test_guestpage.py
```
Expected: ALL PASSED.

- [ ] **Step 4: Commit**

```bash
git add src/cast tests/cast
git commit -m "Stop a cast on the screen it is running on"
```

---

### Task 8: Remove `--cast-display`

**Files:**
- Modify: `src/models.rs:51-59`, `src/display.rs:84`, `src/display.rs:165`, `src/display.rs:169-184`, `src/display.rs:680-681`, `src/display.rs:686-702`

**Interfaces:**
- Consumes: Tasks 2–7, which between them removed every reader of the flag.

The flag comes out **late, not early**: until the scoped paths exist it is the only thing making casting land anywhere deliberate.

- [ ] **Step 1: Delete the flag and its check**

Remove `Args::cast_display` and its doc comment; remove both `check_cast_display` call sites and the function; remove the test `an_unknown_cast_display_fails_at_startup` and the comment referencing the flag in `the_primary_display_is_the_first_declared`.

- [ ] **Step 2: Confirm nothing references it**

Run: `grep -rn "cast_display\|cast-display\|CAST_DISPLAY" src/ tests/ web/ README.md docs/ | grep -v "^docs/superpowers/"`
Expected: no hits outside `docs/superpowers/` (the spec and this plan legitimately discuss it). Any hit in `README.md` or `docs/*.md` is Task 12's, but list them in the report so Task 12 has the list.

- [ ] **Step 3: Build, test**

Run: `cargo build 2>&1 | tail -20` → clean, and specifically **no** unused-field or dead-code warning, which would mean something was left half-removed.
Run: `cargo test` → pass.

- [ ] **Step 4: Commit**

```bash
git add src/models.rs src/display.rs
git commit -m "Remove the flag that pinned casting to one screen"
```

---

### Task 9: The QR target setting

**Files:**
- Modify: `src/settings.rs`, `src/cast/url.rs`, `web/admin.html`
- Modify: `tests/cast/test_settings.py`

**Interfaces:**
- Produces: `AppSettings::cast_qr_target: CastQrTarget` (`Screen` | `Chooser`), default `Screen`.

- [ ] **Step 1: Add the setting**

```rust
/// What the QR drawn on a screen points at.
///
/// One setting for the venue, read by both drawers -- the overlay QR and the
/// idle page's invitation -- so the two cannot disagree. Not part of the overlay
/// configuration, because the idle page draws its own invitation without
/// consulting it and would need the same rule a second time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CastQrTarget {
    /// `…/?screen=<name>` -- the screen the guest is standing in front of.
    #[default]
    Screen,
    /// the bare guest URL; the page offers the list.
    Chooser,
}
```

Add `pub cast_qr_target: CastQrTarget` to `AppSettings` beside `cast_auth`. Follow the existing settings pattern exactly — stored, operator-editable, **no CLI flag** (this is a UX choice, not a capability; `--disable-cast` and `--guest-pages` are capabilities). Persisted, defaulted and surfaced like its neighbours; read through `state.settings`, never `state.args`.

- [ ] **Step 2: Both drawers read it**

The overlay QR (`settings::overlay_payload`, which became display-aware in Task 2) and the idle page's invitation both resolve `sender_url` with `Some(display)` under `Screen` and `None` under `Chooser`. **One setting, both readers** — that is why it is not in the overlay config, where the idle page would have to know the rule a second time.

With exactly one declared display the setting has no effect: both values produce a URL that lands on the only screen. The admin control says so rather than pretending otherwise.

- [ ] **Step 3: The admin control**

In `web/admin.html`'s cast card (`div#castCard` at line 73, settings at `div#castSettings` line 90), add the radio pair labelled **QR-Code auf dem Schirm führt zu** with options **diesem Schirm** and **Auswahl aller Schirme**, disabled with a note when `/api/displays` returns one declared screen.

- [ ] **Step 4: Rebuild and verify in a browser**

Run: `cargo build`, then drive `admin.html` over CDP: the control saves, survives a reload, and is disabled on a single-display controller.

- [ ] **Step 5: Run the suite**

Run: `python3 tests/cast/test_settings.py` → ALL PASSED, with a new case asserting the setting round-trips.

- [ ] **Step 6: Commit**

```bash
git add src/settings.rs src/cast web/admin.html tests/cast/test_settings.py
git commit -m "Let the operator choose what the screen QR points at"
```

---

### Task 10: The guest page binds, chooses and switches

**Files:**
- Modify: `web/index.html`, `web/cast.js`

**Interfaces:**
- Consumes: `/api/cast/info`'s `screens` list (Task 4), `claim`'s `display` field (Task 3).

- [ ] **Step 1: Read the screen from the URL**

`index.html` has **no** notion of a query parameter today — a grep for `location.search`/`URLSearchParams` across `web/` hits only `pdf_viewer.html` and `playlist.html`. Add it in the bootstrap (the top-level block at `web/index.html:762-801`, which is not a named function): read `?screen=`, validate it against the `screens` list from `/api/cast/info`, and hold it in one variable that every `claim`/`pair` call reads.

- [ ] **Step 2: Three entry states, one document**

- **Bound** (valid `?screen=`): header reads *„Sie senden an: <label>"*, share button one tap away.
- **Chooser** (no `?screen=`, several screens): the list, each entry `frei`/`belegt`, busy ones not selectable.
- **Gone** (a `?screen=` naming a screen that is not declared): the chooser plus *„Diesen Bildschirm gibt es nicht mehr."* — not a 404. The guest is holding a phone.

With exactly one declared screen there is no chooser: the single screen resolves, per `display::resolve`.

- [ ] **Step 3: The switcher**

A `<select>` in the header, listing labels with their state, polled from `/api/cast/info` on the existing bootstrap cadence. Picking another screen re-claims against it.

**Disabled once a sender is streaming**, with a note saying to stop first. A live switch means tearing down one `RTCPeerConnection` and negotiating another while the first screen's override unwinds.

- [ ] **Step 4: Send the screen everywhere it is now required**

`claim` (`web/index.html:290`, `:315` for the DELETE), `pair` (`:664` — which posts **no body** today and must now post `{display}`; this is where Task 3's `Option<Json<…>>` tolerance is removed), and `Cast.openSocket` (`web/cast.js:19`, URL built at `:21-22`).

- [ ] **Step 5: Remove the tolerance added in Task 3**

`start_pairing` takes `Json<PairRequest>` outright. Confirm `index.html` sends a body on every path before doing this.

- [ ] **Step 6: Rebuild, then verify in a real browser**

Run: `cargo build`. Then, with two declared screens and a Chrome each, drive the guest page over CDP and observe — observations, not expectations — that: a `?screen=werkstatt` URL binds and names Werkstatt; the bare URL shows both screens with correct `frei`/`belegt`; claiming a busy screen is refused with the server's message; `?screen=kueche` shows the chooser with the "gibt es nicht mehr" note; and the switcher is disabled while streaming.

- [ ] **Step 7: Run the suites**

```bash
python3 tests/cast/test_cast.py
python3 tests/cast/test_pairing.py
python3 tests/cast/test_guestpage.py
python3 tests/cast/test_browser.py
```
Expected: ALL PASSED. `test_browser.py` is the one with real WebRTC between two Chromes.

- [ ] **Step 8: Commit**

```bash
git add web/index.html web/cast.js src/cast
git commit -m "Let a guest pick their screen on the guest page"
```

---

### Task 11: The admin page reports and stops each cast

**Files:**
- Modify: `web/admin.html`

- [ ] **Step 1: A cast line per screen**

The per-screen rows added by the multi-display work gain: who is casting (the sender address), since when, what is showing, and a **Beenden** button calling `DELETE /api/displays/{name}/cast/session`.

**This is new capability, not a port.** `button#castStop` (`web/admin.html:83`) has existed since the page was written and has never had a handler; nothing in `web/` has ever called `DELETE /api/cast/session`. Remove the dead button as part of this task.

- [ ] **Step 2: A failed fetch must not read as an affirmative**

The rule the multi-display branch spent a commit on: a non-OK response says *„Status unbekannt"*, never *„niemand überträgt"*. Follow `admin.html`'s existing `screenLine`/`itemsOf` shapes, which already do this.

- [ ] **Step 3: Rebuild and verify in a browser**

Run: `cargo build`. With two screens and a cast running on one, confirm over CDP: both rows render, the casting one names its sender, **Beenden** ends that cast and leaves the other running, and stopping the controller shows *„Controller nicht erreichbar"*.

- [ ] **Step 4: Commit**

```bash
git add web/admin.html
git commit -m "Show and stop each screen's cast from the admin page"
```

---

### Task 12: The integration suite

**Files:**
- Create: `tests/cast/test_castscreens.py`

Follow `tests/cast/test_display.py` closely — same harness shape: declared screens with explicit CDP ports, one profile directory per screen, `atexit` cleanup, `LONG = 600` on any item whose id is asserted, and **a positive barrier before every negative assertion**.

Cases `[80]`–`[89]`, continuing after `test_display.py`'s `[77]`:

- `[80]` two senders cast to two screens simultaneously; each screen's override is its own and neither disturbs the other
- `[81]` a ticket minted for `foyer` is refused on a socket claiming `werkstatt`
- `[82]` claiming a busy screen is refused; claiming the other succeeds in the same breath
- `[83]` `/api/cast/info`'s `busy` flags flip as sessions come and go, and `max_edge` is per screen
- `[84]` room audio: first caster holds it, the second is told who has it, teardown releases it, the operator can take it through `/api/audio`
- `[85]` pairing codes are per screen and unique across screens while alive
- `[86]` a cast on one screen leaves the other screen's overlay, playlist and current item untouched — the negative that matters most
- `[87]` `?screen=` on the idle page: each screen shows its own code
- `[88]` an unscoped `claim` resolves with one declared screen and `409`s with two
- `[89]` `cast_enabled` switched off ends **every** session

- [ ] **Step 1: Write the suite**
- [ ] **Step 2: Run it** — `python3 tests/cast/test_castscreens.py`, iterate until green. **If a case fails because the code is wrong rather than the test, stop and report it.**
- [ ] **Step 3: Mutation-check `[80]` and `[86]`** — make `activate_display` ignore its display argument and use `state.displays[0]`; confirm both fail; restore; quote the output.
- [ ] **Step 4: Run every neighbour**

```bash
python3 tests/cast/test_cast.py
python3 tests/cast/test_pairing.py
python3 tests/cast/test_reserve.py
python3 tests/cast/test_conflict.py
python3 tests/cast/test_guestpage.py
python3 tests/cast/test_limits.py
python3 tests/cast/test_audio.py
python3 tests/cast/test_display.py
python3 tests/cast/test_overlay.py
python3 tests/cast/test_settings.py
```

- [ ] **Step 5: Commit**

```bash
git add tests/cast/test_castscreens.py
git commit -m "Test two screens casting at once"
```

---

### Task 13: Documentation

**Files:**
- Modify: `README.md`, `docs/casting.md`, `docs/features.md`, `docs/architecture.md`, `docs/deployment.md`, `docs/troubleshooting.md`, `CLAUDE.md`, `tests/cast/README.md`
- Modify: `docs/superpowers/specs/2026-09-16-per-display-casting-design.md` (Status)

- [ ] **Step 1: `docs/casting.md`** — rewrite *Which screen a cast lands on* around per-screen sessions; delete the "what does not follow it yet" list, which this feature closes; document the chooser, the switcher and what `/api/cast/info` tells an unauthenticated guest.
- [ ] **Step 2: `README.md`** — remove `--cast-display` from the flag list (Task 8 reported the exact hits); add the new and changed routes.
- [ ] **Step 3: `CLAUDE.md`** — a `## Casting across screens` subsection holding the rules that cost something to learn: the session lives on `Display`; `attempts` deliberately does not; the screen is a parameter on exempt routes and a path segment only on authenticated ones, and why; audio has one owner and a guest may not take it from a guest; `display::resolve` is the one resolver.
- [ ] **Step 4: `docs/troubleshooting.md`** — a guest scanning a screen whose QR points at the chooser; a cast landing on the wrong screen (the QR target setting); audio refused because another screen holds it.
- [ ] **Step 5: `tests/cast/README.md`** — add `test_castscreens.py` with its ports.
- [ ] **Step 6: Mark the spec implemented.**
- [ ] **Step 7: Commit**

```bash
git add README.md docs CLAUDE.md tests/cast/README.md
git commit -m "Document casting to a screen you choose"
```

---

## Notes carried from the inventory

Two pre-existing defects found while surveying, neither caused by this work and neither in scope. Fix them only if a task touches the line anyway, and mention them in that task's report:

- `tests/cast/test_cast.py`'s `Server` does not default `--guest-pages off`, although `CLAUDE.md` says the harness does.
- `tests/cast/test_limits.py` `[31]` and `tests/cast/test_audio.py` `[31]` share a case number.
