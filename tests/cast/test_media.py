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


def a_playlist(port=None):
    """A playlist, assigned to every declared display -- see test_overlay.py."""
    rows = http("GET", "/api/playlists", port=port)[1] or []
    playlist_id = (rows[0]["id"] if rows
                   else http("POST", "/api/playlists", {"name": "Test"}, port=port)[1]["id"])
    for row in http("GET", "/api/displays", port=port)[1] or []:
        if row.get("playlist_id") != playlist_id:
            http("PUT", f"/api/displays/{row['name']}", {"playlist_id": playlist_id}, port=port)
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
    spawn([BIN, "--port", str(HTTP), "--cast-tls-port", str(TLS),
           "--database-path", f"{SP}/m.db", "--assets-dir", f"{SP}/assets",
           "--cast-cert-path", f"{SP}/cert.pem", "--no-launch-browser",
           "--managed-cert", "off", "--cdp-url", f"http://127.0.0.1:{CDP}"])
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

        print("\n[105] a video has no control bar, and loops")
        status, _ = http("POST", "/api/override", {"asset_id": video, "fit_mode": "fill"}, port=HTTP)
        check("the override is up", status == 200, status)
        clip = await on_media(page, lambda m: m["tag"] == "video")
        check("it is a <video> on our page", clip and clip["tag"] == "video", clip)
        check("without controls", clip and clip["controls"] is False, clip)
        check("looping and autoplaying", clip and clip["loop"] and clip["autoplay"], clip)
        check("and the override's own fit", clip and clip["objectFit"] == "fill", clip)
        http("DELETE", "/api/override", port=HTTP)


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
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
