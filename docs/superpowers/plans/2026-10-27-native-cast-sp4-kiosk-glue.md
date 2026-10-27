# Native-Cast SP4: Player app_id + kiosk-pxe glue — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the per-display casttarget player window land fullscreen on the right output, in front of the frozen Chromium, on the real kiosk image — and build the image with native-cast on.

**Architecture:** Two repos. In **casttarget** (submodule), thread the receiver's `app_id` into the player spawn sites so the player window actually carries `app_id=casttarget-<display>` — the mpv video path gets `--wayland-app-id`, the ffplay TS path gets `SDL_APP_ID` (best-effort). In **kiosk-pxe**, add the standing sway `for_window` rule for `casttarget-<display>` beside the existing `miniclientcontrol-<display>` rules, add `mpv`/`ffmpeg` to the package set, turn on `--features native-cast` in `build.sh`, and bump the submodule pointer.

**Tech Stack:** Rust (casttarget), bash + archiso (kiosk-pxe), sway 1.12 / Wayland.

## Global Constraints

- casttarget player windows must carry `app_id=casttarget-<display>` or the sway rule never matches. This is the whole point of SP4.
- `cast.db` needs **no** new flag: `Args::cast_db_path()` already defaults to `cast.db` beside `--database-path`, i.e. `${DATA}/cast.db` on the USB data partition. Do not add `--cast-db` to kiosk-session.
- The implicit single-display name is `default` (`display.rs:76`), so its window is `casttarget-default`.
- Commit messages carry **no** `Co-Authored-By: Claude` / `Claude-Session` trailer (user global CLAUDE.md).
- Remote git goes through `~/.local/bin/git-wrapper.sh` (YubiKey). The miniclientcontrol remote (git.supercookie.de) also needs `GIT_SSH_COMMAND="ssh -4"`; the casttarget remote (git.fablab-luebeck.de) does not.
- The ffplay TS path stays ffplay — mpv was tried and rejected for the live TS pipe (audio-start crash, `player.rs:373-375`). Do not convert it to mpv.

---

### Task 1: mpv video player carries the window app_id (casttarget)

**Files:**
- Modify: `casttarget/src/cast/player.rs` (`Player::spawn`, ~54-78)
- Modify: `casttarget/src/cast/webrtc.rs` (call site, ~801)
- Test: `casttarget/src/cast/player.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces: `Player::spawn(codec: Codec, width: u16, height: u16, app_id: &str)`; a pure `fn mpv_video_args(fmt: &str, app_id: &str) -> Vec<String>`.
- Consumes: the **window** app_id, which is `AppState::app_id` (set from `ReceiverConfig.app_id = "casttarget-<name>"`, `lib.rs:122`, origin `native_cast/mod.rs`). **NOT `session.app_id`** — that field (`session.rs:332`) is the Chromecast LAUNCH appId (`"0F5096E5"` etc.) and would make the sway rule never match. `webrtc.rs:372` already clones the right one (`s.app_id`) for `CastEvent::Started`; capture the same value for the window.

- [ ] **Step 1: Write the failing test** — append to `player.rs`:

```rust
#[cfg(test)]
mod app_id_tests {
    use super::*;

    #[test]
    fn mpv_video_args_carry_app_id() {
        let args = mpv_video_args("h264", "casttarget-HDMI-A-1");
        assert!(args.iter().any(|a| a == "--wayland-app-id=casttarget-HDMI-A-1"),
                "mpv args must set the wayland app_id: {args:?}");
        assert!(args.iter().any(|a| a == "--demuxer-lavf-format=h264"));
        assert_eq!(args.last().map(String::as_str), Some("-"));
    }
}
```

- [ ] **Step 2: Run it, expect failure** — Run: `cargo test -p casttarget mpv_video_args_carry_app_id`. Expected: FAIL (`mpv_video_args` not found).

- [ ] **Step 3: Extract the arg builder and thread app_id.** Replace the body of `Player::spawn` so it uses a pure builder, and add the builder above `impl Player`:

```rust
/// mpv args for the live video window. `app_id` becomes the Wayland app_id the
/// sway rule matches (`casttarget-<display>`), placing the window fullscreen on
/// the display in front of the frozen Chromium.
fn mpv_video_args(fmt: &str, app_id: &str) -> Vec<String> {
    vec![
        "--no-config".into(),
        "--profile=low-latency".into(),
        "--no-cache".into(),
        "--untimed".into(),
        "--really-quiet".into(),
        format!("--demuxer-lavf-format={fmt}"),
        format!("--wayland-app-id={app_id}"),
        "--title=CastTarget".into(),
        "--keep-open=no".into(),
        "-".into(),
    ]
}
```

And in `Player::spawn`:

```rust
    pub fn spawn(codec: Codec, width: u16, height: u16, app_id: &str) -> std::io::Result<Player> {
        // mpv statt ffplay: --untimed zeigt Frames sofort beim Dekodieren
        // (keine eigene Puffer-Queue, keine fps-Schaetzung) -> Latenz = nur
        // unser Playout-Puffer, konstant.
        let fmt = codec.ffplay_fmt();
        let mut child = Command::new("mpv")
            .args(mpv_video_args(fmt, app_id))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin piped");
        Ok(Player { child, stdin, codec, wrote_header: false, frames: 0, width, height })
    }
```

- [ ] **Step 4: Capture the window app_id once.** In `start_rtp_receiver` (`webrtc.rs`), the existing lock block (~361-373) locks `state` as `s` and already clones `s.app_id` for the event. Bind the window app_id out of that block so it is in scope at the spawn sites below:

```rust
    let window_app_id;
    {
        let mut s = state.lock().unwrap();
        s.stream = Some(StreamStats {
            app_id: session.app_id.clone(),
            started: Instant::now(),
            packets: 0,
            bytes: 0,
            last_rate_bps: 0.0,
            last_activity: Instant::now(),
        });
        // mc erfaehrt den Cast-Start; app_id ist das Fenster-app_id des Displays.
        let _ = s.events.try_send(crate::CastEvent::Started { app_id: s.app_id.clone() });
        window_app_id = s.app_id.clone();
    }
```

(`StreamStats.app_id` stays `session.app_id` — it is only the cosmetic UI/stats label; the *window* uses `window_app_id`.)

- [ ] **Step 5: Update the video spawn call site** — `webrtc.rs` ~801:

```rust
                            .and_then(|c| Player::spawn(c, vs.width, vs.height, &window_app_id))
```

- [ ] **Step 6: Run test + build** — Run: `cargo test -p casttarget mpv_video_args_carry_app_id && cargo build`. Expected: PASS, build OK. (The builder test proves the arg shape; the *call-site* wiring — that `window_app_id`, not `session.app_id`, reaches the builder — is not unit-coverable here and is the thing to eyeball, verified on device.)

- [ ] **Step 7: Commit**

```bash
git -C casttarget add src/cast/player.rs src/cast/webrtc.rs
git -C casttarget commit -m "cast: set the mpv window's wayland app_id from the receiver app_id

The per-display sway rule matches app_id casttarget-<display>; without
it on the player window the rule never fires and the stream opens as a
floating window instead of fullscreen on its display."
```

---

### Task 2: ffplay TS player carries app_id via SDL_APP_ID (casttarget)

**Files:**
- Modify: `casttarget/src/cast/player.rs` (`TsPlayer::spawn`, ~372)
- Modify: `casttarget/src/cast/webrtc.rs` (call site, ~756)

**Interfaces:**
- Produces: `TsPlayer::spawn(app_id: &str)`.

Rationale: ffplay is SDL2. SDL ≥ 2.0.18 reads the Wayland `xdg_toplevel` app_id from the `SDL_APP_ID` env var. This is best-effort — on an older SDL it is a harmless no-op and the Android-mirror window falls back to floating (verified on device). The WebRTC/mpv path (Task 1) is the primary cast path and is not affected.

- [ ] **Step 1: Thread app_id and set the env** — `TsPlayer::spawn`:

```rust
    pub fn spawn(app_id: &str) -> std::io::Result<TsPlayer> {
        // ffplay statt mpv fuer den TS-Live-Pfad (mpv crasht beim Audio-Start
        // ueber die Live-Pipe). SDL>=2.0.18 nimmt die Wayland-app_id aus
        // SDL_APP_ID -> dieselbe sway-Regel wie der mpv-Pfad.
        let mut child = Command::new("ffplay")
            .env("SDL_APP_ID", app_id)
            .args([
```

(leave the rest of the arg list and the `.args(if ... STATS ...)` block unchanged).

- [ ] **Step 2: Update the call site** — `webrtc.rs` ~756, using the same `window_app_id` captured in Task 1 (not `session.app_id`):

```rust
                            match TsPlayer::spawn(&window_app_id) {
```

- [ ] **Step 3: Build + full test** — Run: `cargo build && cargo test --lib`. Expected: build OK, all tests pass (the suite was 28 passing).

- [ ] **Step 4: Commit**

```bash
git -C casttarget add src/cast/player.rs src/cast/webrtc.rs
git -C casttarget commit -m "cast: set the ffplay TS window's app_id via SDL_APP_ID

Android screen-mirror runs through the ffplay TS path; SDL>=2.0.18 picks
up the Wayland app_id from SDL_APP_ID so the same per-display sway rule
places it. Best-effort: a no-op on older SDL."
```

---

### Task 3: Push casttarget, bump the mc submodule, rebuild mc

**Files:**
- Modify: `miniclientcontrol/casttarget` (gitlink), `miniclientcontrol` superproject index

- [ ] **Step 1: Push casttarget**

```bash
~/.local/bin/git-wrapper.sh -C casttarget push origin main
```

- [ ] **Step 2: Rebuild mc with the feature to prove the submodule still links**

```bash
cargo build --features native-cast   # from miniclientcontrol/
```

Expected: `Finished`.

- [ ] **Step 3: Commit the submodule bump in miniclientcontrol**

```bash
git add casttarget
git commit -m "native-cast: bump casttarget to the player app_id fix"
```

- [ ] **Step 4: Push miniclientcontrol**

```bash
GIT_SSH_COMMAND="ssh -4" ~/.local/bin/git-wrapper.sh -C . push origin master
```

---

### Task 4: Standing sway rule for the casttarget window (kiosk-pxe)

**Files:**
- Modify: `kiosk-pxe/airootfs/usr/local/bin/kiosk-session` (discovery loop ~80-84, single-display branch ~85-87)

The multi-display loop already emits `for_window [app_id="miniclientcontrol-${out}"] move container to output ${out}`. Add the casttarget rule beside it — **with** `fullscreen enable`, because the player is not Chromium `--kiosk` and does not race the fullscreen watcher.

- [ ] **Step 1: Add the rule in the per-connector loop** — after the `miniclientcontrol-${out}` `swaymsg` line:

```bash
        swaymsg "for_window [app_id=\"casttarget-${out}\"] move container to output ${out}, fullscreen enable"
```

- [ ] **Step 2: Add the single-display rule** — inside the existing `if (( ${#display_flags[@]} == 0 ))` block (~85-87), the legacy path where mc runs without `--display` and serves one implicit display named `default` (`display.rs:76`, so its receiver window is `casttarget-default`). Add:

```bash
    swaymsg 'for_window [app_id="casttarget-default"] fullscreen enable'
```

(Verified in scope: `native_cast::supervise` receives `display::configure`'s output, which in legacy mode is a single `DisplayConfig{ name: "default" }` — one receiver starts, so the rule is live, not dead.)

- [ ] **Step 3: Syntax-check the script** — Run: `bash -n airootfs/usr/local/bin/kiosk-session`. Expected: no output (valid).

- [ ] **Step 4: Commit**

```bash
git add airootfs/usr/local/bin/kiosk-session
git commit -m "kiosk-session: fullscreen the casttarget player window per display"
```

---

### Task 5: Image build turns native-cast on, with the player deps (kiosk-pxe)

**Files:**
- Modify: `kiosk-pxe/packages.x86_64` (add `mpv`, `ffmpeg`)
- Modify: `kiosk-pxe/build.sh:40` (`--features native-cast`)

`ffplay` ships in the `ffmpeg` package; `mpv` is its own. Only `jq` is in the set today.

- [ ] **Step 1: Add the packages** — insert `mpv` and `ffmpeg` into `packages.x86_64` (keep the file's existing ordering convention).

- [ ] **Step 2: Turn on the feature** — `build.sh:40`:

```bash
    cargo build --release --locked --features native-cast --manifest-path "${SUBMODULE_DIR}/Cargo.toml"
```

- [ ] **Step 3: Build the binary locally to prove the flag + lockfile** — Run: `SKIP_RUST= CARGO_TARGET_DIR=miniclientcontrol/target bash -n build.sh` first (`bash -n` to syntax-check), then from `miniclientcontrol/`: `cargo build --release --locked --features native-cast`. Expected: `Finished` (confirms `--locked` is satisfiable with the feature).

- [ ] **Step 4: Commit**

```bash
git add packages.x86_64 build.sh
git commit -m "image: build miniclientcontrol with native-cast, add mpv and ffmpeg"
```

---

### Task 6: Bump the mc submodule pointer in kiosk-pxe and push

**Files:**
- Modify: `kiosk-pxe/miniclientcontrol` (gitlink)

- [ ] **Step 1: Stage the submodule at the pushed mc commit**

```bash
git add miniclientcontrol
git status --short   # expect: M miniclientcontrol, and the Task 4/5 commits already in
```

- [ ] **Step 2: Commit**

```bash
git commit -m "native-cast: bump miniclientcontrol to the player app_id glue"
```

- [ ] **Step 3: Push kiosk-pxe** (remote via wrapper; kiosk-pxe remote does not need `-4` unless it is supercookie — check `git remote -v` and add `GIT_SSH_COMMAND="ssh -4"` if so)

```bash
~/.local/bin/git-wrapper.sh -C . push origin main
```

---

## Out of scope (device verification, not code)

Left for the user's hardware pass, already listed under SP3 device verification:
- override install/teardown real on the panel,
- freeze detection no restart during a cast,
- freeze CPU measurement,
- reactivation after unfreeze,
- and now: the casttarget window actually fullscreens on its display (Task 1/4), and Android-mirror app_id on the image's SDL (Task 2).

## Self-Review

- **Spec coverage:** design `:314-315` names exactly "the kiosk-session sway rule for casttarget-<display>" (Task 4) and "cast.db placement on the data partition" (covered free by `cast_db_path()`'s default — no task needed, noted in Global Constraints). Design item #2 "thread app_id to every player spawn" = Task 1/2. ✓
- **No placeholders:** every code step shows real code. ✓
- **Type consistency:** `Player::spawn(.., app_id: &str)` and `TsPlayer::spawn(app_id: &str)` are the only signature changes; both call sites are inside `start_rtp_receiver`, which binds `window_app_id` (= `AppState::app_id`) once in the lock block and passes `&window_app_id` to both — **not** `session.app_id` (the Chromecast LAUNCH id). `AudioPlayer::spawn` is untouched — it runs `--no-video --force-window=no`, no window, no app_id needed. ✓
- **Goal-critical correction (review NO-GO #1):** the window app_id must be the receiver's configured `casttarget-<name>`, not the LAUNCH appId. Addressed in Task 1 Step 4. ✓
