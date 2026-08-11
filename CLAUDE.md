# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What this is

`miniclientcontrol` is a single-binary digital-signage controller that runs **on the
client/display device itself** (typically a Raspberry Pi — note the
`arm-unknown-linux-gnueabihf` target in `target/`). It does two things at once:

1. Serves a small web UI + JSON API on `--port` (default `3000`) so an operator can
   upload assets and manage a playlist.
2. Drives a locally running Chromium over the Chrome DevTools Protocol (CDP),
   navigating it through the playlist and injecting a scroll runtime.

The controller does **not** launch Chromium. Chromium must already be running with
`--remote-debugging-port=9222 --kiosk`; the controller *connects* to it. On the Pi
that launch lives in `~/.config/sway/config`, outside this repo.

Chromium's "translate this page?" bubble is **not** suppressible by a command-line
flag. It needs the managed policy, on the device at
`/etc/chromium/policies/managed/no-translate.json`:

```json
{ "TranslateEnabled": false }
```

Verify with `chrome://policy`: the row must read `TranslateEnabled / false /
Platform / Machine / Mandatory / OK`. Three flags look like they should do this
and do not: `--disable-translate` and `--disable-infobars` are ignored outright by
current Chromium (144 on the device), and `--disable-features=Translate` *is*
applied — child processes inherit it — but does not gate the bubble. The prompt
appears because the profile's `intl.selected_languages` is `en-GB,en-US,en` while
the signage shows German pages. The policy is also the only durable fix here:
`--user-data-dir=/tmp/chromium-1` is wiped on boot, so a profile preference would
not survive.

## Build & run

```bash
cargo build            # debug
cargo build --release
cargo run --release -- --port 3000 --cdp-url http://127.0.0.1:9222
```

Cross build for the Pi target that is already configured:

```bash
cargo build --release --target arm-unknown-linux-gnueabihf
```

The device (`pi@10.124.11.124`) reports `uname -m` = `armv7l`, so the build that
actually ships is `armv7-unknown-linux-gnueabihf` via `cross` (Docker daemon must
be running). Give each cross target its **own** `--target-dir`: host proc-macro
`.so`s land in the shared `target/release/build/`, and the per-target `cross`
images carry different glibc versions, so reusing one directory across two targets
fails with `symbol getrandom, version GLIBC_2.25 not defined`.

```bash
cross build --release --target armv7-unknown-linux-gnueabihf --target-dir target/cross-armv7
```

There is no test suite and no linting config. `cargo build` is the only gate.

### Deploying to the device

The binary is running, so copy beside it and rename over the top — an in-place
`scp` gets `ETXTBSY`. Keep the previous binary as a rollback.

```bash
scp <binary> pi@10.124.11.124:~/miniclientcontrol/miniclientcontrol.new
ssh pi@10.124.11.124 'cd ~/miniclientcontrol && cp -a miniclientcontrol miniclientcontrol.bak && mv miniclientcontrol.new miniclientcontrol'
```

**Restart with `sudo loginctl terminate-session <id>`, not by restarting
`getty@tty1`.** Sway is launched from an autologin `/bin/login -f` and lives in a
logind session scope; restarting the getty *service* leaves that scope alone. The
controller and Chromium then survive as orphans re-parented to PID 1, the old
controller keeps port 3000, and the fresh one from sway's `exec` dies on the bind
— leaving the display running the old, already-deleted binary. Terminating the
session kills the whole cgroup, orphans included, and autologin brings everything
back. Find the id with `loginctl list-sessions` (the one with a `tty1` seat).

Also beware `pkill -f` over SSH: a pattern like `miniclientcontrol/miniclientcontrol`
matches the remote shell running the command and kills it mid-script, so the rest
of the command never runs and the output is silently empty.
After changing anything under `web/`, you must **rebuild** — `web/` is compiled
into the binary via `include_dir!` (see `src/web.rs`), it is not read from disk.

## Architecture

```
src/main.rs      CLI parsing, DB pool + schema bootstrap, basic-auth middleware,
                 axum Router, spawns browser_loop
src/models.rs    Args (clap), Asset, PlaylistItemWithAsset, ScrollMode, AppState
src/db.rs        run_migrations() — idempotent CREATE TABLE + ADD COLUMN probes
src/handlers.rs  JSON/multipart API handlers
src/browser.rs   the CDP control loop (largest file; all playback logic)
src/web.rs       serves web/ embedded via include_dir
web/             operator UI + the pages the *display* browser renders
```

### Two audiences for HTTP

This is the single most important thing to keep in mind when touching routes or
middleware. The HTTP server has **two different clients**:

- The **operator** (a human, possibly remote): `/`, `/index.html`,
  `/assets.html`, `/playlist.html`, `/api/*`.
- The **display browser** (Chromium on loopback, sends no credentials):
  `/uploads/*`, `/pdf_viewer.html`, `/pdf.min.js`, `/pdf.worker.min.js`,
  `/autoscroll.js`, `/no_content.svg`, `/empty_playlist.html`, `/logo.svg`.

Basic auth must never be applied to the second set, or the signage goes blank.
`src/main.rs` handles this by exempting loopback peers (`ConnectInfo<SocketAddr>`)
— which is why the server is started with
`into_make_service_with_connect_info::<SocketAddr>()`. Do not drop that.

### Control loop (`src/browser.rs`)

Nested loops:

- **outer**: connect to CDP, subscribe to `Target.attachedToTarget`, pick/clean a
  single control page, install the scroll runtime. Any `is_connection_lost` error
  breaks back out to here and reconnects.
- **inner**: if an override is set, run `run_override_loop`; otherwise fetch the
  active playlist (`is_enabled = 1` and inside the date window), reconcile
  `keep_loaded` tabs, then iterate items.
- **per item**: navigate, wait for readiness, start scrolling, then
  `tokio::select!` on the duration timer / `skip_signal` / `playlist_signal`.

### Signals

`AppState` carries three `Arc<Notify>`: `skip_signal`, `playlist_signal`,
`override_signal`.

**Always use `notify_one()`, never `notify_waiters()`.** The control loop is only
parked on these notifies for part of its cycle (navigation and readiness waiting
can take 10+ seconds). `notify_waiters()` drops a notification when no task is
currently parked, which silently loses "Play now" clicks and override changes.
`notify_one()` stores a permit, so the loop picks it up at the next await point.

### "Play now" / jumps

`POST /api/control/current` writes `AppState::pending_jump`, **not**
`current_item_id`. `current_item_id` is loop-owned (it reports what is on screen);
the loop overwrites it at the top of every item, so a request writing there would
be clobbered.

**Never consume `pending_jump` before the target has been looked up in a freshly
fetched playlist.** The loop's `playlist` snapshot is read once per inner-loop pass
and then iterated item by item, so it can be a whole item duration out of date — an
item added or re-enabled since the last fetch is simply not in it. The loop
therefore *peeks* on `skip_signal`: if the target is in the current snapshot it
jumps immediately, otherwise it breaks out, re-reads the playlist, and resolves the
jump against the fresh list before the item loop starts. Only then is it cleared.
`take()`ing it on the miss path silently dropped the click and resumed playback on
an unrelated item, which is what "Play now plays something random" was.

### Scroll runtime

`web/autoscroll.js` is both served over HTTP *and* `include_str!`-ed into the
binary (`scroll_runtime_script()` in `browser.rs`). It installs `globalThis.__as`
plus `__asApply`. It is registered via `Page.addScriptToEvaluateOnNewDocument` and
also re-evaluated after navigation, because pages with a strict CSP can block it —
`apply_scroll_settings` probes `!!globalThis.__as` and no-ops if it is missing.

PDFs are a special case: they are rendered by `web/pdf_viewer.html` (pdf.js), which
drives its own scrolling from query parameters. `browser.rs` detects this with
`is_internal_pdf_viewer_url` and skips `start_scrolling`/`stop_scrolling` for those.

### pdf.js

`web/pdf.min.js` and `web/pdf.worker.min.js` are vendored (v3.11.174) and served
from the same origin. Never point `workerSrc` at a CDN — the device is often
offline, and the operator page would work while the display silently failed.

## Database

SQLite, path from `--database-path` (default `miniclient.db`, gitignored).
Schema lives **only** in `src/db.rs::run_migrations`, which is idempotent:
`CREATE TABLE IF NOT EXISTS` plus `pragma_table_info` probes before each
`ALTER TABLE ADD COLUMN`. Add new columns the same way. `main.rs` must not
create tables itself — a partial duplicate there caused schema drift.

`PRAGMA foreign_keys` is enabled per connection via `SqliteConnectOptions`
(it is off by default in SQLite, so `ON DELETE CASCADE` was previously a no-op
and deleting an asset left orphaned playlist rows).

Read paths use `COALESCE(p.scroll_config, '…')`: a `NULL` in that column makes the
whole `query_as` fail, and both call sites swallow the error into an empty
playlist, so one bad row would blank the screen.

## API

| Method | Path | Notes |
|---|---|---|
| GET/POST | `/api/assets` | POST is multipart; each field is one file |
| PUT/DELETE | `/api/assets/{id}` | PUT body `{ duration }` |
| GET/POST | `/api/playlist` | |
| PUT/DELETE | `/api/playlist/{id}` | PUT also takes `url` / `asset_id`, see below |
| POST | `/api/playlist/{id}/move` | `{ direction: "up" \| "down" }`, renumbers the list |
| GET/POST | `/api/control/current` | POST `{ item_id }` = play now |
| GET/POST/DELETE | `/api/override` | POST `{ asset_id? , url?, scroll_config? }` |

`PUT /api/playlist/{id}` may change an item's source, but only like for like: a
URL item takes a new `url`, an asset item a new `asset_id`. The opposite is a
`400` — a kind change would need the other column cleared in the same write, and
`playlist_target_url` picks one column over the other silently, so a half-changed
row plays the wrong thing with no error anywhere. `asset_id` is checked against
`assets` first; a dangling id yields a `NULL` `local_path` from the loop's
`LEFT JOIN` and a blank screen.

`POST /api/playlist/{id}/move` renumbers every row to `1..n` instead of swapping
two values. `play_order` is typed by hand in the UI, so duplicates and gaps
accumulate, and a pairwise swap between two rows sharing an order does nothing.

Handlers that can reject a request answer with `{ "error": "..." }`; the UI shows
that string inline. Everything else stays on the "swallow and log" rule below.

Nullable-clearable fields (`start_date`, `end_date`) use
`Option<Option<String>>` with `#[serde(default, deserialize_with = "double_option")]`
in `handlers.rs`. Without the custom deserializer a JSON `null` collapses to the
outer `None` and the field can never be cleared.

## Conventions

- All handler DB errors are swallowed (`let _ = …` / `unwrap_or_default`) so the
  display never dies on a bad request. Keep that, but `error!`-log first.
- The UI is dependency-free vanilla HTML/JS. Build rows with `textContent` /
  `createElement`, not `innerHTML` string interpolation — filenames and URLs are
  attacker-influenced. `playlist.html` funnels this through a small `el()` helper.
- `playlist.html` polls `/api/control/current` and `/api/override` every 2s and
  only updates badges and highlight classes from the poll. It must not re-render
  the item list on a tick: the cards *are* the edit form, so a re-render would wipe
  whatever the operator is typing. Cards with unsaved edits are tracked in a
  `dirty` set and carried over verbatim across list reloads for the same reason.
- `duration` is in seconds and comes from the DB as `i64`; clamp before casting to
  `u64` (a negative value became ~584 billion years of `Duration` and froze the
  playlist on one item).
