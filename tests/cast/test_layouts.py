"""Layouts: a playlist item that splits the screen into widgets on a 24x24 grid."""
import json, os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_cast import Server, check, failures, http

L_SHAPE = {"widgets": [
    {"x": 0, "y": 0, "w": 18, "h": 20, "source": {"url": "https://a.test/"}},
    {"x": 18, "y": 0, "w": 6, "h": 20, "source": {"url": "https://b.test/"}},
    {"x": 0, "y": 20, "w": 24, "h": 4, "source": {"url": "https://c.test/"}}]}


def a_playlist():
    return http("POST", "/api/playlists", {"name": "L"})[1]["id"]


def item(item_id):
    return next(r for r in http("GET", "/api/playlist")[1] if r["id"] == item_id)


def api_flow():
    print("\n[180] a layout item is stored and read back")
    with Server():
        pl = a_playlist()
        status, body = http("POST", "/api/playlist", {"layout": L_SHAPE, "playlist_id": pl})
        check("created", status == 201, (status, body))
        got = item(body["id"])
        check("read back as it was sent", got["layout"]["widgets"][2]["h"] == 4
              and got["url"] is None and got["asset_id"] is None, got)

        print("\n[181] refusals")
        overlap = {"widgets": [{"x": 0, "y": 0, "w": 12, "h": 12, "source": {"url": "https://a.test/"}},
                               {"x": 6, "y": 6, "w": 12, "h": 12, "source": {"url": "https://b.test/"}}]}
        status, body = http("POST", "/api/playlist", {"layout": overlap, "playlist_id": pl})
        check("overlapping widgets are a 400 naming them", status == 400 and "überlappen" in body.get("error", ""), body)
        status, body = http("POST", "/api/playlist", {"layout": L_SHAPE, "url": "https://x.test/", "playlist_id": pl})
        check("a layout and a url at once is a 400", status == 400, (status, body))
        status, body = http("POST", "/api/playlist", {"layout": L_SHAPE, "playlist_id": pl,
                                                      "advance": {"on": "passes", "count": 2}})
        check("passes on a layout is a 400", status == 400, (status, body))
        missing = {"widgets": [{"x": 0, "y": 0, "w": 24, "h": 24, "source": {"asset_id": 9999}}]}
        status, body = http("POST", "/api/playlist", {"layout": missing, "playlist_id": pl})
        check("an unknown asset is a 400", status == 400, (status, body))
        url_item = http("POST", "/api/playlist", {"url": "https://u.test/", "playlist_id": pl})[1]["id"]
        status, _ = http("PUT", f"/api/playlist/{url_item}", {"layout": L_SHAPE})
        check("turning a url item into a layout is refused", status == 400, status)

        print("\n[182] duplicating an item")
        lay = http("POST", "/api/playlist", {"layout": L_SHAPE, "playlist_id": pl})[1]["id"]
        status, body = http("POST", f"/api/playlist/{lay}/duplicate")
        check("duplicate answers 201 with the new id", status == 201 and body["id"] != lay, (status, body))
        copy, original = item(body["id"]), item(lay)
        check("the copy carries the same layout", copy["layout"] == original["layout"], copy)
        check("and lands at the end", copy["play_order"] == max(r["play_order"] for r in http("GET", f"/api/playlist?playlist_id={pl}")[1]), copy)


def viewport_flow():
    print("\n[183] the controller knows each screen's size")
    from test_webhook import Display, start_chrome, stop_chrome, until_async
    import asyncio
    start_chrome()
    try:
        with Display():
            async def wait():
                return await until_async(lambda: (lambda d: d.get("viewport_width"))(
                    next(x for x in http("GET", "/api/displays")[1] if x["name"] == "default")), timeout=30)
            asyncio.run(wait())
            d = next(x for x in http("GET", "/api/displays")[1] if x["name"] == "default")
            check("width and height of the display browser's window",
                  d.get("viewport_width") and d.get("viewport_height") and d["viewport_width"] > d["viewport_height"], d)
    finally:
        stop_chrome()


import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PAGES = 8621
# How many times each path has been fetched, so a test can tell a frame that is
# reloaded every cycle from one that is loaded once and left alone.
PAGE_HITS = {}


class Pages(BaseHTTPRequestHandler):
    """Plain pages that allow framing, each saying its own name."""
    def log_message(self, *a):
        pass

    def do_GET(self):
        PAGE_HITS[self.path] = PAGE_HITS.get(self.path, 0) + 1
        body = f"<body><h1>page {self.path}</h1><div style='height:{3000 if 'tall' in self.path else 10}px'></div>".encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        # No caching, so a reloaded frame is a fresh fetch the hit counter sees.
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def serve(handler, port, host="127.0.0.1"):
    ThreadingHTTPServer.allow_reuse_address = True
    server = ThreadingHTTPServer((host, port), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def show_flow():
    print("\n[185] a layout item is shown as a grid of framed widgets")
    import asyncio, cdp
    from test_webhook import Display, start_chrome, stop_chrome, a_playlist as assigned_playlist, CDP_PORT
    server = serve(Pages, PAGES)
    start_chrome()
    try:
        with Display():
            layout = {"widgets": [
                {"x": 0, "y": 0, "w": 12, "h": 24, "source": {"url": f"http://127.0.0.1:{PAGES}/a"}},
                {"x": 12, "y": 0, "w": 12, "h": 24, "source": {"url": f"http://127.0.0.1:{PAGES}/b"}}]}
            status, body = http("POST", "/api/playlist", {"layout": layout, "playlist_id": assigned_playlist(),
                                                          "advance": {"on": "time", "seconds": 600}})
            check("the layout item is added", status == 201, (status, body))

            want = [f"http://127.0.0.1:{PAGES}/a", f"http://127.0.0.1:{PAGES}/b"]
            async def look():
                ws_url, _ = cdp.page_ws(CDP_PORT)
                async with cdp.Session(ws_url) as page:
                    href, columns, urls = "", [], []
                    # Poll until the frames have settled on their widget URLs:
                    # frames.rs reloads the layout page once on first attach,
                    # which briefly blanks them.
                    for _ in range(80):
                        try:
                            href = await page.eval("location.href")
                            tree = await page.call("Page.getFrameTree")
                            urls = [c["frame"]["url"] for c in tree["frameTree"].get("childFrames", [])]
                            if "/layout.html?item=" in href and urls == want:
                                columns = json.loads(await page.eval("JSON.stringify([...document.querySelectorAll('iframe')].map(f => getComputedStyle(f).gridColumnStart + '/' + getComputedStyle(f).gridColumnEnd))"))
                                break
                        except Exception:
                            pass
                        await asyncio.sleep(0.5)
                    return href, columns, urls
            href, columns, texts = asyncio.run(look())
            check("the display shows the layout page", "/layout.html?item=" in href, href)
            check("the widgets sit where the grid says", columns == ["1/span 12", "13/span 12"], columns)
            check("each frame loads its widget", texts == want, texts)
    finally:
        stop_chrome()
        server.shutdown()
        server.server_close()


SITE, OTHER = 8631, 8633
API_LOG = []


class Site(BaseHTTPRequestHandler):
    """Like checkmk: forbids framing, logs in through a URL, keeps the session in
    a SameSite=Lax cookie, and loads its data by script."""
    def log_message(self, *a):
        pass

    def reply(self, code, body, extra=()):
        self.send_response(code)
        self.send_header("X-Frame-Options", "DENY")
        self.send_header("Content-Security-Policy", "default-src 'self' 'unsafe-inline'; frame-ancestors 'none'")
        for k, v in extra:
            self.send_header(k, v)
        data = body.encode()
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        cookie = self.headers.get("Cookie", "")
        if self.path == "/short":
            self.reply(302, "", [("Location", "/login")])
        elif self.path == "/login":
            self.reply(302, "", [("Set-Cookie", "sess=ok; Path=/; SameSite=Lax"), ("Location", "/dash")])
        elif self.path == "/dash":
            state = "LOGGED-IN" if "sess=ok" in cookie else "NO-COOKIE"
            self.reply(200, f"<body><h1>{state}</h1><p id=api>api: waiting</p><script>fetch('/api')"
                            ".then(r => r.text()).then(t => document.getElementById('api').textContent = 'api: ' + t)</script>",
                       [("Content-Type", "text/html")])
        elif self.path.startswith("/api"):
            API_LOG.append((self.path.partition("from=")[2] or "widget", "sess=ok" in cookie))
            self.reply(200, "DATA" if "sess=ok" in cookie else "DENIED", [("Content-Type", "text/plain")])
        else:
            self.reply(404, "")


class Other(BaseHTTPRequestHandler):
    """Somebody else's page -- a guest's, say -- asking the dashboard for data."""
    def log_message(self, *a):
        pass

    def do_GET(self):
        body = f"<body><img src='http://localhost:{SITE}/api?from=other'>".encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def lan_address():
    import socket
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.connect(("10.255.255.255", 1))
    address = s.getsockname()[0]
    s.close()
    return address


def unlock_flow():
    print("\n[184] a framed dashboard that forbids framing and logs in with a Lax cookie")
    # Secure cookies over plain HTTP are accepted only for localhost, so the
    # dashboard is on localhost and the layout page on 127.0.0.1 -- a real
    # dashboard is HTTPS and needs no such arrangement.
    import asyncio, cdp
    from test_webhook import Display, start_chrome, stop_chrome, a_playlist as assigned_playlist, CDP_PORT
    lan = lan_address()
    servers = [serve(Site, SITE), serve(Other, OTHER, lan)]
    start_chrome()
    try:
        with Display():
            layout = {"widgets": [{"x": 0, "y": 0, "w": 24, "h": 24, "source": {"url": f"http://localhost:{SITE}/short"}}]}
            http("POST", "/api/playlist", {"layout": layout, "playlist_id": assigned_playlist(),
                                           "advance": {"on": "time", "seconds": 600}})

            async def frame_text():
                for _ in range(60):
                    frames = [t for t in cdp.targets(CDP_PORT) if t["type"] == "iframe" and f":{SITE}/" in t["url"]]
                    if frames:
                        async with cdp.Session(frames[0]["webSocketDebuggerUrl"]) as frame:
                            text = await frame.eval("document.body ? document.body.innerText.replace(/\\n+/g, ' | ') : ''")
                            if "api: waiting" not in text and text:
                                return text
                    await asyncio.sleep(0.5)
                # A blocked frame has no target of its own: say what the page shows.
                ws_url, _ = cdp.page_ws(CDP_PORT)
                async with cdp.Session(ws_url) as page:
                    tree = await page.call("Page.getFrameTree")
                    return "frames: " + str([c["frame"].get("url") for c in tree["frameTree"].get("childFrames", [])])

            text = asyncio.run(frame_text())
            check("the widget is shown, logged in, and its script got data", text == "LOGGED-IN | api: DATA", text)

            async def other_site():
                ws_url, _ = cdp.page_ws(CDP_PORT)
                async with cdp.Session(ws_url) as page:
                    await page.call("Target.createTarget", {"url": f"http://{lan}:{OTHER}/"})
                await asyncio.sleep(3)
            API_LOG.clear()
            asyncio.run(other_site())
            carried = [ok for tag, ok in API_LOG if tag == "other"]
            check("another page in the same browser did not ride the session", carried == [False], carried)
    finally:
        stop_chrome()
        for server in servers:
            server.shutdown()
            server.server_close()


def scroll_flow():
    print("\n[186] each widget scrolls with its own mode")
    import asyncio, cdp
    from test_webhook import Display, start_chrome, stop_chrome, a_playlist as assigned_playlist, CDP_PORT
    # Same-origin widgets (served by the controller itself), so layout.html drives
    # them directly. A tall page set to scroll, and a short one left still.
    server = serve(Pages, PAGES)
    start_chrome()
    try:
        with Display():
            layout = {"widgets": [
                {"x": 0, "y": 0, "w": 12, "h": 24, "scroll_config": {"type": "Continuous", "options": {"speed": 8.0, "top_delay": 0, "return_delay": 0}},
                 "source": {"url": f"http://localhost:{PAGES}/tall"}},
                {"x": 12, "y": 0, "w": 12, "h": 24, "source": {"url": f"http://localhost:{PAGES}/short"}}]}
            http("POST", "/api/playlist", {"layout": layout, "playlist_id": assigned_playlist(),
                                           "advance": {"on": "time", "seconds": 600}})

            async def scroll_of(path):
                # Cross-origin frames have their own CDP target; read scrollY there.
                for _ in range(50):
                    frames = [t for t in cdp.targets(CDP_PORT) if t["type"] == "iframe" and path in t["url"]]
                    if frames:
                        async with cdp.Session(frames[0]["webSocketDebuggerUrl"]) as f:
                            return await f.eval("window.scrollY")
                    await asyncio.sleep(0.5)
                return None
            await_tall = asyncio.run
            import time as _t
            _t.sleep(7)
            tall = asyncio.run(scroll_of("/tall"))
            short = asyncio.run(scroll_of("/short"))
            check("the tall widget scrolled and the short one did not", (tall or 0) > 20 and short == 0, (tall, short))
    finally:
        stop_chrome()
        server.shutdown()
        server.server_close()


def steady_flow():
    print("\n[187] a cycling layout is not torn down when nothing changed")
    # A single-item playlist re-enters its one item every advance period. The
    # layout page, and every widget frame under it, must not be reloaded on each
    # such pass -- that teardown-and-rebuild is the flicker a short advance turns
    # constant. Measured by counting the widget's own fetches: after startup
    # (one navigation plus frames.rs's single reload) they stop.
    import time as _t
    from test_webhook import Display, start_chrome, stop_chrome, a_playlist as assigned_playlist
    server = serve(Pages, PAGES)
    start_chrome()
    try:
        with Display():
            PAGE_HITS.clear()
            layout = {"widgets": [{"x": 0, "y": 0, "w": 24, "h": 24,
                                   "source": {"url": f"http://127.0.0.1:{PAGES}/steady"}}]}
            http("POST", "/api/playlist", {"layout": layout, "playlist_id": assigned_playlist(),
                                           "advance": {"on": "time", "seconds": 2}})
            # Let the display come up and the widget settle (startup loads it at
            # most twice), then let several 2 s advance cycles pass.
            _t.sleep(7)
            settled = PAGE_HITS.get("/steady", 0)
            _t.sleep(10)  # about five more advance cycles
            after = PAGE_HITS.get("/steady", 0)
            # Reloading every cycle would add ~5 here; the fix adds none.
            check("the widget frame is loaded once, not on every advance cycle",
                  settled >= 1 and after - settled <= 1, (settled, after))
    finally:
        stop_chrome()
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    api_flow()
    viewport_flow()
    show_flow()
    unlock_flow()
    scroll_flow()
    steady_flow()
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
