# Native cast receivers (Chromecast/AirPlay) inside the controller

**Status:** draft (revised after architecture review)
**Date:** 2026-10-26

## What and why

The controller already casts: a sender runs a web page that speaks our WebRTC
signaling, and the display browser renders it (`src/cast/`, "a cast is an
override"). That reaches only senders that run our page.

`casttarget` is a separate Rust binary that is a **real Chromecast receiver** —
it announces itself over mDNS as a Google Cast device, terminates TLS on 8009,
speaks CASTV2 with genuine Google cast certificates, receives RTP, and plays the
elementary streams in **external `mpv`/`ffplay` windows**. Its sender is anything
with a native cast button: a phone's YouTube app, Chrome's "cast this tab",
Android screen mirroring. AirPlay and Miracast are the same shape and are on its
roadmap ("Chromecast first, AirPlay/Miracast later").

The two do not overlap — they are complementary. This spec folds `casttarget`
into the controller as an **optional library dependency**, reusing the
controller's per-display session, override, access and display framework, so a
native cast lands on a chosen screen and obeys the per-screen rules it can.
The rendering stays external (the stream cannot go through the browser); only the
framework is shared.

## Decisions taken

1. **`casttarget` becomes a library crate plus a thin optional binary.** The
   standalone binary keeps building and testing on a desktop; the controller
   depends on the library. Changes to `casttarget` are kept **as small as the
   library extraction allows** (the review found this is more than a handful of
   lines — see "What changes in casttarget").

2. **The library is a git submodule of this repository**
   (`miniclientcontrol/casttarget/`), a path dependency behind a Cargo feature
   `native-cast`, **default off**. The feature pulls the receiver-only
   dependencies (crypto, `rusqlite`, the mDNS responder) only when built. The
   `winit`/`softbuffer`/`fontdue`/`ureq` info-window UI deps stay on
   `casttarget`'s own `[[bin]]`, never in the library, so the controller never
   links them.

3. **A `CastSource` trait is the protocol boundary.** Chromecast is the first
   implementation; AirPlay and Miracast are each one more `impl`, and nothing on
   the controller side changes to add one.

4. **The library runs the whole receive→player path; the controller
   orchestrates.** The library does signaling, RTP, decode, and spawns
   `mpv`/`ffplay` — with the Wayland `app_id` the controller hands it per display.
   It exposes exactly two things to the controller: an **access hook** (asked
   before a sender is accepted) and an **event channel**
   (`CastStarted{app_id}` / `CastEnded`). The controller decides access, installs
   the override, and manages lifecycle.

5. **A native cast is a new override kind: an external window.** The existing
   override pins a browser URL; this one installs no URL. On `CastStarted` the
   controller freezes the display's Chromium (below) and relies on a standing sway
   rule to bring the player window fullscreen in front of it; on `CastEnded` it
   unfreezes and the player window is gone. It slots into `run_override_loop`
   where `cast_display.html` does, and restores the previous override on teardown
   only if the one on screen is still ours — the rule the WebRTC cast follows.

6. **Chromium is frozen during a native cast, not closed.**
   `Page.setWebLifecycleState("frozen")` on the display's page, reversible with
   `"active"` with page state intact. Closing it would blank the screen for
   seconds on the restart and fights `chromium::supervise`'s "never kill the
   browser" rule. **This is the one decision resting on an unproven premise** (see
   Risks) and is coordinated with `src/freeze.rs` (below).

7. **One receiver per declared display, one stream per screen for now.** The mDNS
   friendly name comes from the display's `label` ("Foyer", "Werkstatt"), the TLS
   port is `8009 + declaration index`, the device id is derived per display. All
   receivers **share one `cast.db`** / one Google identity, differing only in mDNS
   name and port (see Risks for the limits of this). `casttarget` holds **one
   session per receiver** today; several simultaneous casts on one screen are
   **out of scope now** but must not be designed out — the override models "this
   display's cast window(s)", singular today, a tiled set later, and the sway rule
   is the only piece that would change (fullscreen → tiled).

8. **Native-cast access is `anyone`/`off` only; `account` is treated as `off`.**
   A native sender (phone, Chrome tab) presents no mc account or SSO session
   (`SenderInfo` carries a sender id and IP, nothing more), so `cast::access`'s
   `account` mode — which requires a signed-in caster — can never pass for a
   native cast. A screen set to `account` therefore refuses native casts, exactly
   as `off` does. This is a **deliberate narrowing, documented as such, not
   parity.** The access hook is shaped so a later **grant check** can be added
   without changing its callers (below).

9. **`cast.db` on the USB data partition**, like `miniclient.db`, read through
   `rusqlite` **kept isolated in the feature** — a read-mostly certificate store,
   no sqlx migration. The 30-day refetch runs on **one** receiver only; the
   controller coordinates it.

## Architecture

### Crate layout

```
casttarget/                     (submodule)
  src/lib.rs                    NEW: exports the cast protocol + Receiver API;
                                owns AppState/SessionInfo/StreamStats (moved from main.rs)
  src/cast/…                    protocol modules, made pub as the lib API needs
  src/certstore.rs              unchanged
  src/main.rs                   thin: parse args, build Receiver, run UI
  src/ui.rs, fakesender.rs      binary-only (not in lib)
  Cargo.toml                    [lib] + [[bin]]; winit/softbuffer/fontdue/ureq on the bin only
```

The library's entry point:

```rust
pub struct ReceiverConfig {
    pub name: String,           // mDNS friendly name, from display label
    pub port: u16,              // 8009 + index
    pub device_id: String,      // injected per display (not derived)
    pub app_id: String,         // Wayland app_id for the player window(s)
    pub cast_db: PathBuf,
    pub fetch: bool,            // only one receiver gets true
}

pub enum CastEvent { Started { app_id: String }, Ended }

pub trait AccessHook: Send + Sync {
    /// Asked before a sender's LAUNCH is accepted. Blocking is fine — it runs on
    /// the receiver's own std::thread, never on a tokio worker.
    fn may_cast(&self, sender: &SenderInfo) -> bool;
}

/// Starts the receiver on its own threads. Dropping the handle stops it.
pub fn start(cfg: ReceiverConfig,
             access: Arc<dyn AccessHook>,
             events: std::sync::mpsc::SyncSender<CastEvent>) -> Result<ReceiverHandle>;
```

### The controller side (`src/native_cast/`, new, feature-gated)

- **`native_cast::supervise`** starts one `casttarget::start` per declared
  display: `display.label` → name, index → port and derived `device_id`, and
  `app_id = casttarget-<display-name>` (the naming the multi-display glue already
  binds in sway). `display.name` is the identity used for the app_id and output;
  `display.label` is only the human-facing mDNS name.
- The **access hook** calls into `cast::access` via
  `tokio::runtime::Handle::block_on` — safe because `casttarget` has **no tokio**
  and the hook runs on its own `std::thread`, so there is no "block_on inside a
  runtime" panic and no worker starvation. Today it returns `anyone`/`off`; the
  hook is the single place a future grant check is added.
- The **event channel** is a bounded `std::sync::mpsc::SyncSender`, drained by a
  tokio task (`spawn_blocking` on `recv`), drop-to-log when full — the webhook
  dispatcher's model, so a slow consumer never blocks the receiver thread.

### The external-window override

`CastStarted` →
1. `cast::access` already said yes (the hook gated the sender).
2. Install an external-window override on the `Display`. **`OverrideItem` is a
   struct today, not an enum** (`models.rs`), and is `PartialEq` — add the kind as
   a field or refactor to an enum, preserving the `PartialEq` the loop's
   "skip-if-unchanged" and "restore-only-if-ours" depend on. `run_override_loop`
   learns one branch: for an external-window override it does **not**
   `navigate_page`/`start_scrolling`; it freezes the page and parks on the same
   `select!` slots.
3. Freeze: `Page.setWebLifecycleState("frozen")` on `Display::screen_page`, and
   set the display's "painting intentionally suspended" flag.

`CastEnded` → `setWebLifecycleState("active")`, clear the flag, clear the override
(restore the previous one only if still ours).

### Window placement (sway, in kiosk-pxe)

The player window carries `app_id=casttarget-<display>`. A **standing rule** in
`kiosk-session` (kiosk-pxe repo, beside the multi-display rules):

```
for_window [app_id="casttarget-<display>"] move container to output <out>, fullscreen enable
```

Window appears → sway puts it fullscreen on the right output, in front of the
frozen Chromium; placement stays the window manager's job, no runtime `swaymsg`
from the controller. On cast end the window closes and Chromium is visible again.
(When multi-cast-per-screen lands later, this one rule changes from fullscreen to
a tiled container — nothing else.)

### Freeze coordination (`src/freeze.rs`)

`freeze::watch` is spawned per display and already treats `Reading::Nothing` as
**no evidence** of a freeze (never as a freeze). The external-window override sets
a per-`Display` "painting intentionally suspended" flag that the watcher reads,
before `read_frames`, as `Nothing`. Exactly the existing shape; cleared on cast
end. This keeps a deliberately frozen screen from being restarted mid-cast.

### Data and fetch

`cast.db` resolved from the data dir (USB), passed to every receiver. One receiver
gets `fetch: true`; the 30-day refetch writes the shared file, the others reload
identity on the daily-cert rollover they already handle. `rusqlite` compiled only
under `native-cast`.

## What changes in `casttarget` (the real touch points)

The review corrected the earlier "six small bullets". The honest list:

1. **`src/lib.rs`** with the `start`/`ReceiverConfig`/`CastEvent`/`AccessHook`
   API. **`AppState`/`SessionInfo`/`StreamStats` move from the binary crate root
   (`main.rs`) into the library**, since the whole protocol stack
   (`session.rs`, `webrtc.rs`) writes them; `start()` owns an internal `AppState`
   the mc path never reads (it consumes `CastEvent`s). This is the largest part,
   not a rename.
2. **Thread `app_id` to every player spawn site.** `Player::spawn`,
   `TsPlayer::spawn`, `AudioPlayer::spawn` take no app_id today; it must flow
   `ReceiverConfig.app_id` → the lib's `AppState`/device facts →
   `webrtc::handle_webrtc` → each spawn. `mpv --wayland-app-id=<id>` for the mpv
   paths.
3. **The TS/Android-mirror path uses `ffplay`, which has no `--wayland-app-id`.**
   The per-display sway rule then fails for Android screen mirroring. Resolve by
   routing the TS path through `mpv` too (it already handles mpegts) or a thin
   wrapper — **decided in the per-display sub-project**, not left open.
4. **`device_id` becomes injectable.** `stable_device_id(cert+hostname)` is
   identical for all receivers today; change `Service::start`/`stable_device_id`
   to take `ReceiverConfig.device_id`.
5. **Refuse at `LAUNCH`.** `handle_receiver_ns` must consult the access hook and,
   on refusal, **not** create the session and return an idle/error
   `RECEIVER_STATUS` — a defined behavioural change, not just a hook call.
6. **`Cargo.toml`:** declare `[lib]` and `[[bin]]`; move the UI deps to the bin.

The CASTV2 auth, RTP/RTCP, jitter, TS demux and mDNS responder are used unchanged.

## Not in scope (but not designed out)

- **Several simultaneous casts on one screen.** One session per receiver today;
  the override models "this display's cast window(s)" and the sway rule is the
  only change when it lands.
- **`account`-mode native casts / the grant flow.** A later feature: a signed-in
  person temporarily enables native casting on a screen (time-window grant, **not**
  IP-whitelist — login device and cast device routinely differ, phone casts while
  the laptop is signed in). The access hook is the single seam it plugs into.
- **AirPlay/Miracast implementations.** The `CastSource` trait is shaped for them.
- **Closing or CPU-throttling Chromium.** Freeze is the policy.
- **Per-display cast *identity* separation** (a `cast.db` per screen).

## Risks (verify before/early in build)

- **Freeze dropping CPU/GPU on a foreground fullscreen tab is unproven and the
  whole of decision 6 rests on it.** `setWebLifecycleState("frozen")` forces the
  lifecycle state, but a frozen page keeps its last composited frame and a
  Wayland-occluded Chromium surface may get no occlusion signal — the plausible
  outcome is "timers/JS stop, GPU/compositor savings do not". **Measure this first
  in sub-project 3**; if it does not free resources, the fallback is "verdecken
  only" (no freeze, accept the running cost) — the architecture does not change,
  only the policy.
- **Sharing one Google device cert across receivers.** `daily_cert_der` and the
  Google-signed `device_cert_der` are **identical on every port**; only mDNS
  `id`/`fn`, `ssdp_udn`, `cloud_device_id` differ. `casttarget`'s own comment
  (`session.rs`) warns GMS caches device state by identity and that identity churn
  makes devices "disappear" — two LAN endpoints presenting the same device cert is
  that collision, for Android system mirroring / Google Home. Casual YouTube /
  Chrome-tab senders key on mDNS and are expected fine. **Verify against a real
  Android/Home sender, not only a Chrome tab**, in the per-display sub-project; if
  it collides, fall back to a derived-per-display cert or one cast.db per screen.
- **`rusqlite` + `sqlx` link.** Fine **today**: both resolve `libsqlite3-sys` to
  **0.30.1** (mc via `sqlx-sqlite`, casttarget via `rusqlite`), Cargo dedupes to
  one `links = "sqlite3"` package, single binary links clean. The real guard is
  **pin-awareness**: a future `sqlx`/`rusqlite` bump that moves `libsqlite3-sys` to
  a different major is an immediate hard link-collision. Note it where the
  dependency is declared; it is not a cross-build wrinkle.
- **sway z-order:** two fullscreen windows on one output — the newly mapped player
  must land above the frozen Chromium (last-focused wins, expected); confirm on a
  two-output device.

## Decomposition

Four independently-testable sub-projects, in order. Each is its own plan.

1. **casttarget library extraction.** `lib.rs`; move `AppState`/`SessionInfo`/
   `StreamStats` into the lib; `[[bin]]` keeps the UI deps; `Service::start`
   accepts injected `device_id` and `app_id`. **Gate:** standalone binary +
   `scripts/ui-test.sh` still green; the lib builds with no UI deps.
2. **mc feature-gated `native_cast::supervise`.** The `block_on` access hook into
   `cast::access`, the bounded event channel, one receiver per declared display.
   **Gate:** feature **off** compiles/links and changes nothing (every existing
   `tests/cast/` suite passes); feature **on** announces one receiver per declared
   display with the display's label; the hook refuses on `off`/`account`, accepts
   on `anyone`.
3. **ExternalWindow override + freeze coordination.** The `run_override_loop`
   branch, the suppress-freeze flag on `Display`, the `setWebLifecycleState`
   calls. **Gate:** simulated `CastStarted`/`CastEnded` installs/tears down the
   override; `freeze.rs` does not restart a deliberately frozen screen; **the
   freeze-frees-resources measurement from Risks is done here.**
4. **Per-display naming/placement.** app_id threading to the spawn sites, the
   ffplay→mpv decision for the TS path, the `kiosk-session` sway rule, `cast.db` on
   the data partition, the shared-cert verification against a real Android sender.
   **Gate:** a native cast from a real device lands fullscreen on its chosen
   screen in front of the frozen Chromium, and ends clean.

## Testing

- `casttarget` standalone keeps `scripts/ui-test.sh` and builds as a binary — the
  extraction must not regress it.
- Controller, feature **off**: links none of it; existing `tests/cast/` unchanged.
- Controller, feature **on**, `tests/cast/` style: a receiver per declared display
  named by label; the hook refuses on `off`/`account`, accepts on `anyone`; a
  simulated `CastStarted` installs the override and freezes the page, `CastEnded`
  restores it; `freeze.rs` does not restart a frozen screen. `casttarget`'s
  `--fakesender` stands in where a real cast device is absent.
- Cross-compile under `native-cast` for armv7 (`cross build`).

## Documentation to update (mc)

- `CLAUDE.md` — a native-cast section: the external-window override, the freeze
  coordination, the per-display receiver naming, the feature gate, the
  `account`-is-`off` narrowing.
- `docs/casting.md` — native receivers beside the WebRTC relay.
- `docs/deployment.md` — the `native-cast` feature, `cast.db` on the data
  partition, the kiosk-pxe sway rule.
- kiosk-pxe: the `kiosk-session` sway rule for `casttarget-<display>`, and
  `cast.db` placement on the data partition.
