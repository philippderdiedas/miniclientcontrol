# Dayparting: which playlist a screen shows, by weekday and time of day

**Status:** designed
**Date:** 2026-09-23

## What and why

A screen is assigned one playlist, and an item has an optional date window. There
is no way to say "Monday to Friday 8:00–18:00 the office playlist, otherwise the
night one", which is the most common thing a venue asks a signage system to do
after "show these pages".

## Decisions taken

1. **The timetable belongs to the display's assignment** — not to the playlist
   and not to the item. A playlist with opening hours leaves open what the screen
   shows outside them, and two screens sharing a playlist may want different
   hours; hours on every item is one schedule typed N times, and the item's date
   window already covers "this notice until Friday".

2. **Structured windows, not cron.** Cron describes instants, a window needs a
   start and an end. Used as a minute filter it works, but "8:00–18:30" already
   needs two expressions, the day-of-month/weekday OR rule is a trap, a hand
   written expression cannot always be turned back into a builder's checkboxes,
   and overlap detection becomes a minute-by-minute scan. A window is weekdays
   plus from/to; if more is ever wanted (months, a date range) it grows optional
   fields and the UI stays one to one with the data.

3. **List order is priority.** The first window in the list that matches now
   wins. Overlaps are allowed and the UI **warns** about them rather than refusing
   them — an operator may overlap on purpose ("the lunch window beats the day
   window").

4. **A window boundary switches immediately.** At 18:00:00 the item on screen is
   left and the new playlist starts at its beginning — exactly what a manual
   reassignment in the admin already does. A scheduled switch is a reassignment
   the clock makes.

## The model

`displays.playlist_id` stays, unchanged, and is now read as the **default
playlist**: what the screen shows when no window matches. Every existing
installation keeps behaving exactly as before, because it has no windows.

A new table, created in `db::run_migrations`:

```sql
CREATE TABLE IF NOT EXISTS display_slots (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    display      TEXT NOT NULL REFERENCES displays(name) ON DELETE CASCADE,
    position     INTEGER NOT NULL,
    weekdays     INTEGER NOT NULL,   -- bit 0 = Monday … bit 6 = Sunday
    start_minute INTEGER NOT NULL,   -- 0..=1439
    end_minute   INTEGER NOT NULL,   -- 1..=1440
    playlist_id  INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE
);
```

- **Deleting a playlist deletes its windows.** A window without a playlist means
  nothing, and keeping it would be a row that silently does nothing. The default
  keeps its existing `ON DELETE SET NULL`. `PRAGMA foreign_keys` is already on per
  connection, which is what makes both work.
- **A window ending at or before its start crosses midnight.** 22:00–06:00 on
  Friday runs from Friday 22:00 to Saturday 06:00: the weekdays name the day a
  window *starts*.
- **`start == end` is refused.** A whole day is 00:00–24:00.
- **Time is the device's local time** (`chrono::Local`). On the two days a year the
  clocks change, a boundary inside the shifted hour happens an hour early, late or
  twice; that is accepted rather than engineered around.
- At most **50 windows per display**, refused past that. A timetable longer than
  that is a mistake, and the list is re-read on every wake of the loop.

## Resolution: `src/schedule.rs`

A new module with pure functions and no database access, so the rules are unit
tested without a runtime:

- `Slot { weekdays: u8, start_minute: u16, end_minute: u16, playlist_id: i64 }`
- `active(default: Option<i64>, slots: &[Slot], now: NaiveDateTime) -> Active`,
  where `Active { playlist_id: Option<i64>, slot: Option<usize> }` — the first
  matching slot in order, else the default with `slot: None`. A slot matches when
  `now` falls in `[start, end)` on a listed weekday, or, for a slot crossing
  midnight, in `[start, 24:00)` on a listed weekday or `[00:00, end)` on the day
  after one.
- `next_boundary(slots: &[Slot], now: NaiveDateTime) -> Option<NaiveDateTime>` —
  the soonest instant strictly after `now` at which any slot starts or ends,
  looking at most eight days ahead. `None` when there are no slots.
- `overlaps(slots: &[Slot]) -> Vec<(usize, usize)>` — pairs `(i, j)`, `i < j`,
  whose weekly minute ranges intersect **and** whose playlists differ, meaning
  `i` hides `j` where they meet. Overlapping windows naming the same playlist are
  harmless and not reported. Computed by expanding each slot into ranges on a
  0..10080 week-minute axis, splitting a midnight crossing (and a Sunday-night
  crossing into Monday) in two.

`display.rs` gains the one function the rest of the code calls:
`active_playlist(pool, name) -> Result<Active, sqlx::Error>`, reading the default
and the slots (ordered by `position`) and handing them to `schedule::active` with
`Local::now()`. **It is the only resolver**, in the same sense as
`display::resolve`: nothing else reads `displays.playlist_id` to decide what to
play.

## The control loop

Two places read the assignment today, and both switch to `active_playlist`:

- The top of every inner pass: `SELECT playlist_id FROM displays WHERE name = ?`
  becomes `active_playlist`. The existing "an assignment that *changed* restarts
  at the playlist's beginning" rule (`last_assigned`, `resume_after_order`) then
  covers a window change for free.
- `is_playlist_item_active_now`, which joins `displays.playlist_id` today, takes
  the resolved playlist id instead (`p.playlist_id = ?`), so the item on screen
  is judged against the playlist that is live *now*.

And one new wake-up: the per-item `select!` gets a branch
`sleep_until(next_boundary)`. The boundary is computed from a fresh read of the
slots **on every pass of the `while !remaining.is_zero()` loop**, not once per
item, so an edited timetable (whose `PUT` pokes `playlist_signal`, which wakes that
loop) never leaves a stale timer behind. When the branch fires, the loop resolves
again; a different playlist breaks out of the item exactly like a reassignment,
the same playlist (two adjacent windows naming it) recomputes the remaining time
and carries on. The idle screen already re-reads every five seconds, and an
override (a cast, a pinned page) is untouched: it outranks the playlist and its
loop never reads the assignment.

`notify_one`, never `notify_waiters`, for the `playlist_signal` poke — the rule
that applies to every signal on `Display`.

## API

No new route. The timetable travels with the display it belongs to, so one
"Speichern" is one request and one transaction:

- **`GET /api/displays`** adds, per display:
  - `schedule`: `[{ "weekdays": [1,2,3,4,5], "from": "08:00", "to": "18:00", "playlist_id": 3 }, …]`
    in priority order; weekdays are ISO numbers, Monday = 1 … Sunday = 7; `to`
    may be `"24:00"`
  - `overlaps`: `[[0, 2], …]` — index pairs from `schedule::overlaps`
  - `active_playlist_id` and `active_slot` (index or `null`) — what is live now
- **`PUT /api/displays/{name}`** accepts an optional `schedule` with the same
  shape, **replacing the whole list** in the same transaction as `label` and
  `playlist_id`. Refused with `400` and a German `{ "error": … }` naming the row:
  no weekday, a weekday outside 1–7, a time that is not `HH:MM` (or `24:00` as
  `to`), `from == to`, a playlist that does not exist, more than 50 rows. The
  playlist check happens in the statement that writes the row
  (`INSERT … SELECT … WHERE EXISTS`), for the same reason the item move does it:
  a check before the write leaves a window for the playlist to go.
  The response carries `overlaps` so the page can warn straight after saving.
  The `PUT` pokes `playlist_signal` when `schedule` was sent, as it does for
  `playlist_id`.

The route stays **operator-only**: in neither `is_display_path` nor
`cast::is_cast_public_path`. The display browser never asks what it should play —
the controller drives it — and a guest has no business reading the venue's hours.

## UI: `web/displays.html`

Each display card gets a **Zeitplan** section below the existing playlist choice,
which is relabelled **Standard-Playlist (wenn kein Zeitfenster passt)**:

- one row per window: seven checkboxes Mo–So, **Von** and **Bis** (`<input
  type="time">`, with `24:00` offered as "Tagesende"), a playlist select, ↑ / ↓ to
  reorder, and a delete button
- **+ Zeitfenster** appends a row: Mo–Fr, 08:00–18:00, the first playlist
- a yellow hint under an overlapping pair, from the server's `overlaps`:
  „Zeile 1 verdeckt Zeile 3, wo sie sich überschneiden“
- **Jetzt aktiv:** the playlist live now, and which window made it so (or
  „Standard“)

Rows are built with `createElement`/`textContent`, never `innerHTML`. The card's
existing per-card `dirty` rule covers the section: the page polls every two
seconds and rebuilds every card except the ones being edited, so a half-typed
window survives the poll and a display that loses its `declared` status still
shows up on every other card.

## Testing

**Rust (`#[cfg(test)]`):**

- `schedule::active`: the first matching slot wins over a later one; the default
  applies outside every slot; a weekday not listed does not match; a midnight
  crossing matches late on its start day and early on the next, not early on its
  start day; Sunday 22:00–06:00 matches Monday 05:59; `[start, end)` — 18:00 is
  outside 08:00–18:00.
- `schedule::next_boundary`: the next start, the next end, across midnight and
  across the week; `None` with no slots.
- `schedule::overlaps`: overlapping windows with different playlists are
  reported, with the same playlist are not, adjacent windows (08–12, 12–18) are
  not, a midnight crossing overlapping a morning window on the next day is.
- Migration: the table exists; deleting a playlist deletes its windows and leaves
  the display's default at `NULL`.

**Python, `tests/cast/`:**

- API (plain HTTP): a schedule round-trips in order; each refusal is a `400`
  that names the row and writes nothing (the default in the same request
  included); `overlaps` comes back; replacing the list removes rows no longer
  sent.
- End to end in `test_display.py`'s two-screen harness, on the **non-primary**
  screen (the standing rule: a case that exercises `displays[0]` passes with the
  resolution replaced by `state.primary()`):
  - a window covering now switches that screen to its playlist, and the other
    screen keeps its own;
  - a window starting at the next full minute interrupts the item on screen
    within a few seconds of that minute, not at the end of the item (item
    duration 600 s). This case takes up to a minute by design.

## Not in scope

- Date ranges and holidays on a window. The item's date window covers campaigns;
  a window can grow optional date fields later without changing the rest.
- A webhook event for a window change — `playback.item_changed` already reports
  what the screen switched to.
- Switching the panel on and off.
- Cron, see decision 2.
