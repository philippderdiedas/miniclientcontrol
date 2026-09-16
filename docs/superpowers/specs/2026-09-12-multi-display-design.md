# Multiple displays from one controller

**Status:** implemented
**Date:** 2026-09-12

## What and why

One controller drives one screen. A venue with two screens runs two controllers,
each with its own port, its own database and its own assets directory — which is
what [README.md](../../../README.md#two-displays-on-one-machine) documents today,
and what the fab lab actually runs.

That works, and it has one genuine virtue worth naming up front: a controller
that crashes takes down one screen, not both. Folding two screens into one
process gives that up. It is worth giving up because the separation costs more
than it pays, in four specific ways:

- **The same file is uploaded twice.** Separate `--assets-dir` means the same PDF
  lives in two places and is updated in two places. Two processes cannot share an
  asset library without sharing a database, and sharing a database is most of
  this change anyway.
- **One HTTP port cannot serve both.** An operator reaches screen one on `:3000`
  and screen two on `:3001`, and has to remember which is which.
- **Two admin pages.** Every playlist edit begins by deciding which URL to open.
- **A guest cannot be sent to a particular screen.** They get whichever screen's
  QR code they scanned, because that screen's controller is the only one that
  knows about them.

So: one controller, one port, one asset library, one admin page, and as many
screens as the deployment declares.

### Scope

This spec covers two of the three pieces the work decomposes into:

1. **Playlists become objects.** A `playlists` table, items belong to a playlist,
   and the API and admin page follow. Still one display, no behaviour change.
2. **The controller drives N displays.** One Chromium and one control loop per
   display, per-display playback state, playlist assigned per display.

**Casting per display is deliberately not here.** `cast.rs` is the most intricate
file in the tree, and rebuilding it at the same time as the control loop would
blur exactly the task boundaries that make review effective. Until it lands,
casting targets one configured display (see *Casting, in the meantime*).

## Decisions taken

Seven questions shaped this. Their answers are load-bearing.

1. **Independent playlists per screen, not mirroring.** Mirroring falls out for
   free — two displays assigned the same playlist — so building for independence
   costs nothing extra and buys the case the venue actually has: the foyer shows
   opening hours while the workshop shows machine status.

2. **A playlist is a first-class object, not a column on an item.** The
   alternative — `playlist_items.display_id` — cannot express "both screens show
   this", and makes mirroring a matter of duplicating rows. It also cannot
   express the case that decided this: a screen is removed for good and the
   *remaining* screen should take over the removed one's playlist. With playlists
   as objects that is a dropdown; with a column it is a data migration.

3. **A playlist is never deleted with a display.** It outlives the screen it was
   assigned to, unassigned, until an operator picks it up again.

4. **Displays are declared by the deployment, not discovered.** This reverses an
   earlier decision, and the reasoning is worth keeping. Discovery was prototyped
   against sway 1.12 and works: `swaymsg -t get_outputs` lists connector names,
   `[app_id="…"] move container to output …` places a window, and an unplugged
   output hands its window to another rather than losing it. It was dropped
   anyway, because it puts compositor-specific knowledge inside the controller —
   sway names an output `HDMI-A-1` where i3 says `HDMI-1` — and because it takes
   window placement away from the window manager, which
   [README.md](../../../README.md#two-displays-on-one-machine) already assigns to
   the window manager. A third screen is a once-per-installation act, not a daily
   one; a flag and two lines of window-manager config are the right price.

   What the prototype did establish, and what this design rests on, is that
   **`--chromium-class` becomes the Wayland `app_id`** — so the window manager can
   already tell two of our windows apart, with no help from us.

5. **One Chromium per display, not one Chromium with several windows.** One
   browser can host several windows over CDP — `Target.createTarget` with
   `newWindow` — but it was measured and buys nothing: 515 MB PSS for one browser
   with two windows against 494 MB for two browsers with one each. Chromium's cost
   is per renderer, not per browser process. It would also cost the placement
   mechanism, because every window of one process shares an `app_id`, and
   `Browser.setWindowBounds` does not move a window under native Wayland — the
   size takes effect and the position does not, which is Wayland working as
   designed rather than a bug.

6. **Only what must be per-display becomes per-display.** Assets, settings, the
   overlay configuration, credentials, audio and webhook targets stay global,
   because duplicating *configuration* was never the complaint. Playback state,
   the assigned playlist and the override are per display because they cannot be
   anything else.

7. **Casting targets one configured display until it is built properly.**

## The rule, in one sentence

A display is a name the deployment declares, a playlist is a thing an operator
names and assigns, and everything else the controller owns stays shared.

## Data model

```sql
CREATE TABLE IF NOT EXISTS playlists (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT NOT NULL,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS displays (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    -- The name the deployment declared with `--display`. The identity: an
    -- operator's assignment has to survive a restart, so it cannot be an index.
    name        TEXT NOT NULL UNIQUE,
    -- What the operator sees. Defaults to `name`, editable.
    label       TEXT,
    playlist_id INTEGER,
    FOREIGN KEY(playlist_id) REFERENCES playlists(id) ON DELETE SET NULL
);
```

`playlist_items` gains `playlist_id INTEGER` with the same `ON DELETE SET NULL`
reasoning inverted: deleting a playlist must **not** delete its items silently, so
the API refuses to delete a playlist that still has items and says how many.

**`ON DELETE SET NULL` on `displays.playlist_id` is what makes decision 3 true in
the schema** rather than only in the handler: deleting a playlist unassigns it
from any display, and deleting a display cannot reach the playlist at all.

### Migration

`run_migrations` gains the two tables and the column, then a one-time backfill
guarded the way the existing probes are:

1. If `playlists` is empty **and** `playlist_items` has rows, insert a playlist
   named `Standard` and set every item's `playlist_id` to it.
2. If `displays` is empty, insert one row per declared display. The first
   declared display is assigned the `Standard` playlist if one was just created.

After upgrading, a single-screen device plays exactly what it played before, from
a playlist it did not previously know it had. That property is the whole reason
subproject 1 is separated from subproject 2 — it is testable on its own, against a
copy of a real device's database, before any of the N-display work exists.

## Declaring displays

```
--display foyer --display werkstatt
--display foyer:9222 --display werkstatt:9223   # explicit CDP port
```

Each declared display gets, exactly as a separate controller does today:

| | derived from | example |
|---|---|---|
| CDP port | `9222 + index`, or the explicit form | `9223` |
| `app_id` / WM class | `miniclientcontrol-<name>` | `miniclientcontrol-werkstatt` |
| user-data-dir | `/tmp/miniclientcontrol-chromium-<name>` | |

The `app_id` is derived from the **name**, not the port as today, because the
window-manager config a human writes should read `werkstatt`, not `9223`.

**With no `--display` at all, the controller behaves exactly as it does now**: one
implicit display named `default`, using `--cdp-url` and the existing class and
profile defaults. Every current deployment keeps working untouched, which matters
because these run unattended in a venue.

Window placement stays the window manager's job:

```
assign [app_id="miniclientcontrol-foyer"]     output HDMI-A-1
assign [app_id="miniclientcontrol-werkstatt"] output DP-1
```

## Per-display state

`AppState`'s singular playback fields move into a `Display` struct, one per
declared display, behind `Arc<HashMap<String, Arc<Display>>>` keyed by name:

```rust
pub struct Display {
    pub name: String,
    pub cdp_url: String,
    pub current_item_id: Mutex<Option<i64>>,
    pub pending_jump: Mutex<Option<i64>>,
    pub override_item: Mutex<Option<OverrideItem>>,
    pub browser_pid: Mutex<Option<u32>>,
    pub skip_signal: Notify,
    pub playlist_signal: Notify,
    pub override_signal: Notify,
    pub overlay_signal: Notify,
}
```

The map is built once at startup from the flags and never changes, so no lock
guards the map itself — only the fields inside a `Display`.

This is a smaller change than it looks: `current_item_id` has four call sites,
`pending_jump` four, `override_item` eight and `browser_pid` two. The signals move
with them.

`browser_loop` becomes `browser_loop(state, display)`, spawned once per display.
Every rule in [CLAUDE.md](../../../CLAUDE.md#the-control-loop-srcbrowserrs) applies
unchanged per loop — notably that the loop owns what is on screen, that
`notify_one` is never `notify_waiters`, and that `pending_jump` is peeked and not
taken until the target is found in a freshly fetched playlist. **The playlist each
loop fetches is now filtered by its display's assigned `playlist_id`**, and a
display with no playlist assigned shows the idle screen.

## API

Playlists:

- `GET /api/playlists` — list, with item counts
- `POST /api/playlists` — create
- `PUT /api/playlists/{id}` — rename
- `DELETE /api/playlists/{id}` — refuses with `409` while it still holds items

Items keep their existing paths and gain a playlist scope:

- `GET /api/playlist?playlist_id=<id>` — the existing shape, filtered
- `POST /api/playlist` — body gains a required `playlist_id`
- `PUT`/`DELETE /api/playlist/{id}`, `POST /api/playlist/{id}/move` — unchanged,
  except that `move` renumbers within the item's own playlist

Displays:

- `GET /api/displays` — name, label, assigned playlist, whether its browser is
  currently attached
- `PUT /api/displays/{name}` — set `label` and `playlist_id`

Playback control and override become display-scoped:

- `GET`/`POST /api/displays/{name}/control/current`
- `GET`/`POST`/`DELETE /api/displays/{name}/override`

**The old unscoped paths stay**, resolving to the `default` display when exactly
one display is declared and answering `409` naming the available displays when
more are. A deployment that upgrades without changing its flags keeps whatever
scripts it has; one that declares two displays gets told why its script is now
ambiguous rather than having a coin flipped for it.

## The admin page

`playlist.html` gains a playlist selector; everything below it edits the selected
playlist and is otherwise unchanged.

A new `displays.html`, linked from `admin.html`, lists the declared displays with
their label, assigned playlist and attachment state. Assignment is a dropdown per
display. This is where decision 2 pays off: reassigning the removed screen's
playlist to the remaining screen is one selection.

Both follow the existing UI rules — vanilla JS, `el()` with `textContent` and
never `innerHTML`, a 2 s poll that updates only status and never re-renders a card
being edited.

## Casting, in the meantime

`--cast-display <name>` picks the display a cast pins, defaulting to the first
declared. `activate_display`/`deactivate_display` take that display rather than
the global override, which is a parameter change and not a redesign — a cast is
still an override, it just now says whose.

The guest URL, the QR code and `is_active()` keep their current single-session
meaning. Casting to a chosen screen, one session per display, and a per-display
QR are subproject 3.

## Webhooks

The envelope gains a `display` key beside `device`:

```json
{ "event": "playback.item_changed", "timestamp": "…", "device": "kiosk-pi-1",
  "display": "werkstatt", "data": { … } }
```

Additive, so an existing target keeps working. All ten events become
display-scoped, which is why this belongs in the envelope rather than in each
event's `data`. `api::catalogue()`'s `envelope` array gains it, so the admin
page's placeholder chips offer it without being told — the catalogue is the only
source, as
[CLAUDE.md](../../../CLAUDE.md#webhooks-srcwebhook) requires.

## Not in scope

- **Per-display casting.** Subproject 3.
- **Compositor discovery.** Decision 4. The prototype notes are in this document
  so that a later attempt starts from measurements rather than guesses.
- **Per-display settings or overlay.** The overlay configuration stays global;
  only its cast-QR resolution becomes per-display, and that arrives with
  subproject 3.
- **Fault isolation between displays.** One process means one crash takes every
  screen. Accepted deliberately; if it proves painful, the answer is a supervisor
  that restarts the process, not a second process.
- **Moving a running item between displays.** A playlist is assigned, not dragged.

## Testing

Rust unit tests:

- the backfill: items with no playlist land in `Standard`; a database that already
  has playlists is left alone; running it twice changes nothing
- deleting a playlist that holds items is refused; deleting one that does not
  unassigns it from any display
- display name to `app_id`, CDP port and profile path derivation, including the
  explicit-port form
- with no `--display`, exactly one display named `default` exists with today's
  defaults

`tests/cast/test_display.py`, stdlib-only, in the existing harness style — it
needs a real Chromium per declared display, so it belongs with `test_browser.py`
and `test_webhook.py` among the suites that launch one:

- two declared displays play their assigned playlists independently, and an item
  change on one does not disturb the other
- two displays assigned the *same* playlist both play it — mirroring
- a display with no playlist assigned shows the idle screen and does not error
- reassigning a playlist while it is playing takes effect on the next item
- an override on one display leaves the other alone
- the unscoped legacy paths answer `409` naming the displays when two are declared
- a webhook delivery carries the `display` key naming the right screen

## Documentation to update

- `README.md` — the API overview gains playlists and displays; *Two displays on
  one machine* is rewritten around `--display` and keeps the window-manager config
- `docs/features.md` — playlists and displays as concepts
- `docs/architecture.md` — the module map, and the control loop described as one
  per display
- `docs/deployment.md` — declaring displays, and the fault-isolation trade
- `CLAUDE.md` — per-display state, the derivation rules, the legacy-path
  behaviour, and why discovery was not taken
