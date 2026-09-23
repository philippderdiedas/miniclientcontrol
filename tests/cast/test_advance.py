"""When an item moves on: after a time, or after its content ran N times.

The API half is plain HTTP. The browser half drives a real Chrome through the
real browser_loop, because a stored count proves nothing about the screen.
"""
import asyncio, base64, json, os, shutil, subprocess, sys, time, urllib.request, uuid
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cdp
from test_cast import Server, check, failures, http

SP = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.join(SP, "..", "..", "target", "debug", "miniclientcontrol")
CHROME = "/usr/bin/google-chrome-stable"
HTTP, TLS, CDP = 3061, 3504, 9262
# The operator pages' own browser, for recording a video (case [145]).
ADMIN_CDP = 9264
PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
procs = []


def upload(name, data, mimetype, port=None):
    port = port or 3021
    boundary = "----mcc" + uuid.uuid4().hex
    body = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n"
            f"Content-Type: {mimetype}\r\n\r\n").encode() + data + f"\r\n--{boundary}--\r\n".encode()
    req = urllib.request.Request(f"http://127.0.0.1:{port}/api/assets", data=body, method="POST",
                                 headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    with urllib.request.urlopen(req, timeout=10) as res:
        json.load(res)
    return next(r["id"] for r in http("GET", "/api/assets", port=port)[1] if r["filename"] == name)


def a_playlist(port=None):
    rows = http("GET", "/api/playlists", port=port)[1] or []
    playlist_id = (rows[0]["id"] if rows
                   else http("POST", "/api/playlists", {"name": "Test"}, port=port)[1]["id"])
    for row in http("GET", "/api/displays", port=port)[1] or []:
        if (row.get("schedule") or {}).get("default_playlist_id") != playlist_id:
            http("PUT", f"/api/displays/{row['name']}/schedule",
                 {"default_playlist_id": playlist_id, "windows": []}, port=port)
    return playlist_id


def item(item_id, port=None):
    return next(r for r in http("GET", "/api/playlist", port=port)[1] if r["id"] == item_id)


# `speed` is pixels per frame (about 60 a second): 5 is ~300 px/s.
SCROLL = {"type": "Continuous", "options": {"speed": 5.0, "top_delay": 0, "return_delay": 500}}


def api_flow():
    print("\n[140] an item says when it moves on")
    with Server():
        pl = a_playlist()
        status, body = http("POST", "/api/playlist", {"url": "https://a.test/", "playlist_id": pl})
        check("an item naming nothing is created", status == 201, (status, body))
        check("and moves on after ten seconds",
              item(body["id"])["advance"] == {"on": "time", "seconds": 10}, item(body["id"]))
        check("the item no longer has a duration", "duration" not in item(body["id"]), item(body["id"]))

        status, body = http("POST", "/api/playlist", {"url": "https://b.test/", "playlist_id": pl,
                                                      "advance": {"on": "time", "seconds": 0}})
        check("seconds are clamped, not refused", item(body["id"])["advance"]["seconds"] == 1, body)

        status, body = http("POST", "/api/playlist", {"url": "https://c.test/", "playlist_id": pl,
                                                      "duration": 30})
        check("a request still sending duration is refused", status == 422, (status, body))

        print("\n[141] passes need something that ends")
        status, body = http("POST", "/api/playlist", {"url": "https://d.test/", "playlist_id": pl,
                                                      "advance": {"on": "passes", "count": 2}})
        check("a page that does not scroll cannot count passes", status == 400 and "error" in body,
              (status, body))
        status, body = http("POST", "/api/playlist", {"url": "https://e.test/", "playlist_id": pl,
                                                      "scroll_config": SCROLL,
                                                      "advance": {"on": "passes", "count": 2}})
        check("a scrolling page can", status == 201, (status, body))
        scrolling = body["id"]
        status, body = http("PUT", f"/api/playlist/{scrolling}", {"scroll_config": {"type": "None", "options": None}})
        check("switching its scroll off under the passes is refused", status == 400, (status, body))
        check("and nothing was written",
              item(scrolling)["scroll_config"]["type"] == "Continuous", item(scrolling))
        status, _ = http("PUT", f"/api/playlist/{scrolling}", {"advance": {"on": "time", "seconds": 5},
                                                              "scroll_config": {"type": "None", "options": None}})
        check("together with a switch to time it goes through", status == 200, status)
        video = upload("clip.mp4", b"\x00\x00\x00\x18ftypmp42", "video/mp4")
        status, body = http("POST", "/api/playlist", {"asset_id": video, "playlist_id": pl,
                                                      "advance": {"on": "passes", "count": 3}})
        check("a video can count passes without scrolling", status == 201, (status, body))
        status, _ = http("PUT", f"/api/playlist/{body['id']}", {"duration": 4})
        check("duration on an update is refused too", status == 422, status)


def spawn(cmd, **kwargs):
    p = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, **kwargs)
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


def current(port=HTTP):
    return http("GET", "/api/control/current", port=port)[1].get("item_id")


def serve_page(name, html, port=HTTP):
    """An HTML page as an asset, played through its /uploads/ URL."""
    upload(name, html.encode(), "text/html", port=port)
    row = next(r for r in http("GET", "/api/assets", port=port)[1] if r["filename"] == name)
    return f"http://127.0.0.1:{port}/uploads/{row['local_path']}"


TALL = "<!doctype html><body style='margin:0'><div style='height:3000px;background:linear-gradient(#f00,#00f)'></div>"
FITS = "<!doctype html><body><p>fits</p>"


async def time_on(item_id, port=HTTP, timeout=90):
    """Seconds item `item_id` stays current once it becomes current."""
    became = wait_for(lambda: current(port) == item_id, timeout)
    if not became:
        return None
    start = time.time()
    left = wait_for(lambda: current(port) != item_id, timeout)
    return time.time() - start if left else None


async def browser_flow():
    print("\n[142] a scrolling page moves on after its passes")
    if not os.path.exists(CHROME):
        print("  SKIP  no Chrome at " + CHROME)
        return
    shutil.rmtree(f"{SP}/advance-display", ignore_errors=True)
    spawn([CHROME, "--headless=new", f"--remote-debugging-port={CDP}",
           f"--user-data-dir={SP}/advance-display", "--no-first-run", "--no-sandbox",
           "--disable-gpu", "--window-size=1280,720",
           "--autoplay-policy=no-user-gesture-required", "about:blank"])
    check("display chrome up", wait_for(lambda: cdp.targets(CDP)) is not None)
    for leftover in ("a.db", "a.db-wal", "a.db-shm"):
        try:
            os.remove(os.path.join(SP, leftover))
        except FileNotFoundError:
            pass
    spawn([BIN, "--port", str(HTTP), "--cast-tls-port", str(TLS),
           "--database-path", f"{SP}/a.db", "--assets-dir", f"{SP}/assets",
           "--cast-cert-path", f"{SP}/cert.pem", "--no-launch-browser",
           "--managed-cert", "off", "--cdp-url", f"http://127.0.0.1:{CDP}",
           "--advance-stall-timeout", "8"])
    check("controller up", wait_for(lambda: http("GET", "/api/cast/info", port=HTTP)[0] == 200) is not None)

    pl = a_playlist(port=HTTP)
    filler = http("POST", "/api/playlist", {"url": serve_page("filler.html", FITS), "playlist_id": pl,
                                            "advance": {"on": "time", "seconds": 3}}, port=HTTP)[1]["id"]
    # 2280 px to scroll (3000 minus the 720 px window) at ~300 px/s: ~7.6 s a
    # pass plus half a second at the bottom.
    tall = http("POST", "/api/playlist", {"url": serve_page("tall.html", TALL), "playlist_id": pl,
                                          "scroll_config": SCROLL,
                                          "advance": {"on": "passes", "count": 2}}, port=HTTP)[1]["id"]
    shown = await time_on(tall)
    check("it moves on by itself", shown is not None, shown)
    check("after the second pass, not the first (each is ~8 s)", shown and 13 < shown < 40, shown)

    print("\n[143] it stays at the bottom after the last pass")
    ws_url, _ = cdp.page_ws(CDP)
    async with cdp.Session(ws_url) as page:
        wait_for(lambda: current() == tall, 60)
        tops = []
        deadline = time.time() + 40
        while time.time() < deadline and current() == tall:
            try:
                tops.append(await page.eval("Math.round(window.scrollY)"))
            except Exception:
                pass
            await asyncio.sleep(0.1)
        # After the final bottom the page must not jump back to 0 before leaving.
        last_bottom = max((i for i, t in enumerate(tops) if t > 2000), default=None)
        check("the last samples before leaving are at the bottom, not back at the top",
              last_bottom is not None and all(t > 2000 for t in tops[last_bottom:]), tops[-15:])

    print("\n[144] a page that fits the screen does not flash through its passes")
    http("PUT", f"/api/playlist/{tall}", {"enabled": False}, port=HTTP)
    fits = http("POST", "/api/playlist", {"url": serve_page("fits.html", FITS), "playlist_id": pl,
                                          "scroll_config": {"type": "Continuous", "options":
                                                            {"speed": 400.0, "top_delay": 0, "return_delay": 0}},
                                          "advance": {"on": "passes", "count": 2}}, port=HTTP)[1]["id"]
    shown = await time_on(fits)
    check("two passes take at least two minimum passes (2 x 3 s)", shown and shown >= 5.5, shown)
    http("PUT", f"/api/playlist/{fits}", {"enabled": False}, port=HTTP)

    print("\n[145] a video moves on after its plays")
    # Recorded in a browser of its own, as test_media.py case [111] does: the
    # display's tab is brought to front by the loop, and Chrome defers media in
    # a background tab.
    from test_media import RECORD
    shutil.rmtree(f"{SP}/advance-admin", ignore_errors=True)
    spawn([CHROME, "--headless=new", f"--remote-debugging-port={ADMIN_CDP}",
           f"--user-data-dir={SP}/advance-admin", "--no-first-run", "--no-sandbox",
           "--disable-gpu", "about:blank"])
    check("admin chrome up", wait_for(lambda: cdp.targets(ADMIN_CDP)) is not None)
    admin_ws, _ = cdp.page_ws(ADMIN_CDP)
    async with cdp.Session(admin_ws) as admin:
        await admin.call("Page.navigate", {"url": f"http://127.0.0.1:{HTTP}/assets.html"})
        await asyncio.sleep(1.5)
        await admin.eval(RECORD, timeout=30)
    clip = wait_for(lambda: next((r for r in http("GET", "/api/assets", port=HTTP)[1]
                                  if r["filename"] == "recorded.webm"), None), 30)
    check("a short video was recorded", clip is not None, clip)
    video = http("POST", "/api/playlist", {"asset_id": clip["id"], "playlist_id": pl,
                                           "advance": {"on": "passes", "count": 2}}, port=HTTP)[1]["id"]
    ws_url, _ = cdp.page_ws(CDP)
    async with cdp.Session(ws_url) as page:
        wait_for(lambda: current() == video, 90)
        looping = None
        for _ in range(40):
            try:
                looping = await page.eval(
                    "(() => { const v = document.getElementById('media'); return v && globalThis.__media"
                    " && globalThis.__media.state().startedBy ? v.loop : null; })()")
                if looping is not None:
                    break
            except Exception:
                pass
            await asyncio.sleep(0.25)
        check("counting plays turns loop off", looping is False, looping)
    # Measured on its next showing, from the moment it becomes current.
    shown_video = await time_on(video)
    check("two ~3 s plays take ~6 s plus the readiness wait, short of the 8 s stall",
          shown_video and 5 < shown_video < 16, shown_video)
    http("PUT", f"/api/playlist/{video}", {"enabled": False}, port=HTTP)

    print("\n[146] a paged PDF moves on after its last page")
    from test_media import pdf_bytes
    pdf = upload("slides.pdf", pdf_bytes(pages=3), "application/pdf", port=HTTP)
    paged = http("POST", "/api/playlist", {"asset_id": pdf, "playlist_id": pl, "fit_mode": "contain",
                                           "scroll_config": {"type": "Step", "options":
                                                             {"step_time": None, "step_px": None, "step_delay": 1500}},
                                           "advance": {"on": "passes", "count": 1}}, port=HTTP)[1]["id"]
    shown = await time_on(paged)
    check("three pages at 1.5 s each, then on", shown and 3 < shown < 20, shown)
    http("PUT", f"/api/playlist/{paged}", {"enabled": False}, port=HTTP)

    print("\n[147] a page whose counter is gone moves on after the stall timeout")
    stuck = http("POST", "/api/playlist", {"url": serve_page("stuck.html", TALL), "playlist_id": pl,
                                           "scroll_config": SCROLL,
                                           "advance": {"on": "passes", "count": 50}}, port=HTTP)[1]["id"]
    ws_url, _ = cdp.page_ws(CDP)
    async with cdp.Session(ws_url) as page:
        wait_for(lambda: current() == stuck, 60)
        started = time.time()
        # Remove it repeatedly: the controller re-evaluates the runtime after
        # navigation, but not while the item stands.
        while current() == stuck and time.time() - started < 40:
            try:
                await page.eval("delete globalThis.__advance; if (globalThis.__as) globalThis.__as.disable(); true")
            except Exception:
                pass
            await asyncio.sleep(0.5)
        waited = time.time() - started
    check("it moved on after about the stall timeout (8 s here), not the 50 passes",
          7 <= waited < 20, waited)

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
        shutil.rmtree(f"{SP}/advance-display", ignore_errors=True)
        shutil.rmtree(f"{SP}/advance-admin", ignore_errors=True)
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
