# Video Length Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A video's real length becomes its asset duration at upload, the add form prefills an item's duration from it, and the video starts together with the item's clock so the item ends with the video.

**Architecture:** `assets.html` measures a video with a `<video>` element and sends the length as a multipart `duration` field before the file; `upload_asset` applies it to the following file parts. `media_viewer.html` holds a video on its first frame and exposes `globalThis.__media.start()`; `browser.rs` calls it where the item's clock starts and in the override loop.

**Tech Stack:** Rust (axum multipart), vanilla HTML/JS, stdlib Python end-to-end tests.

**Spec:** `docs/superpowers/specs/2026-09-23-video-length-design.md`

## Global Constraints

- `web/` is compiled in — `cargo build` after every change under `web/` before any Python test.
- Measured seconds are **rounded down**, at least 1, then `clamp_duration`. Anything not a finite positive number → the default of 10.
- A `duration` field applies to the file parts **after** it, until the next `duration` field. The page sends a `duration` field (possibly empty) before **every** file, so a value never carries over to a file it was not measured for.
- Measuring: `video/*` only, 10 s timeout per file, and a file whose header has no duration (`Infinity`) is resolved by seeking far past the end.
- `__media.start()` is probed, never assumed; on any page but the media viewer it does not exist.
- Fallback: a video not started 20 s after load starts itself.
- UI built with `createElement`/`textContent`, never `innerHTML` interpolation.
- No `Co-Authored-By` / `Claude-Session` trailers.
- Stop any local instance before Python suites; `test_media.py` uses HTTP 3051 / CDP 9252.

## File Structure

| File | Change |
|---|---|
| `src/handlers.rs` | `seconds_from_measured`, `duration` field in `upload_asset` |
| `web/assets.html` | `measureVideo`, measuring upload, *Länge ermitteln* |
| `web/playlist.html` | add form prefills *Dauer* |
| `web/media_viewer.html` | hold the video, `__media` |
| `src/browser.rs` | `start_media`, two call sites |
| `tests/cast/test_media.py` | `[110]`–`[112]` |
| `README.md`, `docs/features.md`, `CLAUDE.md`, `docs/roadmap.md`, `tests/cast/README.md`, spec | docs |

---

### Task 1: The upload takes a measured length

**Files:** `src/handlers.rs` (`upload_asset` ~205-290, a new function beside `clamp_duration` ~29, tests module), `tests/cast/test_media.py` (`api_flow`)

**Interfaces:** Produces `pub(crate) fn seconds_from_measured(raw: &str) -> Option<i64>` and the multipart rule above.

- [ ] **Step 1: Failing tests**

In the `#[cfg(test)] mod tests` of `src/handlers.rs`:

```rust
    #[test]
    fn a_measured_length_is_rounded_down_to_whole_seconds() {
        assert_eq!(seconds_from_measured("37.8"), Some(37));
        assert_eq!(seconds_from_measured(" 5 "), Some(5));
        // A clip shorter than a second still plays for one.
        assert_eq!(seconds_from_measured("0.4"), Some(1));
        for junk in ["", "abc", "NaN", "inf", "-3", "0"] {
            assert_eq!(seconds_from_measured(junk), None, "{junk:?}");
        }
    }
```

In `tests/cast/test_media.py`, add below `upload`:

```python
def upload_parts(parts, port=None):
    """One multipart POST to /api/assets with parts in the order given:
    ("duration", "37.8") for a text field, ("file", name, data, mimetype) for a
    file. Returns {filename: asset} for the files in it."""
    port = port or 3021
    boundary = "----mcc" + uuid.uuid4().hex
    body = b""
    for part in parts:
        if part[0] == "duration":
            body += (f"--{boundary}\r\nContent-Disposition: form-data; name=\"duration\"\r\n\r\n"
                     f"{part[1]}\r\n").encode()
        else:
            _, name, data, mimetype = part
            body += (f"--{boundary}\r\n"
                     f"Content-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n"
                     f"Content-Type: {mimetype}\r\n\r\n").encode() + data + b"\r\n"
    body += f"--{boundary}--\r\n".encode()
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/api/assets", data=body, method="POST",
        headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    with urllib.request.urlopen(req, timeout=10) as res:
        json.load(res)
    names = {p[1] for p in parts if p[0] == "file"}
    return {row["filename"]: row for row in http("GET", "/api/assets", port=port)[1]
            if row["filename"] in names}
```

and at the end of `api_flow`'s `with Server():` block:

```python
        print("\n[110] an upload carries the length measured in the browser")
        stored = upload_parts([
            ("duration", "37.8"), ("file", "clip-a.mp4", b"a", "video/mp4"),
            ("duration", "5"), ("file", "clip-b.mp4", b"b", "video/mp4"),
            ("file", "clip-c.mp4", b"c", "video/mp4"),
            ("duration", ""), ("file", "poster-d.png", PNG, "image/png"),
            ("duration", "abc"), ("file", "clip-e.mp4", b"e", "video/mp4"),
        ])
        check("a length is rounded down", stored["clip-a.mp4"]["duration"] == 37, stored["clip-a.mp4"])
        check("a second field applies to the file after it",
              stored["clip-b.mp4"]["duration"] == 5, stored["clip-b.mp4"])
        check("and keeps applying until the next field",
              stored["clip-c.mp4"]["duration"] == 5, stored["clip-c.mp4"])
        check("an empty field resets to the default", stored["poster-d.png"]["duration"] == 10,
              stored["poster-d.png"])
        check("nonsense is the default, and the upload still succeeds",
              stored["clip-e.mp4"]["duration"] == 10, stored["clip-e.mp4"])
        stored = upload_parts([("file", "clip-f.mp4", b"f", "video/mp4")])
        check("no field at all is the default", stored["clip-f.mp4"]["duration"] == 10,
              stored["clip-f.mp4"])
```

- [ ] **Step 2: Run, see them fail**

`cargo test a_measured_length` → compile error (no function). `cargo build && cd tests/cast && python3 -c "import test_media as t; t.api_flow()" | grep -E "FAIL|\[110"; cd ../..` → `[110]` checks fail (everything is 10).

- [ ] **Step 3: Implement**

`src/handlers.rs`, after `clamp_duration`:

```rust
/// A length measured by the upload page, as whole seconds: rounded down, at
/// least 1. Down, because a fraction of the last second cut off is invisible
/// and a flash of the video starting over is not. `None` for anything that is
/// not a finite positive number, which leaves the default in place.
pub(crate) fn seconds_from_measured(raw: &str) -> Option<i64> {
    let value: f64 = raw.trim().parse().ok()?;
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    Some((value.floor() as i64).max(1))
}
```

In `upload_asset`, before the `while` loop:

```rust
    // A `duration` field applies to the file parts after it, until the next one:
    // multipart parts are ordered and read one after another, so this needs no
    // buffering. The upload page sends one before every file, measured or empty.
    let mut measured: Option<i64> = None;
```

Replace

```rust
        // Plain (non-file) form fields carry no filename; they are not assets.
        let Some(raw_filename) = field.file_name().map(|f| f.to_string()) else {
            continue;
        };
```

with

```rust
        // Plain (non-file) form fields carry no filename; they are not assets.
        // The one plain field that means something is the measured length.
        let Some(raw_filename) = field.file_name().map(|f| f.to_string()) else {
            if field.name() == Some("duration") {
                measured = field.text().await.ok().as_deref().and_then(seconds_from_measured);
            }
            continue;
        };
```

and replace `let default_duration = 10;` with

```rust
        let default_duration = measured.map(clamp_duration).unwrap_or(10);
```

(the `INSERT` already binds `default_duration`).

- [ ] **Step 4: Run** — `cargo test` green; the `[110]` checks pass.

- [ ] **Step 5: Commit** — `git add src/handlers.rs tests/cast/test_media.py && git commit -m "Take a measured length with an upload"`

---

### Task 2: The page measures, at upload and on demand; the add form prefills

**Files:** `web/assets.html` (script), `web/playlist.html` (add form init), `tests/cast/test_media.py` (`browser_flow`)

- [ ] **Step 1: Failing browser case**

In `tests/cast/test_media.py`, add a module-level helper:

```python
RECORD = """(async () => {
  // A real, playable WebM of about three seconds, recorded in this page from an
  // animated canvas -- no fixture file and no ffmpeg. MediaRecorder writes no
  // duration into the header, which is exactly the case the page must handle.
  const canvas = document.createElement('canvas');
  canvas.width = 160; canvas.height = 90;
  const ctx = canvas.getContext('2d');
  let frame = 0;
  const paint = setInterval(() => {
    ctx.fillStyle = `hsl(${(frame++ * 12) % 360}, 80%, 50%)`;
    ctx.fillRect(0, 0, 160, 90);
  }, 40);
  const recorder = new MediaRecorder(canvas.captureStream(25), { mimeType: 'video/webm' });
  const chunks = [];
  recorder.ondataavailable = (e) => chunks.push(e.data);
  const stopped = new Promise((resolve) => { recorder.onstop = resolve; });
  recorder.start(200);
  await new Promise((resolve) => setTimeout(resolve, 3200));
  recorder.stop();
  await stopped;
  clearInterval(paint);
  const file = new File(chunks, 'recorded.webm', { type: 'video/webm' });
  const input = document.getElementById('fileInput');
  const transfer = new DataTransfer();
  transfer.items.add(file);
  input.files = transfer.files;
  document.getElementById('uploadBtn').click();
  return file.size;
})()"""
```

and at the end of `browser_flow`, after `[109]`, dedented to the function body:

```python
    print("\n[111] the upload page measures a video and the length is stored")
    with urllib.request.urlopen(urllib.request.Request(
            f"http://127.0.0.1:{CDP}/json/new?about:blank", method="PUT"), timeout=5) as res:
        tab = json.load(res)
    async with cdp.Session(tab["webSocketDebuggerUrl"]) as admin:
        await admin.call("Page.navigate", {"url": f"http://127.0.0.1:{HTTP}/assets.html"})
        await asyncio.sleep(1.5)
        size = await admin.eval(RECORD, timeout=30)
        check("a video was recorded in the page", (size or 0) > 1000, size)
        recorded = wait_for(lambda: next((a for a in http("GET", "/api/assets", port=HTTP)[1]
                                          if a["filename"] == "recorded.webm"), None), 20)
        check("the recording was uploaded", recorded is not None)
        check("with its measured length, not the default",
              recorded and recorded["duration"] in (2, 3), recorded)

        print("\n[111b] an existing video can be measured again")
        http("PUT", f"/api/assets/{recorded['id']}", {"duration": 10}, port=HTTP)
        await admin.call("Page.reload", {})
        await asyncio.sleep(1.5)
        clicked = await admin.eval(f"""(() => {{
            const row = [...document.querySelectorAll('#assetsBody tr')]
              .find((tr) => tr.firstChild && tr.firstChild.textContent === '{recorded['id']}');
            const button = row && [...row.querySelectorAll('button')]
              .find((b) => b.textContent === 'Länge ermitteln');
            if (!button) return false;
            button.click();
            return true;
        }})()""")
        check("a video row has the button", clicked is True, clicked)
        remeasured = wait_for(lambda: (lambda d: d if d != 10 else None)(
            next(a for a in http("GET", "/api/assets", port=HTTP)[1]
                 if a["id"] == recorded["id"])["duration"]), 20)
        check("and it writes the length back", remeasured in (2, 3), remeasured)

        print("\n[111c] picking the asset in the add form fills in its length")
        await admin.call("Page.navigate", {"url": f"http://127.0.0.1:{HTTP}/playlist.html"})
        await asyncio.sleep(2)
        filled = await admin.eval(f"""(() => {{
            const pick = document.getElementById('addAsset');
            pick.value = '{recorded['id']}';
            pick.dispatchEvent(new Event('change', {{ bubbles: true }}));
            return document.getElementById('addDuration').value;
        }})()""")
        check("Dauer shows the asset's length", filled in ("2", "3"), filled)
    urllib.request.urlopen(f"http://127.0.0.1:{CDP}/json/close/{tab['id']}", timeout=5)
```

- [ ] **Step 2: Run, see it fail** — `cargo build && cd tests/cast && python3 test_media.py | grep -E "FAIL|\[111"; cd ../..` → the length checks fail (10), no button, no prefill.

- [ ] **Step 3: `assets.html`**

Add above `async function updateDuration`:

```js
    // A video's length in seconds, or null. `loadedmetadata` gives it for most
    // files; one whose header carries none -- a recording straight out of
    // MediaRecorder, for one -- reports Infinity there, and seeking far past the
    // end makes the browser work the real length out. Muted and never shown:
    // this only reads the file.
    function measureVideo(src, timeoutMs = 10000) {
      return new Promise((resolve) => {
        const video = document.createElement('video');
        video.preload = 'metadata';
        video.muted = true;
        let settled = false;
        const done = (value) => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          video.removeAttribute('src');
          video.load();
          resolve(Number.isFinite(value) && value > 0 ? value : null);
        };
        const timer = setTimeout(() => done(null), timeoutMs);
        video.addEventListener('error', () => done(null));
        video.addEventListener('loadedmetadata', () => {
          if (Number.isFinite(video.duration)) { done(video.duration); return; }
          video.addEventListener('durationchange', () => {
            if (Number.isFinite(video.duration)) done(video.duration);
          });
          video.currentTime = 1e101;
        });
        video.src = src;
      });
    }

    async function remeasure(asset, feedback) {
      flash(feedback, 'messe…');
      const seconds = await measureVideo(`/uploads/${encodeURIComponent(asset.local_path)}`);
      if (seconds === null) { flash(feedback, 'Länge nicht lesbar', true); return; }
      // Rounded down like the server does for an upload, so the two agree.
      const duration = Math.max(1, Math.floor(seconds));
      const res = await fetch(`/api/assets/${asset.id}`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ duration }),
      });
      if (!res.ok) { flash(feedback, `Speichern fehlgeschlagen (HTTP ${res.status})`, true); return; }
      flash(feedback, `${duration} s ✓`);
      await loadAssets();
    }
```

In the row builder, after `delBtn` is created, add:

```js
        const extra = [];
        if ((asset.mimetype || '').startsWith('video/')) {
          const measureBtn = document.createElement('button');
          measureBtn.textContent = 'Länge ermitteln';
          measureBtn.addEventListener('click', () => remeasure(asset, feedback));
          extra.push(' ', measureBtn);
        }
```

and change `actions.append(saveBtn, ' ', delBtn, feedback);` to `actions.append(saveBtn, ' ', delBtn, ...extra, feedback);`.

In the upload handler, replace

```js
      const fd = new FormData();
      const expected = input.files.length;
      for (const f of input.files) fd.append('file', f);

      const msg = document.getElementById('uploadMsg');
      msg.textContent = 'Uploading...';
```

with

```js
      const fd = new FormData();
      const expected = input.files.length;
      const msg = document.getElementById('uploadMsg');
      msg.textContent = 'Messe Videolängen…';
      // A `duration` field before *every* file, empty when there is nothing to
      // say: the server applies one to the files after it, so skipping it for an
      // image would hand the image the previous video's length.
      for (const f of input.files) {
        let seconds = null;
        if ((f.type || '').startsWith('video/')) {
          const url = URL.createObjectURL(f);
          seconds = await measureVideo(url);
          URL.revokeObjectURL(url);
        }
        fd.append('duration', seconds === null ? '' : String(seconds));
        fd.append('file', f);
      }
      msg.textContent = 'Uploading...';
```

- [ ] **Step 4: `playlist.html`**

After `const addScroll = scrollEditor(null);` and its `append`, add:

```js
    // Picking an asset fills in its length -- a video's is measured when it is
    // uploaded -- and the operator may still change it before adding. Stored on
    // the item as typed, like every duration.
    document.getElementById('addAsset').addEventListener('change', (e) => {
      const asset = assets.find((a) => String(a.id) === e.target.value);
      if (asset && asset.duration) document.getElementById('addDuration').value = asset.duration;
    });
```

- [ ] **Step 5: Run** — `cargo build && cd tests/cast && python3 test_media.py | grep -E "FAIL|ALL PASSED|FAILED"; cd ../..` → `ALL PASSED`.

- [ ] **Step 6: Commit** — `git add web/assets.html web/playlist.html tests/cast/test_media.py && git commit -m "Measure a video's length in the browser and prefill it"`

---

### Task 3: The video starts with the item's clock

**Files:** `web/media_viewer.html`, `src/browser.rs` (new `start_media`; before `let item_started_at = Instant::now();` ~525; in `run_override_loop` after the overlay apply ~770), `tests/cast/test_media.py`

**Interfaces:** `globalThis.__media = { start(), state() }`, `state()` → `{ kind, startedBy: 'controller'|'fallback'|null, startedAt: ms|null }`.

- [ ] **Step 1: Failing case** — at the end of `browser_flow`, after `[111c]`:

```python
    print("\n[112] a video item starts with the item's clock, not at load")
    playlist = a_playlist(port=HTTP)
    http("POST", "/api/playlist", {"asset_id": recorded["id"], "playlist_id": playlist,
                                   "duration": 30}, port=HTTP)
    video_item = http("GET", "/api/playlist", port=HTTP)[1][-1]["id"]
    http("POST", "/api/control/current", {"item_id": video_item}, port=HTTP)
    ws_url, _ = cdp.page_ws(CDP)
    async with cdp.Session(ws_url) as page:
        state = None
        for _ in range(100):
            try:
                state = json.loads(await page.eval(
                    "JSON.stringify(globalThis.__media ? globalThis.__media.state() : null)"))
                if state and state.get("kind") == "video" and state.get("startedBy"):
                    break
            except Exception:
                pass
            await asyncio.sleep(0.2)
        check("the controller started it", state and state.get("startedBy") == "controller", state)
        # Past the target drain and the readiness wait: a video that started on
        # load would report a few hundred milliseconds here.
        check("well after the page loaded, where the item's clock starts",
              state and (state.get("startedAt") or 0) > 1500, state)
        playing = json.loads(await page.eval("""JSON.stringify((() => {
            const v = document.getElementById('media');
            return { paused: v.paused, t: v.currentTime, controls: v.controls };
        })())"""))
        check("and it is playing from near the beginning", not playing["paused"] and playing["t"] < 5,
              playing)
```

- [ ] **Step 2: Run, see it fail** — `startedBy` never appears (`__media` does not exist).

- [ ] **Step 3: `media_viewer.html`**

Replace the whole `if (kind === 'video') { … }` block (controls/autoplay/loop/canplay) with:

```js
      // Set by `start()`: who started the video and when, for the tests and for
      // anybody reading the page's state over CDP.
      let started = null;

      if (kind === 'video') {
        // No `controls`: the bar is the complaint this page exists for. `loop`
        // unconditionally -- the item's duration ends it either way. No
        // `autoplay`: the video waits on its first frame for the controller,
        // which starts it the moment the item's clock starts -- otherwise it has
        // been playing for the seconds the readiness wait takes, and an item as
        // long as the video would show its beginning again at the end.
        media.controls = false;
        media.loop = true;
        media.playsInline = true;
        media.preload = 'auto';
      }

      // Unmuted first, muted only if the browser refuses. The controller starts
      // Chromium with --autoplay-policy=no-user-gesture-required, but a browser
      // started outside it may not have the flag, and a silent video is better
      // than a black screen.
      const play = () => {
        const attempt = media.play();
        if (attempt && attempt.catch) {
          attempt.catch((err) => {
            if (err && err.name === 'NotAllowedError') {
              media.muted = true;
              media.play().catch(() => {});
            }
          });
        }
      };

      const start = (by) => {
        if (kind !== 'video') return false;
        started = { by, at: Math.round(performance.now()) };
        media.currentTime = 0;
        play();
        return true;
      };

      globalThis.__media = {
        start: () => start('controller'),
        state: () => ({
          kind,
          startedBy: started ? started.by : null,
          startedAt: started ? started.at : null,
        }),
      };

      // Nobody driving this page -- opened by hand, or a controller that never
      // gets to it -- must not leave it on a still frame for ever.
      if (kind === 'video') {
        setTimeout(() => { if (!started) start('fallback'); }, 20000);
      }
```

- [ ] **Step 4: `browser.rs`**

Add beside `apply_overlay_payload`:

```rust
/// Start the media viewer's video now, with the item's clock.
///
/// Probed, like the scroll and overlay runtimes: on any page but the media
/// viewer `__media` does not exist and this does nothing, which is why it is
/// called for every item rather than only for video URLs. A failure is not
/// worth a log above debug -- the page starts itself after 20 s.
async fn start_media(page: &Page) {
    if let Err(e) = page
        .evaluate("(() => { if (globalThis.__media) globalThis.__media.start(); })()")
        .await
    {
        debug!("Could not start the media on this page: {}", e);
    }
}
```

In the item loop, directly before `let item_started_at = Instant::now();`:

```rust
                // The video and the clock start together, so an item as long as
                // its video ends with it instead of showing its start again.
                start_media(&active_page).await;
```

In `run_override_loop`, directly after the `apply_overlay_payload(page, &overlay)` block:

```rust
        start_media(page).await;
```

- [ ] **Step 5: Run** — `cargo build && cargo test && cd tests/cast && python3 test_media.py | grep -E "FAIL|ALL PASSED|FAILED"; python3 test_overlay.py | tail -1; cd ../..` → both `ALL PASSED`.

- [ ] **Step 6: Commit** — `git add web/media_viewer.html src/browser.rs tests/cast/test_media.py && git commit -m "Start a video together with its item's clock"`

---

### Task 4: Docs

- [ ] **`README.md`** — under `POST /api/assets`, add: "an optional text field `duration` (seconds, decimal) applies to the file parts after it until the next one; rounded down to whole seconds, at least 1; anything else is the default 10. The upload page measures each video and sends it."
- [ ] **`docs/features.md`** (*Assets*) — add: "A video's length is measured when it is uploaded and becomes its duration, and picking it for a playlist item fills that in. The video starts together with the item's clock, so an item as long as its video ends with it. *Länge ermitteln* re-measures a video that was uploaded before."
- [ ] **`CLAUDE.md`** — after the paragraph on PDFs being the exception to scrolling, add:

```markdown
**The media viewer holds a video until `__media.start()`.** `browser.rs` calls
`start_media` where the item's clock starts (and in the override loop), because
the readiness waits take seconds and an autoplaying video would have run that
long already — an item as long as its video would show the start again at the
end. Probed like the other runtimes; the page starts itself after 20 s so an
undriven page is never a still frame. An upload's `duration` field applies to
the file parts after it, so the upload page sends one — empty if need be —
before every file.
```

- [ ] **`docs/roadmap.md`** — delete the *Video length from the file* entry.
- [ ] **`tests/cast/README.md`** — `test_media.py` line: "image fit, a video without controls, and a video's length (needs Chrome)"; note that `[111]` records a three-second WebM in the page.
- [ ] **Spec** — `**Status:** designed` → `**Status:** implemented`.
- [ ] **Commit** — `git add -A README.md docs/ CLAUDE.md tests/cast/README.md && git commit -m "Document video length"`
