# An item advances when its content ends

Status: implemented (designed and approved in conversation on 2026-09-24).

## What and why

Today every playlist item shows for a number of seconds. That is right for a
slide and wrong for anything whose length the operator does not know: a page
that scrolls, a PDF, a video whose length was never measured. The operator
guesses, and the screen either cuts the content off or sits at the bottom of it.

An item now says **when it moves on**: after a time, or after its content has
run through a number of times.

## The model

`duration` leaves the playlist item. In its place, `advance`:

```rust
#[serde(tag = "on", rename_all = "snake_case")]
pub enum Advance {
    Time { seconds: u32 },  // {"on": "time", "seconds": 30}
    Passes { count: u16 },  // {"on": "passes", "count": 2}
}
```

**What a pass is follows from the content; it is not stored.** A video: it
played to its end. Anything with a scroll mode other than `none` — a web page,
an image, a PDF: it reached the bottom (for a paged PDF, the last page) and
held there for its `return_delay`. Storing the kind beside the count would allow
"last page" on a video, a stored setting that silently does nothing.

**`Passes` needs something to count**: a video asset, or a scroll mode other
than `none`. Anything else is a `400` naming the reason — on create, on update,
and when an update changes the scroll mode or the source out from under an
existing `Passes`.

Bounds: `seconds` is clamped to `1..=604800` (seven days) like `duration` was (a negative
value once froze a playlist for 584 billion years); `count` to `1..=100`.

## API and storage

- `POST`/`PUT /api/playlist…` take `advance`; `duration` is gone. A request
  still sending `duration` is a `422` (`deny_unknown_fields`), not a silent
  `200` — the same treatment `PUT /api/displays/{name}` got for `playlist_id`.
- Items read back with `advance` and without `duration` or `asset_duration`.
- The item no longer inherits anything from its asset. `assets.duration` stays
  as what it has become — the **measured length of a video** — shown on the
  assets page and used by the playlist page to prefill `Time` when a video is
  added. It is no longer editable there, because nothing inherits it.
- Migration (`db::run_migrations`, probed like every column): add
  `playlist_items.advance TEXT NOT NULL DEFAULT '{"on":"time","seconds":10}'`,
  backfill `Time { seconds: duration ?? assets.duration ?? 10 }` — exactly what
  the loop computed until now, so every playlist plays as before — then drop
  `playlist_items.duration`. `COALESCE` on read like the other JSON columns.
- Webhook `playback.item_changed`: `duration` becomes `advance` (the object),
  and the catalogue says so.
- Proposals: the line reads `Weiter: 30 s → 2 Durchläufe`.

## The runtimes count

The scroll runtime (`web/autoscroll.js`), the PDF viewer and the media viewer
each publish the same small interface:

```js
globalThis.__advance = {
  state() { return { passes, progress_at }; },  // progress_at: performance.now()
  reset(count) { /* passes = 0, progress_at = now, target = count */ },
};
```

The loop calls `reset(count)` when the item starts, so the runtime knows the
target: **having completed the last pass it stays where it is** — at the
bottom, on the last page, on the video's last frame. The loop polls, so without
this the runtime would already have jumped back to the top (or restarted the
video) when the loop noticed, and the top would flash for up to one tick.

- **Scroll**: a pass completes on reaching the bottom and holding
  `return_delay`. Between passes it returns to the top as today; after the last
  it stays at the bottom.
- **PDF**: the viewer drives its own scrolling, so it counts itself: a pass is
  the bottom of the document, which in the paged fits is the last page.
- **Video**: in `Passes` mode the viewer is told (a query parameter) not to
  `loop`; it counts `ended` and restarts the video itself until the target. In
  `Time` mode it loops as today.
- **`progress_at` moves whenever the content is on schedule**: the scroll
  position changed, `currentTime` advanced, or the runtime is in a hold it
  chose (top delay, bottom delay, step delay). A deliberate pause is progress;
  a stuck one is not.
- **A minimum per pass** (`MIN_PASS`, 3 s): a page that fits the screen is at
  the bottom at once, and `count: 3` with no delays would otherwise flash.

## The loop decides

`browser.rs` owns the screen, as before. For a `Time` item nothing changes. For
a `Passes` item the per-item `select!` gains a tick (~500 ms) that evaluates
`globalThis.__advance?.state()` over CDP:

- `passes >= count` → next item.
- **Stalled** → next item and a `warn!` naming the item: no `__advance` at all
  (a CSP blocked the runtime, an error page), or `progress_at` older than the
  **stall timeout**. The timeout is `--advance-stall-timeout` (seconds, default
  120), a deployment-level flag with no runtime setting: it is fault handling,
  not a choice about content.
- The existing signals (`skip_signal`, `playlist_signal`, `override_signal`,
  `overlay_signal`, the timetable boundary) interrupt a `Passes` item exactly as
  they interrupt a `Time` one.
- `keep_loaded` tabs are brought to front, not navigated, so `reset(count)` on
  showing one is what starts the count — from when it is seen, not from when it
  was loaded.
- Overrides and casts have no `advance` and are untouched.

Probed like the other runtimes, never assumed: the tick tolerates a missing
object (that is the stall case) and a navigation under the evaluate.

## The operator page

The card's duration field becomes "Weiter nach: [Zeit ▾] [30] s" or
"[Durchläufen ▾] [2] ×". "Durchläufen" is disabled with a hint where there is
nothing to count, and re-checked when the scroll mode or the source changes.

## Tests

- Rust: the enum's JSON shape and bounds; the `Passes` validation (video,
  scroll mode, a change that invalidates it); the migration backfill from item
  duration, asset duration and neither.
- Python with a real Chrome: a scrolling page with `count: 2` advances after the
  second bottom and not the first; a video with `count: 2` after the second end;
  a paged PDF after its last page; a page whose runtime is gone (the test
  deletes `__advance`; the re-evaluation after navigation gets past a CSP, so a
  CSP is not a reliable way to provoke this) advances after the stall timeout,
  run with a short one; a page that fits the
  screen respects `MIN_PASS`; `duration` in a `PUT` is a `422`.

## Not in this

A total cap per item (the stall timeout covers the fault; a long video run
twice is not one), a "loop n times" for a time-based item, and advancing on an
external trigger.
