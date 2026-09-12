# Architecture

## Modules

```
src/main.rs      CLI, database bootstrap, auth middleware, router, two listeners
src/models.rs    CLI arguments, DTOs, AppState
src/db.rs        run_migrations(): idempotent CREATE TABLE + ADD COLUMN probes
src/handlers.rs  assets, playlist, playback control, override
src/browser.rs   the CDP control loop -- all playback logic lives here
src/cast.rs      cast signaling relay and session lifecycle
src/settings.rs  runtime settings, operator credentials, overlay config, /api/settings
src/chromium.rs  finds, launches and supervises the display browser
src/tls.rs       self-signed certificate, HTTPS listener, public-address resolution
src/mdns.rs      publishes an extra .local name via avahi-publish
src/audio.rs     venue audio through pactl, behind a backend seam
src/webhook/     outbound webhooks: the Dispatcher on AppState, and its HTTP surface
src/web.rs       serves web/ embedded with include_dir
web/             the operator UI and the pages the display browser renders
```

`web/` is **compiled into the binary**. Editing anything there needs a rebuild;
it is not read from disk at runtime.

## The control loop

`browser_loop` is three nested loops:

- **outer** — connect to CDP, subscribe to target events, pick and clean a single
  control page, install the injected runtimes. Any error that means the CDP
  session is gone breaks back out to here and reconnects.
- **inner** — if an override is set, run the override loop; otherwise read the
  active playlist, reconcile keep-loaded tabs, and iterate items.
- **per item** — navigate, wait for readiness, apply the overlay, start scrolling,
  then `select!` on the duration timer against the skip, playlist and overlay
  signals.

The loop **owns what is on screen**. The API never navigates; it writes state and
pokes a signal.

### Signals

`AppState` carries several `Arc<Notify>`: skip, playlist, override, overlay.

**Always `notify_one()`, never `notify_waiters()`.** The loop is only parked on
these for part of its cycle — navigation and readiness waiting can take ten
seconds or more. `notify_waiters()` drops a notification when nobody is parked,
which silently loses a "play now" click. `notify_one()` stores a permit.

### Why "play now" is not a field the API writes

`current_item_id` reports what is on screen and is owned by the loop, which
overwrites it at the top of every item. A jump request therefore goes to
`pending_jump` instead, and is only cleared once the target has been found in a
**freshly read** playlist: the loop's snapshot can be a whole item duration stale,
so an item added or re-enabled since the last read is simply not in it.

## Three audiences for HTTP

This is the thing to keep in mind when touching routes or middleware. The server
has three kinds of client, and they need different treatment:

| Audience | Reaches | Credentials |
|---|---|---|
| **Operator** | `/admin.html`, `/playlist.html`, `/assets.html`, `/api/*` | required, when configured |
| **Display browser** | `/uploads/*`, `/pdf_viewer.html`, the pdf.js files, `/autoscroll.js`, `/no_content.svg`, `/empty_playlist.html`, `/logo.svg`, `/api/cast/state` | exempt, **loopback only** |
| **Cast guest** | `/`, `/index.html`, `/cast.html`, `/cast.js`, `/cast_display.html`, `/api/cast/{ws,claim,pair,info,qr.svg,audio}` | exempt, **from any address** |

The display browser is driven over CDP and cannot present credentials, so
requiring them there blanks the signage. The guest is somebody's laptop on the
LAN, so its exemption cannot be address-scoped — and gating it behind operator
credentials would mean handing out the operator password to everyone who wants to
share a screen. `--cast-auth` guards those routes instead.

Getting the two exemption lists confused breaks it in one of two directions:
widen the display list and its paths become reachable from the whole LAN; narrow
the guest list and nobody can cast.

Room audio is the case where the same feature sits on both sides of that line.
`/api/cast/audio` is on the guest list and checks the connected sender's address
instead of credentials; `/api/audio` is not on it, so the operator credentials
decide, and it works with no cast running. Two routes rather than one widened
check: the guest route is exempt from authentication, so admitting "somebody else"
there would admit the whole LAN. They share their implementation, so the two
cannot behave differently.

Both listeners are started with connection info attached, because the loopback
check needs the peer address. Dropping that makes the extractor panic.

## Two listeners, one router

- **Plain HTTP** on `--port`, bound to **loopback only** by default. It exists for
  the display browser, which fetches from `http://127.0.0.1` — already a secure
  context, so TLS there would buy nothing and cost a certificate the kiosk
  browser has no way to trust.
- **HTTPS** on `--cast-tls-port`, bound to all interfaces, with a self-signed
  certificate. Everything a human touches goes over this, which also stops
  operator credentials from crossing the network base64-encoded.

Both serve the **same** `Router` and the same `AppState`, so a guest on the TLS
origin and the display on loopback meet in one signaling registry, and the guest
page never fetches across origins.

The TLS socket is bound **before** anything else needs the port, so a clash is a
startup error rather than a background task that logs and leaves casting dead. An
explicitly configured port that is taken is fatal — usually an older instance of
this binary still holding it. With no flag, the next free port after 3443 is
taken and logged, and `AppState::cast_tls_port` is the port actually bound.

## A cast is an override

Starting a cast pins the existing override to the cast page; ending one puts back
whatever was there before. `browser.rs` needed no changes for this, because two
things there already do the right thing:

- the per-item `select!` watches the override signal, so a cast interrupts the
  current item instead of waiting out its duration;
- the override loop ignores a notification whose override is unchanged, without
  which a redundant notify would re-navigate the page and tear down the live peer
  connection mid-cast.

On teardown the previous override is restored **only if the one on screen is still
ours**. An operator who set a different override during the cast made a newer
decision, and silently reverting it would look like the UI ignoring them.

## Outbound webhooks

`webhook::Dispatcher` lives on `AppState`, and `fire` is the whole interface the
rest of the tree sees: `browser.rs`, `cast.rs` and `handlers.rs` call it and carry
on. It is synchronous and returns nothing, because `browser.rs` calls it from
inside the control loop — anything that could block, error or await there would
put a stranger's HTTP server in the path of what is on the screen. `fire` builds
the payload, spawns, and returns; the spawned tasks read the enabled targets,
render per target and deliver, bounded by a semaphore that **drops** rather than
queues once it is full.

`src/webhook/mod.rs` holds the events, the storage, the rendering and the
hand-rolled HTTP client; `src/webhook/api.rs` holds the operator's CRUD routes and
the event catalogue the admin page renders from. The split is for file size only.

## Injected runtimes

Two scripts are registered to run on every new document and re-evaluated after
navigation: the scroll runtime and the overlay runtime. Re-evaluation is not
belt-and-braces — a page with a strict content-security policy can block the
registered copy, so both are probed for and no-op when missing.

The overlay is applied per item, and the item's own overlay is read **fresh** from
the database rather than from the playlist snapshot, which can be an item duration
old.

A page that shows a connection code asks the overlay to stand down, and sets a
global flag **before** calling — the runtime seeds itself from that flag on
install. This is load-bearing rather than defensive: a pairing code arrives on the
socket within a second of the page loading, while the controller injects the
runtime only after its readiness waits, so a page that could only call the method
would be covered by the very overlay it asked to move.

The overlay's configuration is **pushed into the page** over CDP rather than
fetched by it. That is why `/api/overlay` needs no authentication exemption: only
the admin page ever requests it. A display that had to fetch its own
configuration would need one, and would break the moment credentials were turned
on.

## Database

SQLite, path from `--database-path`. Tables: `assets`, `playlist_items`,
`settings` (key/value, for what the operator can change without a restart), and
`webhooks` (one row per outbound target).

Schema lives **only** in `db::run_migrations`, which is idempotent:
`CREATE TABLE IF NOT EXISTS` plus a `pragma_table_info` probe before each
`ALTER TABLE ADD COLUMN`. `main.rs` must not create tables itself; a partial
duplicate there once caused schema drift.

`PRAGMA foreign_keys` is enabled per connection, because it is off by default in
SQLite and `ON DELETE CASCADE` was therefore a no-op — deleting an asset left
orphaned playlist rows behind.

Read paths `COALESCE` the JSON columns. A `NULL` there fails to decode, which
fails the whole query, and both call sites swallow the error into an empty
playlist — so one bad row would blank the screen.

## Conventions

- Handler database errors are swallowed so the display never dies on a bad
  request, but they are logged first.
- The UI is dependency-free vanilla HTML and JS. Rows are built with
  `textContent` and `createElement`, never string-interpolated HTML: filenames
  and URLs are attacker-influenced.
- Pages that poll must not re-render the controls they poll into. On the playlist
  and admin pages the form **is** the state, so a refresh would eat what the
  operator is typing. Cards with unsaved edits are tracked and carried across
  reloads for the same reason.
- Durations arrive as `i64` and are clamped before casting to `u64`. A negative
  value once became about 584 billion years of `Duration` and froze the playlist
  on one item.
