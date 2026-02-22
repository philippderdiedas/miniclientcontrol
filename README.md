# Mini Client Control

Mini Client Control is a Rust-based local signage/playback controller.

It provides:
- a web UI for uploading assets and managing a playlist,
- a SQLite-backed scheduler (order, enable/disable, optional date window),
- browser automation via Chrome DevTools Protocol (CDP),
- optional scroll behavior for long pages and PDFs,
- an override mode to immediately play a specific asset or URL.

## Tech Stack

- Rust + Tokio
- Axum (HTTP server + API)
- SQLx + SQLite
- Chromiumoxide (CDP browser control)
- Embedded static UI from `/web`

## Requirements

- Rust toolchain (stable)
- A Chromium/Chrome instance running with remote debugging enabled

Example:

```bash
chromium --remote-debugging-port=9222 --kiosk
```

If Chromium is not available as `chromium`, try your local binary (for example `google-chrome` or `chromium-browser`).

## Quick Start

1. Build and run:

```bash
cargo run --release
```

2. Open the control UI:

- `http://localhost:3000/`

3. In the UI:

- Upload files in **Asset Management**
- Add assets/URLs in **Playlist Management**
- Set order, duration, schedule window, and scroll mode

### Enable Basic Auth

Basic Auth is optional. To enable it, set both username and password:

```bash
cargo run --release -- \
  --basic-auth-user admin \
  --basic-auth-password 'change-me'
```

Or with environment variables:

```bash
export BASIC_AUTH_USER=admin
export BASIC_AUTH_PASSWORD='change-me'
cargo run --release
```

If credentials are enabled, all routes (UI, API, uploads) require authentication.

## Command Line Options

```text
--port <u16>                 (default: 3000)
--assets-dir <path>          (default: ./assets)
--database-path <path>       (default: miniclient.db)
--cdp-url <url>              (default: http://127.0.0.1:9222)
--basic-auth-user <string>   (optional, must be set with password)
--basic-auth-password <string> (optional, must be set with user)
```

These options also support environment variables through `clap` `env` support.

## Runtime Behavior

- Creates the asset directory if missing.
- Creates/migrates SQLite tables (`assets`, `playlist_items`).
- Starts an HTTP server on `0.0.0.0:<port>`.
- Serves uploaded files from `/uploads/...`.
- Serves embedded UI files with fallback to `index.html`.
- Runs a background browser loop that:
  - reads active playlist entries,
  - loads content in controlled browser tabs,
  - applies scroll mode,
  - reacts to skip/playlist/override signals.

## API Overview

### Assets

- `GET /api/assets` — list assets
- `POST /api/assets` — upload one or more files (`multipart/form-data`)
- `PUT /api/assets/{id}` — update asset metadata (currently duration)
- `DELETE /api/assets/{id}` — delete file + DB row

### Playlist

- `GET /api/playlist` — list playlist items with joined asset info
- `POST /api/playlist` — add item (asset or URL)
- `PUT /api/playlist/{id}` — update order/duration/enabled/schedule/scroll config
- `DELETE /api/playlist/{id}` — remove playlist item

### Playback Control

- `GET /api/control/current` — get current item id
- `POST /api/control/current` — jump to item id (`{ "item_id": <id|null> }`)

### Override

- `POST /api/override` — activate override playback
  - body supports either `asset_id` or `url`
  - optional `scroll_config`
- `DELETE /api/override` — clear override and return to playlist loop

## Scroll Configuration

`scroll_config` is serialized as tagged JSON:

- `{"type":"None"}`
- `{"type":"Step","options":{"step_time":500,"step_px":null,"step_delay":2000}}`
- `{"type":"Continuous","options":{"speed":1.0,"top_delay":2000,"return_delay":2000}}`

PDFs are rendered through the internal viewer (`/pdf_viewer.html`) and support both step and continuous scrolling.

## Project Layout

- `src/main.rs` — app bootstrap, router setup
- `src/handlers.rs` — REST API handlers
- `src/browser.rs` — browser/session/playback loop
- `src/db.rs` — schema init + lightweight migrations
- `src/models.rs` — CLI args, DTOs, app state
- `src/web.rs` — embedded static file serving
- `web/` — frontend pages and JS helpers
- `assets/` — uploaded files (runtime)

## Notes

- The server currently uses permissive CORS (`CorsLayer::permissive()`).
- Max upload body size is configured to 500 MB.
- This project is designed for trusted local/network environments unless hardened further.
