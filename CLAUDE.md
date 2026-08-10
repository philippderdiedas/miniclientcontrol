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
`--remote-debugging-port=9222 --kiosk`; the controller *connects* to it.

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

There is no test suite and no linting config. `cargo build` is the only gate.
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
be clobbered. The loop `take()`s `pending_jump` when it wakes on `skip_signal`.

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
| PUT/DELETE | `/api/playlist/{id}` | |
| GET/POST | `/api/control/current` | POST `{ item_id }` = play now |
| POST/DELETE | `/api/override` | POST `{ asset_id? , url?, scroll_config? }` |

Nullable-clearable fields (`start_date`, `end_date`) use
`Option<Option<String>>` with `#[serde(default, deserialize_with = "double_option")]`
in `handlers.rs`. Without the custom deserializer a JSON `null` collapses to the
outer `None` and the field can never be cleared.

## Conventions

- All handler DB errors are swallowed (`let _ = …` / `unwrap_or_default`) so the
  display never dies on a bad request. Keep that, but `error!`-log first.
- The UI is dependency-free vanilla HTML/JS. Build rows with `textContent` /
  `createElement`, not `innerHTML` string interpolation — filenames and URLs are
  attacker-influenced.
- `duration` is in seconds and comes from the DB as `i64`; clamp before casting to
  `u64` (a negative value became ~584 billion years of `Duration` and froze the
  playlist on one item).
