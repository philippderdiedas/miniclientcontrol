# Video length from the file

**Status:** implemented
**Date:** 2026-09-23

## What and why

Every upload gets a duration of 10 s, whatever it is. A 45-second video on a
playlist is cut off after ten seconds unless somebody notices and types the length
in by hand — and nothing tells them to.

Two findings shape the fix:

1. **A correct asset duration alone changes nothing today.** The add form sends
   `duration: 10` with every new item and the card sends its duration on every
   save, so every item created through the UI carries a fixed number, and the
   loop's fallback to the asset's duration (`item.duration.or(item.asset_duration)`)
   never fires for them.
2. **"Duration = video length" is not yet "ends with the video".** The item's
   clock starts only once the content is on screen — after the 700 ms target
   drain, the readiness wait (at least ~1 s of network idle), the overlay and a
   1.2 s pause (`item_started_at` in `browser.rs`) — while the video has been
   playing since it loaded (`autoplay`). With the duration set to the video's
   length the item outlasts the video by about three seconds, and because the
   viewer loops, the audience sees the start of the video again at the end.

## Decisions taken

1. **The browser measures, at upload.** `assets.html` reads each video's length
   from the local file through a `<video>` element's `loadedmetadata` and sends it
   with the upload. No new dependency on the device (no ffprobe, no container
   parser), and it understands exactly the formats Chromium plays — which is what
   the screen plays. An upload through the API without the field keeps the 10 s
   default.
2. **The item's duration is prefilled when it is created, and stored.** Picking an
   asset in the add form fills *Dauer* with the asset's length; the operator may
   change it; the item stores that number, as it does today. No "empty means the
   asset's length" state.
3. **The video and the item's clock start together.** The media viewer holds the
   video on its first frame until the controller starts the clock, and the
   controller starts the video at that same instant. With that, a duration equal
   to the video's length ends the item with the video, to a fraction of a second,
   without the loop having to wait on a page event — which is why "end the item
   when the video ends" is not a separate feature.

## Upload: `POST /api/assets` and `web/assets.html`

- **A multipart text field `duration` applies to the file parts that follow it**,
  until the next `duration` field. Multipart parts are ordered and the handler
  already reads them one after another, so the rule costs no buffering. The page
  sends, per file, `duration` first, then the file.
- The value is seconds as a decimal number (what `HTMLMediaElement.duration`
  gives). It is **rounded down** to whole seconds, at least 1, then passed through
  `clamp_duration`: a fraction of the last second cut off is invisible, a flash of
  the video starting over is not. A value that is not a finite positive number is
  ignored and the default applies — the upload itself never fails over it.
- The page measures with a 10 s timeout per file and only for `video/*`. A file it
  cannot measure — and every non-video — gets an **empty** `duration` field in front
  of it, which resets to the default; leaving the field out would hand it the
  previous file's length.

## Existing videos: *Länge ermitteln*

Each video asset in `assets.html` gets a button **Länge ermitteln** that measures
the uploaded file (`/uploads/<local_path>`) the same way and saves it through the
existing `PUT /api/assets/{id}` `{ duration }`. Without it every video already on
a device would stay at 10 s. The measuring function is shared with the upload
path, so the two cannot round differently.

## Creating an item: `web/playlist.html`

Choosing an asset in the add form sets the *Dauer* input to that asset's
`duration`. Nothing else about the form changes: the operator can overwrite the
number, and it is stored on the item. Swapping the asset on an existing card does
not touch its duration — that number was set by somebody on purpose.

## Starting the video with the clock

`web/media_viewer.html`:

- A video is created **without `autoplay`**, with `preload="auto"`, so it loads and
  shows its first frame.
- The page defines `globalThis.__media.start()`: set `currentTime = 0`, then
  `play()`, with the existing unmuted-first, muted-on-`NotAllowedError` fallback.
  Calling it again restarts from the beginning. For an image page it is a no-op.
- **Fallback:** if `start()` has not been called 20 s after load, the page starts
  the video itself, so a page opened outside the controller never stands on a
  still frame.

`src/browser.rs`:

- Where the item loop sets `item_started_at`, it first evaluates
  `globalThis.__media && globalThis.__media.start()` on the active page — probed,
  not assumed, like the scroll and overlay runtimes, so on any other page it does
  nothing. Evaluated on every item, not only for video URLs: the probe is the
  check.
- `run_override_loop` does the same after its readiness wait, so a video override
  starts from the beginning too.
- A `keep_loaded` video tab is started the same way each time its item comes
  round, so it begins from the start rather than wherever it was left.

## Testing

**Rust (`#[cfg(test)]`):** the rounding function — `37.8 → 37`, `0.4 → 1`, `NaN`,
`inf`, `-3` and `"abc"` → no value (default applies).

**Python, `tests/cast/test_media.py`:**

- API: an upload with `duration=37.8` before the file gets asset duration 37; a
  second file after a second field `duration=5` gets 5; a file without a field gets
  10; `duration=abc` gets 10 and the upload succeeds.
- Browser: a short real video is generated in the test (a few seconds of WebM from
  a canvas via `MediaRecorder` in the test's headless Chrome, so no fixture file and
  no ffmpeg), uploaded through `assets.html`'s own upload path, and its asset
  duration equals the recorded length (rounded down).
- Viewer: on a video item the video is paused at `currentTime` 0 until the loop
  starts the item, and after the start `currentTime` is well under one second
  while `performance.now()` is seconds past load — the clock and the video started
  together.

## Not in scope

- ffprobe or a container parser on the device.
- Lengths for images and PDFs; they keep their default.
- Changing a card's duration when its asset changes.
- Ending an item on a page event; decision 3 makes it unnecessary.

## Amendment made while implementing

The browser case measures in a Chrome of its own (CDP 9253), not in a second tab
of the display's: the control loop brings its page to the front on every item,
and Chrome defers loading media in a tab that is not in front, so the measuring
timed out there. An operator uploading is looking at the page, which is what the
separate browser models.
