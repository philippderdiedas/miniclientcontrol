"""How an image or video sits on the screen, and a video with no control bar.

The first half is plain HTTP: the fit is stored per playlist item, falls back
when it is nonsense, and a background that is not a colour is refused. The
second half drives a real Chrome through the real browser_loop, because the
whole point of the feature is what the display draws -- and a stored value
proves nothing about that.
"""
import asyncio, base64, json, os, shutil, subprocess, sys, time, urllib.request, uuid
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cdp
from test_cast import Server, check, failures, http

SP = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.join(SP, "..", "..", "target", "debug", "miniclientcontrol")
CHROME = "/usr/bin/google-chrome-stable"
HTTP, TLS, CDP = 3051, 3494, 9252
# The operator pages' own browser, case [111].
ADMIN_CDP = 9253

# One transparent pixel. Enough for every case here: `scroll` draws it at full
# width, so a 1x1 image becomes 1280x1280 on a 1280x720 window and the document
# really is taller than the screen.
PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")



def pdf_bytes(pages=2, width=595, height=842):
    """A minimal PDF, built by hand so the suite needs no fixture file.

    Portrait A4 pages, each with one filled rectangle so pdf.js has something to
    draw. The xref offsets are computed, not guessed: pdf.js repairs a broken
    table, but a test that only passes because of a repair is testing the repair.
    """
    kids = " ".join(f"{3 + 2 * i} 0 R" for i in range(pages))
    objects = ["<< /Type /Catalog /Pages 2 0 R >>",
               f"<< /Type /Pages /Kids [{kids}] /Count {pages} >>"]
    for i in range(pages):
        content = f"0 0 1 rg {50 + 10 * i} 50 200 200 re f"
        objects.append(f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] "
                       f"/Contents {4 + 2 * i} 0 R >>")
        objects.append(f"<< /Length {len(content)} >>\nstream\n{content}\nendstream")
    out = b"%PDF-1.4\n"
    offsets = []
    for number, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out += f"{number} 0 obj\n{body}\nendobj\n".encode()
    xref = len(out)
    out += f"xref\n0 {len(objects) + 1}\n0000000000 65535 f \n".encode()
    for offset in offsets:
        out += f"{offset:010d} 00000 n \n".encode()
    out += (f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\n"
            f"startxref\n{xref}\n%%EOF\n").encode()
    return out


procs = []


def upload(name, data, mimetype, port=None):
    """POST one file to /api/assets and return the new asset's id."""
    port = port or 3021
    boundary = "----mcc" + uuid.uuid4().hex
    body = (f"--{boundary}\r\n"
            f"Content-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n"
            f"Content-Type: {mimetype}\r\n\r\n").encode() + data + f"\r\n--{boundary}--\r\n".encode()
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/api/assets", data=body, method="POST",
        headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    with urllib.request.urlopen(req, timeout=10) as res:
        json.load(res)
    rows = http("GET", "/api/assets", port=port)[1]
    return next(row["id"] for row in rows if row["filename"] == name)


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


def a_playlist(port=None):
    """A playlist, assigned to every declared display -- see test_overlay.py."""
    rows = http("GET", "/api/playlists", port=port)[1] or []
    playlist_id = (rows[0]["id"] if rows
                   else http("POST", "/api/playlists", {"name": "Test"}, port=port)[1]["id"])
    for row in http("GET", "/api/displays", port=port)[1] or []:
        if (row.get("schedule") or {}).get("default_playlist_id") != playlist_id:
            http("PUT", f"/api/displays/{row['name']}/schedule",
                 {"default_playlist_id": playlist_id, "windows": []}, port=port)
    return playlist_id


def item(item_id, port=None):
    return next(row for row in http("GET", "/api/playlist", port=port)[1] if row["id"] == item_id)


def api_flow():
    print("\n[100] a new item sits contained on black")
    with Server():
        asset = upload("poster.png", PNG, "image/png")
        playlist = a_playlist()
        status, _ = http("POST", "/api/playlist", {"asset_id": asset, "playlist_id": playlist})
        check("the item is created", status == 201, status)
        row = http("GET", "/api/playlist")[1][-1]
        check("its fit defaults to contain", row["fit_mode"] == "contain", row)
        check("and its background to black", row["fit_background"] == "#000000", row)

        print("\n[101] the fit and the background are stored per item")
        status, _ = http("PUT", f"/api/playlist/{row['id']}",
                         {"fit_mode": "cover", "fit_background": "#00ff00"})
        check("the edit saves", status == 200, status)
        saved = item(row["id"])
        check("cover is kept", saved["fit_mode"] == "cover", saved)
        check("and so is the colour", saved["fit_background"] == "#00ff00", saved)

        status, _ = http("POST", "/api/playlist",
                         {"asset_id": asset, "playlist_id": playlist,
                          "fit_mode": "height", "fit_background": "#123"})
        created = http("GET", "/api/playlist")[1][-1]
        check("a new item can carry both from the start",
              status == 201 and created["fit_mode"] == "height"
              and created["fit_background"] == "#123", created)

        http("POST", "/api/playlist",
             {"asset_id": asset, "playlist_id": playlist, "fit_mode": "scroll"})
        renamed = http("GET", "/api/playlist")[1][-1]
        check("`scroll`, the old name for full width, is stored as width",
              renamed["fit_mode"] == "width", renamed)

        print("\n[101b] a PDF defaults to full width, the way it has always been drawn")
        pdf = upload("handout.pdf", pdf_bytes(), "application/pdf")
        http("POST", "/api/playlist", {"asset_id": pdf, "playlist_id": playlist})
        handout = http("GET", "/api/playlist")[1][-1]
        check("an item that names no fit gets width for a PDF",
              handout["fit_mode"] == "width", handout)
        http("POST", "/api/playlist",
             {"asset_id": pdf, "playlist_id": playlist, "fit_mode": "contain"})
        chosen = http("GET", "/api/playlist")[1][-1]
        check("and whatever it names when it names one", chosen["fit_mode"] == "contain", chosen)
        http("POST", "/api/playlist", {"url": "https://example.test", "playlist_id": playlist})
        page = http("GET", "/api/playlist")[1][-1]
        check("a URL item gets the plain default", page["fit_mode"] == "contain", page)

        print("\n[102] nonsense falls back, a colour that is not one is refused")
        http("PUT", f"/api/playlist/{row['id']}", {"fit_mode": "stretch-it"})
        check("an unknown fit becomes contain rather than an error",
              item(row["id"])["fit_mode"] == "contain", item(row["id"]))

        status, body = http("PUT", f"/api/playlist/{row['id']}",
                            {"fit_background": "red; display:none", "duration": 42})
        check("a background that is not a hex colour is a 400",
              status == 400 and "error" in (body or {}), (status, body))
        after = item(row["id"])
        check("and nothing else in that request was written",
              after["duration"] != 42 and after["fit_background"] == "#00ff00", after)

        status, body = http("POST", "/api/playlist",
                            {"asset_id": asset, "playlist_id": playlist, "fit_background": "blue"})
        check("the same refusal on create", status == 400 and "error" in (body or {}), (status, body))

        other = http("POST", "/api/playlists", {"name": "Zweite"})[1]["id"]
        status, _ = http("PUT", f"/api/playlist/{row['id']}",
                         {"playlist_id": other, "fit_mode": "cover"})
        check("a move carrying a fit edit is refused like any other combined edit",
              status == 400, status)

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


def spawn(cmd):
    p = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    procs.append(p)
    return p


def wait_for(fn, timeout=30, interval=0.3):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            value = fn()
            if value:
                return value
        except Exception:
            pass
        time.sleep(interval)
    return None


MEDIA = """(() => {
  const m = document.getElementById('media');
  if (!m) return JSON.stringify({media: false, href: location.href});
  const root = document.scrollingElement || document.documentElement;
  return JSON.stringify({
    media: true,
    tag: m.tagName.toLowerCase(),
    href: location.href,
    objectFit: getComputedStyle(m).objectFit,
    controls: m.controls === true || m.hasAttribute('controls'),
    loop: m.loop === true,
    autoplay: m.autoplay === true,
    background: getComputedStyle(document.body).backgroundColor,
    scrollable: root.scrollHeight > window.innerHeight + 2,
  });
})()"""


PDF = """(() => {
  const canvases = [...document.querySelectorAll('#pages canvas')];
  const root = document.scrollingElement || document.documentElement;
  return JSON.stringify({
    href: location.href,
    pages: canvases.length,
    vw: window.innerWidth,
    vh: window.innerHeight,
    rects: canvases.map((c) => {
      const r = c.getBoundingClientRect();
      return [Math.round(r.width), Math.round(r.height)];
    }),
    fit: canvases.length ? getComputedStyle(canvases[0]).objectFit : null,
    background: getComputedStyle(document.body).backgroundColor,
    scrollHeight: root.scrollHeight,
  });
})()"""


RECORD = """(async () => {
  // A real, playable WebM of about three seconds, recorded in this page from an
  // animated canvas -- no fixture file and no ffmpeg. MediaRecorder writes no
  // duration into the header, which is exactly the case the page must handle.
  const canvas = document.createElement('canvas');
  canvas.width = 160; canvas.height = 90;
  const ctx = canvas.getContext('2d');
  // Frames requested by hand rather than at a frame rate: a tab that is not in
  // front has its timers and paints throttled, and a stream that waits for
  // paints records nothing (a 524-byte file with a header and no frames).
  const stream = canvas.captureStream(0);
  const track = stream.getVideoTracks()[0];
  let frame = 0;
  const paint = setInterval(() => {
    ctx.fillStyle = `hsl(${(frame++ * 12) % 360}, 80%, 50%)`;
    ctx.fillRect(0, 0, 160, 90);
    track.requestFrame();
  }, 40);
  const recorder = new MediaRecorder(stream, { mimeType: 'video/webm' });
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


async def on_pdf(page, predicate, timeout=40):
    """Poll the display until the PDF viewer has drawn both pages and `predicate` agrees."""
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        try:
            last = json.loads(await page.eval(PDF))
            if "/pdf_viewer.html?" in last["href"] and last["pages"] == 2 and predicate(last):
                return last
        except Exception:
            pass
        await asyncio.sleep(0.3)
    return last


def near(a, b, slack=2):
    return abs(a - b) <= slack


async def on_media(page, predicate, timeout=40):
    """Poll the display until the media viewer shows something `predicate` likes."""
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        try:
            last = json.loads(await page.eval(MEDIA))
            if last.get("media") and predicate(last):
                return last
        except Exception:
            pass  # mid-navigation: the context was just destroyed
        await asyncio.sleep(0.3)
    return last


async def browser_flow():
    print("\n[103] an image item is drawn by our page with the fit it asked for")
    if not os.path.exists(CHROME):
        print("  SKIP  no Chrome at " + CHROME)
        return

    shutil.rmtree(f"{SP}/media-display", ignore_errors=True)
    spawn([CHROME, "--headless=new", f"--remote-debugging-port={CDP}",
           f"--user-data-dir={SP}/media-display", "--no-first-run", "--no-sandbox",
           "--disable-gpu", "--window-size=1280,720",
           "--autoplay-policy=no-user-gesture-required", "about:blank"])
    check("display chrome up", wait_for(lambda: cdp.targets(CDP)) is not None)

    for leftover in ("m.db", "m.db-wal", "m.db-shm"):
        try:
            os.remove(os.path.join(SP, leftover))
        except FileNotFoundError:
            pass
    # Logged to a file rather than discarded: case [109] reads it back, because
    # what the control loop writes to the journal is part of what is under test.
    log = open(f"{SP}/m.log", "w")
    procs.append(subprocess.Popen(
        [BIN, "--port", str(HTTP), "--cast-tls-port", str(TLS),
         "--database-path", f"{SP}/m.db", "--assets-dir", f"{SP}/assets",
         "--cast-cert-path", f"{SP}/cert.pem", "--no-launch-browser",
         "--managed-cert", "off", "--cdp-url", f"http://127.0.0.1:{CDP}"],
        stdout=log, stderr=subprocess.STDOUT, env={**os.environ, "RUST_LOG": "info"}))
    check("controller up", wait_for(
        lambda: http("GET", "/api/cast/info", port=HTTP)[0] == 200) is not None)

    image = upload("fit.png", PNG, "image/png", port=HTTP)
    video = upload("clip.mp4", b"\x00\x00\x00\x18ftypmp42", "video/mp4", port=HTTP)
    playlist = a_playlist(port=HTTP)
    http("POST", "/api/playlist",
         {"asset_id": image, "playlist_id": playlist, "duration": 3,
          "fit_mode": "cover", "fit_background": "#00ff00"}, port=HTTP)
    image_item = http("GET", "/api/playlist", port=HTTP)[1][-1]["id"]

    ws_url, _ = cdp.page_ws(CDP)
    async with cdp.Session(ws_url) as page:
        shown = await on_media(page, lambda m: m["tag"] == "img")
        check("the display is on the media viewer, not /uploads/",
              shown and "/media_viewer.html?" in shown["href"], shown)
        check("with the item's fit", shown and shown["objectFit"] == "cover", shown)
        check("and its background colour",
              shown and shown["background"] == "rgb(0, 255, 0)", shown)
        check("and nothing to scroll", shown and shown["scrollable"] is False, shown)

        print("\n[104] `width` draws it full width and gives the document height")
        http("PUT", f"/api/playlist/{image_item}", {"fit_mode": "width"}, port=HTTP)
        # The edit lands on the item's next navigation; the item is three
        # seconds long and loops, so that is within a few seconds. Waits for the
        # height as well as the URL: until the image has loaded it is zero
        # pixels tall and the page is not scrollable yet.
        tall = await on_media(page, lambda m: "fit=width" in m["href"] and m["scrollable"])
        check("the page is taller than the screen",
              tall and tall["scrollable"] is True, tall)

        print("\n[104b] `height` fills the height and scrolls nothing")
        http("PUT", f"/api/playlist/{image_item}", {"fit_mode": "height"}, port=HTTP)
        high = await on_media(page, lambda m: "fit=height" in m["href"])
        size = json.loads(await page.eval(
            "JSON.stringify((() => { const r = document.getElementById('media')"
            ".getBoundingClientRect(); return [r.width, r.height, innerHeight]; })())"))
        check("the square image is exactly as tall as the screen, and as wide",
              near(size[1], size[2]) and near(size[0], size[2]), size)
        check("and the page does not scroll", high and high["scrollable"] is False, high)

        print("\n[105] a video has no control bar, and loops")
        status, _ = http("POST", "/api/override", {"asset_id": video, "fit_mode": "fill"}, port=HTTP)
        check("the override is up", status == 200, status)
        clip = await on_media(page, lambda m: m["tag"] == "video")
        check("it is a <video> on our page", clip and clip["tag"] == "video", clip)
        check("without controls", clip and clip["controls"] is False, clip)
        # Looping, but not `autoplay`: the video waits for the controller to
        # start it with the item's clock (case [112]).
        check("looping, and held for the controller rather than autoplaying",
              clip and clip["loop"] and not clip["autoplay"], clip)
        check("and the override's own fit", clip and clip["objectFit"] == "fill", clip)

        print("\n[106] a PDF with `contain` is one screen per page")
        pdf = upload("slides.pdf", pdf_bytes(), "application/pdf", port=HTTP)
        http("POST", "/api/override",
             {"asset_id": pdf, "fit_mode": "contain", "fit_background": "#00ff00"}, port=HTTP)
        slides = await on_pdf(page, lambda d: "fit=contain" in d["href"])
        check("both pages are drawn", slides and slides["pages"] == 2, slides)
        check("each one the size of the screen",
              slides and all(near(w, slides["vw"]) and near(h, slides["vh"])
                             for w, h in slides["rects"]), slides)
        check("with the page contained in it", slides and slides["fit"] == "contain", slides)
        check("so a step of one screen height is one page",
              slides and near(slides["scrollHeight"], 2 * slides["vh"]), slides)
        check("on the item's background", slides and slides["background"] == "rgb(0, 255, 0)",
              slides)

        print("\n[107] `height` makes a portrait page exactly as tall as the screen")
        http("POST", "/api/override", {"asset_id": pdf, "fit_mode": "height"}, port=HTTP)
        high = await on_pdf(page, lambda d: "fit=height" in d["href"])
        check("each page is screen height, and as wide as its aspect ratio makes it",
              high and all(near(h, high["vh"]) and near(w, high["vh"] * 595 / 842)
                           for w, h in high["rects"]), high)

        print("\n[108] a PDF that names no fit looks the way PDFs always have")
        http("POST", "/api/override", {"asset_id": pdf}, port=HTTP)
        wide = await on_pdf(page, lambda d: "fit=width" in d["href"])
        check("full width less the viewer's margin, one page after another",
              wide and all(near(w, wide["vw"] - 24) for w, _ in wide["rects"]), wide)
        http("DELETE", "/api/override", port=HTTP)

        print("\n[109] a secret in a playlist URL never reaches the log")
        # The shape that put a real password in a real journal: a dashboard that
        # logs in through its query string. Served by the controller itself so the
        # navigation succeeds offline.
        secret_url = (f"http://127.0.0.1:{HTTP}/empty_playlist.html"
                      "?_username=kiosk&_password=hunter2&token=s3cr3t-t0ken")
        http("POST", "/api/playlist",
             {"url": secret_url, "duration": 600, "playlist_id": playlist}, port=HTTP)
        secret_item = http("GET", "/api/playlist", port=HTTP)[1][-1]["id"]
        http("POST", "/api/control/current", {"item_id": secret_item}, port=HTTP)
        on_it = wait_for(lambda: http("GET", "/api/control/current", port=HTTP)[1]
                         .get("item_id") == secret_item, 40)
        check("the loop is showing the item", on_it is True)
        # And through the override path, which logs its own line.
        http("POST", "/api/override", {"url": secret_url}, port=HTTP)
        time.sleep(4)
        http("DELETE", "/api/override", port=HTTP)
        time.sleep(1)
        written = open(f"{SP}/m.log", encoding="utf-8", errors="replace").read()
        check("the item was logged at all -- or this proves nothing",
              "_username=kiosk" in written, written[-500:])
        check("but not its password", "hunter2" not in written,
              [line[:200] for line in written.splitlines() if "hunter2" in line][:3])
        check("nor its token", "s3cr3t-t0ken" not in written,
              [line[:200] for line in written.splitlines() if "s3cr3t" in line][:3])

    print("\n[111] the upload page measures a video and the length is stored")
    # A browser of its own for the operator's pages, not a second tab in the
    # display's: the control loop brings its page to the front on every item, and
    # Chrome defers loading media in a tab that is not in front -- the measuring
    # timed out there and every upload got the default. An operator uploading is
    # looking at the page, which is what this browser models.
    shutil.rmtree(f"{SP}/media-admin", ignore_errors=True)
    spawn([CHROME, "--headless=new", f"--remote-debugging-port={ADMIN_CDP}",
           f"--user-data-dir={SP}/media-admin", "--no-first-run", "--no-sandbox",
           "--disable-gpu", "about:blank"])
    check("admin chrome up", wait_for(lambda: cdp.targets(ADMIN_CDP)) is not None)
    admin_ws, _ = cdp.page_ws(ADMIN_CDP)
    async with cdp.Session(admin_ws) as admin:
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

    print("\n[113] hovering an asset shows it")
    assets_by_name = {a["filename"]: a for a in http("GET", "/api/assets", port=HTTP)[1]}
    admin_ws, _ = cdp.page_ws(ADMIN_CDP)
    async with cdp.Session(admin_ws) as admin:
        await admin.call("Page.navigate", {"url": f"http://127.0.0.1:{HTTP}/assets.html"})
        await asyncio.sleep(1.5)

        async def hover_row(asset_id):
            return json.loads(await admin.eval(f"""(() => {{
                const row = [...document.querySelectorAll('#assetsBody tr')]
                  .find((tr) => tr.firstChild && tr.firstChild.textContent === '{asset_id}');
                const name = row && row.children[1];
                if (!name) return JSON.stringify({{ found: false }});
                name.dispatchEvent(new MouseEvent('mouseenter'));
                const box = document.querySelector('.asset-preview');
                const visual = box && box.firstElementChild;
                return JSON.stringify({{
                  found: true,
                  shown: !!box && box.style.display === 'block',
                  tag: visual ? visual.tagName.toLowerCase() : null,
                  src: visual ? visual.getAttribute('src') : null,
                }});
            }})()"""))

        image = await hover_row(assets_by_name["fit.png"]["id"])
        check("an image's name shows the image",
              image["shown"] and image["tag"] == "img"
              and image["src"].startswith("/uploads/") and image["src"].endswith("fit.png"), image)
        video = await hover_row(assets_by_name["recorded.webm"]["id"])
        check("a video's name shows its first frame", video["shown"] and video["tag"] == "video",
              video)

        await hover_row(assets_by_name["slides.pdf"]["id"])
        # The test PDF's first page carries a blue rectangle: finding its blue in
        # the rendered picture says pdf.js drew the page, not just an empty box.
        blue = None
        for _ in range(40):
            blue = json.loads(await admin.eval("""(() => {
                const img = document.querySelector('.asset-preview img');
                if (!img || img.dataset.pdf !== 'ready' || !img.naturalWidth)
                  return JSON.stringify({ ready: false, state: img && img.dataset.pdf });
                const c = document.createElement('canvas');
                c.width = img.naturalWidth; c.height = img.naturalHeight;
                const ctx = c.getContext('2d');
                ctx.drawImage(img, 0, 0);
                const data = ctx.getImageData(0, 0, c.width, c.height).data;
                let found = false;
                for (let i = 0; i < data.length; i += 4) {
                  if (data[i + 2] > 200 && data[i] < 80 && data[i + 1] < 80) { found = true; break; }
                }
                return JSON.stringify({ ready: true, found, width: c.width });
            })()"""))
            if blue.get("ready"):
                break
            await asyncio.sleep(0.25)
        check("a PDF's name shows its first page, drawn", blue.get("found") is True, blue)

        print("\n[113b] a playlist card's head and the pickers show it too")
        await admin.call("Page.navigate", {"url": f"http://127.0.0.1:{HTTP}/playlist.html"})
        await asyncio.sleep(2)
        head = json.loads(await admin.eval(f"""(() => {{
            const src = [...document.querySelectorAll('.card .src')]
              .find((s) => s.textContent.startsWith('Asset #{assets_by_name["fit.png"]["id"]} '));
            if (!src) return JSON.stringify({{ found: false }});
            src.dispatchEvent(new MouseEvent('mouseenter'));
            const box = document.querySelector('.asset-preview');
            return JSON.stringify({{ found: true, shown: !!box && box.style.display === 'block',
              tag: box && box.firstElementChild ? box.firstElementChild.tagName.toLowerCase() : null }});
        }})()"""))
        check("an asset card's head shows the asset", head.get("shown") and head.get("tag") == "img",
              head)
        await admin.eval(f"""(() => {{
            const pick = document.getElementById('addAsset');
            // Opened and in view first: a thumbnail renders only once it is on
            // screen, and the add form is a collapsed <details> -- exactly the
            // case the lazy rendering is for.
            document.getElementById('addBlock').open = true;
            pick.scrollIntoView();
            pick.value = '{assets_by_name["slides.pdf"]["id"]}';
            pick.dispatchEvent(new Event('change', {{ bubbles: true }}));
        }})()""")
        thumb = None
        for _ in range(40):
            thumb = json.loads(await admin.eval("""(() => {
                const pick = document.getElementById('addAsset');
                const holder = pick.nextElementSibling;
                const img = holder && holder.classList.contains('asset-thumb') && holder.querySelector('img');
                return JSON.stringify({ holder: !!holder && holder.className,
                                        state: img ? img.dataset.pdf : null });
            })()"""))
            if thumb.get("state") == "ready":
                break
            await asyncio.sleep(0.25)
        check("the add form's picker shows the chosen PDF's first page beside it",
              thumb.get("state") == "ready", thumb)


if __name__ == "__main__":
    try:
        api_flow()
        asyncio.run(browser_flow())
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            try:
                p.wait(timeout=10)
            except Exception:
                p.kill()
        shutil.rmtree(f"{SP}/media-display", ignore_errors=True)
        shutil.rmtree(f"{SP}/media-admin", ignore_errors=True)
        # Case [109]'s copy of the controller log: scratch, and an untracked file
        # beside the suite is noise in every `git status` afterwards.
        if os.path.exists(f"{SP}/m.log"):
            os.remove(f"{SP}/m.log")
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
