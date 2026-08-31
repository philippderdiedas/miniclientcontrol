# Outbound webhooks

**Status:** designed
**Date:** 2026-08-31

## What and why

The controller knows things nobody else does. A cast started. The display
browser died and came back. The playlist has been empty since Tuesday. All of
it lives in a log on a device in a corridor, and the only way to learn any of
it is to open the admin page and look.

So: the controller POSTs to URLs the operator configures when something
happens. A Discord channel gets a line when a guest puts a page on the foyer
screen; a monitoring system gets a ping when the display drops; a room-booking
system learns the display went idle.

### This is not the picklecast `--webhook` coming back

`CLAUDE.md` lists `--webhook` under "deliberately not taken from picklecast",
and that stands. That webhook was *glue between two processes* — picklecast
telling the controller a cast had started, so the controller could flip an
override. It is dead because both sides now live in one binary and the
override is set by a function call.

This is a different thing in the same shape: the controller telling a *third
party*, which was never what the picklecast callback did. Nothing here
re-introduces a second process or an inter-process dependency, and nothing on
the display path waits for a webhook.

## Decisions taken

Six questions shaped this. Their answers are load-bearing.

1. **Outbound only.** Inbound triggers already exist as `/api/override` and
   `/api/control/current` behind basic auth. A second, differently-secured
   entry point that changes what is on screen is new attack surface for no new
   capability.

2. **Many targets, configured in the settings UI, never on the command line.**
   A URL alone would fit a flag; per-target headers, event selection and a body
   template do not. One editable list in one place, and no flag that pins it.

3. **A full template engine (minijinja), not string substitution.** Discord,
   Slack, Telegram and a home-automation webhook all want different JSON, and
   the difference is structural, not a matter of filling blanks. Chosen over
   handlebars for being lighter with `serde` its only required dependency.

4. **Fire and forget.** One attempt, no retry, no disk queue. These events are
   ephemeral status, not a ledger: a `cast.started` redelivered four minutes
   later, after the cast ended, is worse than never sent. A durable outbox
   would also put an SD-card write on the path of every event, on a device
   where the card is the component that dies.

5. **Redirects are refused.** See *Security*.

6. **No global on/off and no CLI flag.** A fresh database has no rows, so
   nothing is delivered and nothing needs switching off — unlike
   `--managed-cert` and `--guest-pages`, which exist because those features act
   without being configured. It also means the Python harness needs no new
   default.

## The rule, in one sentence

An event names itself and its data; a target decides which events it wants,
what shape they arrive in, and where they go — and nothing a target does can
reach the display.

## Data model

A table, not a settings key. These are rows with independent lifetimes; the
settings KV would have to rewrite the whole blob on every edit.

```sql
CREATE TABLE IF NOT EXISTS webhooks (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    name         TEXT NOT NULL,              -- operator's label; appears in the UI and in logs
    url          TEXT NOT NULL,
    method       TEXT NOT NULL DEFAULT 'POST',
    is_enabled   BOOLEAN DEFAULT 1,
    events       TEXT DEFAULT '[]', -- JSON array of event names
    headers      TEXT DEFAULT '{}', -- JSON object; values are templates
    body         TEXT,                       -- minijinja template; NULL means the default envelope
    insecure_tls BOOLEAN DEFAULT 0,
    created_at   DATETIME DEFAULT CURRENT_TIMESTAMP
);
```

In `db.rs::run_migrations` with `CREATE TABLE IF NOT EXISTS`, like every other
table. `main.rs` does not create it.

`events` and `headers` are read `COALESCE(events, '[]')` / `COALESCE(headers,
'{}')`. A real SQL `NULL` fails to decode and takes the whole query with it,
which is the rule the JSON columns on `playlist_items` already follow.

Logs and the admin page name a target by `name`, never by URL: a URL may carry
a token in its query string.

## The events

Ten events in four families. The envelope is the same for all of them:

```json
{
  "event": "cast.started",
  "timestamp": "2026-08-31T14:03:11Z",
  "device": "foyer-pi",
  "data": { "sender_ip": "192.168.1.44", "mode": "cast" }
}
```

`device` is the machine's hostname, read once at startup from
`hostname(2)`/`/etc/hostname` and empty if it cannot be read. Deliberately not
a new setting: a receiver needs to tell two displays apart, the hostname
already does that, and an operator who wants a friendlier name has a `name`
per target and a template to put it in.

`timestamp` is RFC 3339 in UTC, from `chrono`.

| Event | `data` | Emitted from |
|---|---|---|
| `playback.item_changed` | `item_id`, `kind` (`asset`/`url`), `title`, `url`, `duration` | `browser.rs`, top of each item |
| `playback.playlist_empty` | — | `browser.rs`, entering the idle screen |
| `override.set` | `url`, `source` (`operator`/`cast`/`guest_page`) | `handlers.rs::set_override`, `cast.rs::activate_display` |
| `override.cleared` | `source` | `handlers.rs::clear_override`, `cast.rs::deactivate_display` |
| `cast.started` | `sender_ip`, `mode` | `cast.rs::activate_display` |
| `cast.ended` | `reason` (`operator`/`grace`/`disabled`), `duration_secs` | `cast.rs::deactivate_display` |
| `guest_page.shown` | `url`, `sender_ip` | `cast.rs::activate_display` |
| `guest_page.ended` | `reason`, `duration_secs` | `cast.rs::deactivate_display` |
| `display.disconnected` | `error` | `browser.rs`, outer loop on `is_connection_lost` |
| `display.connected` | `reconnect` (bool) | `browser.rs`, after a successful CDP attach |

Three properties of this table are deliberate:

- **`override.set` carries a `source`.** A cast and a guest page both pin
  `override_item`, so without it a receiver cannot tell an operator's decision
  from a cast starting, and would see two events for one occurrence. The
  cast and page families stay separate regardless, because they carry a sender.
- **Every URL goes through `guest_page::redact`.** Credentials in a guest URL
  reach the browser and nothing else; a webhook is "nothing else". Same
  function as the log and the admin page use, so the three cannot drift.
- **`title` is the asset's filename, or the URL for a URL item**, redacted like
  every other URL. There is no title column on `playlist_items`, and inventing
  one is a playlist feature, not a webhook feature.
- **`playback.item_changed` is the chatty one.** A ten-second playlist fires it
  360 times an hour. It is opt-in per target like everything else, and the UI
  says so beside the checkbox rather than leaving the operator to find out from
  their receiver's bill.

The catalogue — names, descriptions, and the fields each `data` carries — is
served by `GET /api/webhooks/events` and is the **only** source the UI uses. A
page offering placeholders the server does not send is the overlay-preview
mistake in a new place.

## The dispatcher

New module `src/webhook.rs`. Nothing else grows by more than a few lines.

```rust
pub struct Dispatcher {
    pool: SqlitePool,
    device: String,
    inflight: Arc<Semaphore>,
    last: Arc<Mutex<HashMap<i64, LastResult>>>,
}

impl Dispatcher {
    /// Never blocks, never fails, never awaits the network.
    pub fn fire(&self, event: Event) { … }
}
```

`AppState::webhooks: Arc<Dispatcher>`.

**`fire` takes `&self`, is not `async`, and returns `()`.** This is the
property the whole design rests on: `browser.rs` calls it from inside the
control loop, and a call site that could block, error, or await a slow receiver
would put a stranger's HTTP server in the path of what is on the screen. `fire`
builds the `serde_json::Value`, spawns, and returns.

Per event, in the spawned task:

1. `SELECT … FROM webhooks WHERE is_enabled = 1`, at fire time and not cached.
   An operator who disables a target expects the next event to respect it, and
   the query is trivial against a table with single-digit rows.
2. Keep the rows whose `events` array contains this event name.
3. Render body and headers per target.
4. Deliver concurrently: 5 s to connect, 10 s in total.
5. Non-2xx or transport error → `error!` naming the target and the status.
   Success → `debug!`.

Two guards the naive version misses:

- **A bounded number of deliveries in flight, and a drop rather than a queue.**
  A semaphore of 8. Past it the event is dropped with a `warn!` naming the
  count. The alternative is unbounded `spawn` on a Pi, and
  `playback.item_changed` against a receiver that has begun to hang is exactly
  the shape that produces thousands of parked tasks holding sockets. Dropping
  is honest — the delivery contract is already best-effort.
- **The response body is read and discarded with a 64 KB cap**, rather than
  ignored. A receiver that answers with a stream and never closes the
  connection otherwise ties up the socket for the whole timeout, and a hostile
  one for longer.

**Failure is per target.** Each delivery is its own task with its own result,
so a broken row cannot stop the others.

**Last result** lives in `Arc<Mutex<HashMap<i64, LastResult>>>` — `{ at, event,
outcome }`, one entry per webhook id, overwritten on each delivery. In memory
only. It is diagnostics for an operator standing at the page; persisting it
would put an SD-card write on the path of every event.

### The HTTP client

Hand-rolled in `webhook.rs`, following `managed_cert::fetch`: `url` to parse,
`TcpStream::connect`, `tokio_rustls` when the scheme is `https`,
`hyper::client::conn::http1` to speak.

Not `hyper-util`'s pooled client and not `hyper-rustls`. Both mean a new crate
or new features, and this project's rule is that a new HTTP dependency is
guilty until `cross build` says otherwise — `reqwest` pulling in `aws-lc-rs` is
why the rule exists. The hand-rolled path is already proven on armv7.

The cost is no connection reuse: a fresh TCP and TLS handshake per delivery,
which is irrelevant at these rates.

## Rendering

The template context is the envelope itself, so a target with a template and a
target without one see identical data.

**No `body`** means the envelope serialised, with `Content-Type:
application/json`. The common case needs no template.

**With a `body`**, minijinja renders it and the result is sent verbatim.
`Content-Type` comes from the target's own headers — a Discord target sets
`application/json`, a plain-text one sets `text/plain`. Unset defaults to
`application/json`.

**Header values are templates too**, rendered from the same context. It costs
nothing and buys a target that routes on an event name in a header.

Three rules keep this from emitting broken JSON:

- **`tojson` is the escape, and the UI teaches it.** `{"text": "{{ data.url
  }}"}` breaks the moment a URL contains a quote; `{"text": {{ data.url |
  tojson }}}` cannot. The placeholder chips in the UI insert the `| tojson`
  form, so what an operator gets by clicking is the correct shape.
- **Autoescape is off, unconditionally.** minijinja defaults to HTML escaping,
  which turns `&` into `&amp;` inside a JSON string.
  `Environment::set_auto_escape_callback` returns `AutoEscape::None`.
- **Undefined renders empty.** `UndefinedBehavior::Lenient`. `{{ data.title }}`
  on an event carrying no title must not fail the delivery — a missing field
  cannot be allowed to silence a display-disconnected notice. The UI's
  per-event field list is what stops this being a guessing game.

A render error — a filter that does not exist, a syntax error that survived
saving — fails *that target's* delivery, logs the minijinja error with line and
column, and appears in the target's last-result line. It touches no other
target and never reaches the control loop.

### The dependency

```toml
minijinja = { version = "2", default-features = false, features = ["builtins", "serde", "json"] }
```

Pure Rust, `serde` its only required dependency. **The plan verifies it with
`cross build --target armv7-unknown-linux-gnueabihf` as its own step, before
anything is built on top of it.** This project has been bitten twice —
`aws-lc-rs` via `reqwest`, and `libpulse-binding` — and discovering it at the
end is what makes it expensive.

## Security

- **Redirects are refused.** A `3xx` is reported as the failure it is, with its
  `Location` in the log and the last-result line. Following one would send the
  target's `Authorization` header to a host the operator never configured and
  cannot see, chosen by whoever controls the receiver; stripping the headers on
  a cross-host hop would instead deliver an unauthenticated request that fails
  anyway. GitHub and Stripe both refuse for the same reason. Separately,
  `301`/`302` are historically downgraded to `GET`, so a "successful" delivery
  would arrive as an empty one. On this device a redirect on a webhook URL is a
  config error with a one-line fix; reporting it makes that a thirty-second
  correction instead of a permanent silent rewrite.
- **TLS is verified against `webpki-roots`,** with a per-target `insecure_tls`
  opt-in. Internal receivers on self-signed certificates are this project's
  normal world — the display browser ignores certificate errors on purpose —
  but a target holding an API token must not skip verification silently. So it
  is a visible per-row choice, never a global.
- **Every URL in a payload is redacted** by `guest_page::redact`.
- **The routes are operator-only.** Not in `is_display_path`, which would
  expose them to the whole LAN, and not in `cast::is_cast_public_path`, which
  would expose them to every guest. A target's headers hold tokens.
- **Header values are returned as stored.** The settings surface is already
  behind basic auth and `cast_code` is returned in the clear today; a
  write-only field would be inconsistent for no gain against an attacker who,
  by then, is already authenticated.

## API

- `GET /api/webhooks` — list, each row carrying its in-memory `last_result`
- `POST /api/webhooks` — create
- `PUT /api/webhooks/{id}` — update
- `DELETE /api/webhooks/{id}`
- `POST /api/webhooks/{id}/test` — render and deliver a sample event
- `GET /api/webhooks/events` — the event catalogue

Validation on create and update: an unknown event name is a `400`; a URL whose
scheme is not `http`/`https` is a `400`; `method` is restricted to `POST`,
`PUT`, `PATCH`, because a `GET` webhook with a body is a contradiction; the
template is compiled and a syntax error answers `{"error": "…"}` with the
minijinja message, which the UI shows inline per the project convention.
Compiling is the only check possible up front — a template valid for one event
and nonsense for another is what the test send is for.

Everything else follows the swallow-and-log rule: a handler that cannot read
the table logs and answers with what it has.

### The test send

`POST /api/webhooks/{id}/test` renders the target against a synthetic sample of
an event the operator picks and **delivers it for real**, answering `{status,
body_excerpt, error}`. Not a dry run: a dry run proves the template compiles
and nothing about whether Discord accepts it, which is the actual question. The
envelope carries `"test": true` so a receiver can tell.

## The admin page

New page `web/webhooks.html`, linked from `admin.html`. Not a section of the
settings form — that is one atomic write of a fixed struct, and this is a list
of rows with their own lifecycle.

Per row: name, URL, method, enabled, event checkboxes grouped by family, a
key/value header editor, a body textarea, `insecure_tls`, **Testen**,
**Löschen**. Beside the template, the selected events' fields as clickable
chips that insert `{{ data.x | tojson }}`. The last result as a coloured line —
`✓ 204 · vor 3 s · playback.item_changed`, or the failure with its status. The
chatty-event warning sits next to `playback.item_changed`, not in a document
nobody opens.

Follows the existing UI rules. Vanilla JS, rows built with `textContent` and
`createElement` through an `el()` helper and **never** `innerHTML`
interpolation: a webhook URL and a receiver's error body are both
attacker-influenced. The page polls every 2 s and **updates only the
last-result lines**, never re-rendering a card — the cards are the edit form,
and a re-render eats whatever the operator is typing. Cards with unsaved edits
go in a `dirty` set carried across reloads, exactly as `playlist.html` does.

`web/` is compiled into the binary by `include_dir!`. Changing this page needs
a rebuild, or the change looks exactly like a change that did not work.

## Not in scope

- **Inbound webhooks.** Decision 1.
- **Retries, an outbox, replay, dead-lettering.** Decision 4. If reliability is
  wanted later it is an outbox table plus a drain task, and it does not
  invalidate anything here.
- **Per-target rate limiting or coalescing.** The event checkboxes are the
  control. Revisit if `playback.item_changed` proves unusable in practice.
- **Signing deliveries** (an HMAC over the body, as GitHub sends). Receivers on
  a LAN behind an operator-set bearer token do not need it, and a signature
  nobody verifies is decoration.
- **Templating the URL.** A target's URL is fixed. A URL assembled from event
  data is a request to a host chosen by the payload, which is the redirect
  problem wearing a different hat.

## Testing

Rust unit tests in `webhook.rs`:

- each event serialises to the documented shape
- event-name matching against a target's list, including an unknown name
- `tojson` renders a quote-bearing URL into valid JSON
- autoescape is off: a `&` survives rendering
- lenient undefined: a missing field renders empty and does not error
- a URL with credentials arrives redacted in the payload

`tests/cast/test_webhook.py`, stdlib-only, in the existing harness style — an
`http.server` receiver on a spare port recording what arrives:

- a delivery arrives with the expected body and headers
- a target not subscribed to the event receives nothing
- a disabled target receives nothing
- credentials in a guest page URL arrive redacted
- a receiver answering `500` does not stop a second target from receiving
- **a receiver that hangs does not stall the playlist** — assert the next item
  still advances
- a `301` is refused and reported, and the body never reaches the new location
- `POST /api/webhooks/{id}/test` returns the receiver's status

The hang test is the important one. It is the assertion that `fire` cannot
reach the control loop, which is the single thing that must not break.

## Documentation to update

- `README.md` — the API overview gains a Webhooks section
- `docs/features.md` — what webhooks do
- `docs/architecture.md` — the module map gains `webhook.rs`
- `CLAUDE.md` — the `--webhook` line under "not taken from picklecast" gains a
  sentence distinguishing that callback from this feature, plus the `fire`
  must-not-block rule and the refuse-redirects rule
