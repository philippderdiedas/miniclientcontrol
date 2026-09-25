"""Built-in widgets: clock, banner, QR, countdown -- as a standalone item and as
a layout widget, rendered by the controller's own /widget.html."""
import asyncio, json, os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cdp
from test_cast import check, failures, http


def a_playlist():
    return http("POST", "/api/playlists", {"name": "B"})[1]["id"]


def item(item_id):
    return next(r for r in http("GET", "/api/playlist")[1] if r["id"] == item_id)


def api_flow():
    from test_cast import Server
    print("\n[190] a built-in item is stored, and the refusals")
    with Server():
        pl = a_playlist()
        status, body = http("POST", "/api/playlist",
                            {"playlist_id": pl, "builtin": {"kind": "clock", "show_seconds": True}})
        check("a built-in clock item is created", status == 201, (status, body))
        got = item(body["id"])
        check("it reads back as a built-in, with no url/asset/layout",
              got["builtin"]["kind"] == "clock" and got["url"] is None
              and got["asset_id"] is None and got["layout"] is None, got)

        status, _ = http("POST", "/api/playlist",
                         {"playlist_id": pl, "url": "https://x.test/", "builtin": {"kind": "clock"}})
        check("a url and a built-in at once is a 400", status == 400, status)

        status, _ = http("POST", "/api/playlist",
                         {"playlist_id": pl, "builtin": {"kind": "banner", "text": "Hi"},
                          "advance": {"on": "passes", "count": 2}})
        check("passes on a built-in is a 400", status == 400, status)

        status, _ = http("POST", "/api/playlist", {"playlist_id": pl, "builtin": {"kind": "banner", "text": "x" * 800}})
        stored = http("GET", "/api/playlist")[1][-1]["builtin"]
        check("a long banner text is capped at 500", len(stored["text"]) == 500, len(stored["text"]))

        # A url item cannot be turned into a built-in.
        url_item = http("POST", "/api/playlist", {"playlist_id": pl, "url": "https://u.test/"})[1]["id"]
        status, _ = http("PUT", f"/api/playlist/{url_item}", {"builtin": {"kind": "clock"}})
        check("turning a url item into a built-in is refused", status == 400, status)


def show_flow():
    print("\n[191] a standalone built-in item is shown full-screen")
    from test_webhook import Display, start_chrome, stop_chrome, a_playlist as assigned_playlist, CDP_PORT
    start_chrome()
    try:
        with Display():
            http("POST", "/api/playlist",
                 {"playlist_id": assigned_playlist(), "builtin": {"kind": "clock", "show_seconds": True},
                  "advance": {"on": "time", "seconds": 600}})

            async def look():
                ws, _ = cdp.page_ws(CDP_PORT)
                async with cdp.Session(ws) as page:
                    href, first = "", ""
                    for _ in range(80):
                        try:
                            href = await page.eval("location.href")
                            if "/widget.html?c=" in href:
                                first = await page.eval("(document.getElementById('main') || {}).textContent || ''")
                                if any(c.isdigit() for c in first):
                                    break
                        except Exception:
                            pass
                        await asyncio.sleep(0.5)
                    await asyncio.sleep(1.3)
                    second = await page.eval("(document.getElementById('main') || {}).textContent || ''")
                    return href, first, second
            href, first, second = asyncio.run(look())
            check("the display shows the widget page", "/widget.html?c=" in href, href)
            check("the clock rendered a time", any(c.isdigit() for c in first), first)
            check("and it ticks (the seconds moved)", first != second, (first, second))
    finally:
        stop_chrome()


def widget_flow():
    print("\n[192] a built-in widget sits in a layout at its grid place")
    from test_webhook import Display, start_chrome, stop_chrome, a_playlist as assigned_playlist, CDP_PORT
    start_chrome()
    try:
        with Display():
            layout = {"widgets": [
                {"x": 0, "y": 0, "w": 12, "h": 24, "source": {"kind": "clock"}},
                {"x": 12, "y": 0, "w": 12, "h": 24, "source": {"kind": "banner", "text": "Hallo"}}]}
            http("POST", "/api/playlist", {"layout": layout, "playlist_id": assigned_playlist(),
                                           "advance": {"on": "time", "seconds": 600}})

            async def look():
                ws, _ = cdp.page_ws(CDP_PORT)
                async with cdp.Session(ws) as page:
                    for _ in range(80):
                        try:
                            href = await page.eval("location.href")
                            tree = await page.call("Page.getFrameTree")
                            urls = [c["frame"]["url"] for c in tree["frameTree"].get("childFrames", [])]
                            if "/layout.html?item=" in href and len(urls) == 2 and all("/widget.html?c=" in u for u in urls):
                                cols = json.loads(await page.eval(
                                    "JSON.stringify([...document.querySelectorAll('iframe')]"
                                    ".map(f => getComputedStyle(f).gridColumnStart + '/' + getComputedStyle(f).gridColumnEnd))"))
                                return urls, cols
                        except Exception:
                            pass
                        await asyncio.sleep(0.5)
                    return [], []
            urls, cols = asyncio.run(look())
            check("both widgets are the controller's widget page", len(urls) == 2 and all("/widget.html?c=" in u for u in urls), urls)
            check("and they sit where the grid says", cols == ["1/span 12", "13/span 12"], cols)
    finally:
        stop_chrome()


def content_flow():
    print("\n[193] a QR built-in draws modules, a banner shows its text")
    from test_webhook import Display, start_chrome, stop_chrome, a_playlist as assigned_playlist, CDP_PORT
    start_chrome()
    try:
        with Display():
            pl = assigned_playlist()
            http("POST", "/api/playlist",
                 {"playlist_id": pl, "builtin": {"kind": "qr", "source": "text", "qr_text": "https://example.invalid/x", "label": "Scan"},
                  "advance": {"on": "time", "seconds": 3}})
            http("POST", "/api/playlist",
                 {"playlist_id": pl, "builtin": {"kind": "banner", "text": "GESCHLOSSEN"},
                  "advance": {"on": "time", "seconds": 3}})

            async def look(predicate):
                ws, _ = cdp.page_ws(CDP_PORT)
                async with cdp.Session(ws) as page:
                    for _ in range(80):
                        try:
                            got = await page.eval(predicate)
                            if got:
                                return got
                        except Exception:
                            pass
                        await asyncio.sleep(0.5)
                    return None
            # The two items cycle every 3 s; poll for each in turn.
            rects = asyncio.run(look(
                "(() => { const s=document.querySelector('svg.qr'); return s ? s.querySelectorAll('rect').length : 0; })()"))
            check("the QR drew real modules", (rects or 0) > 10, rects)
            banner = asyncio.run(look(
                "(() => { const m=document.getElementById('main'); return m && m.textContent.includes('GESCHLOSSEN'); })()"))
            check("the banner shows its text", banner is True, banner)
    finally:
        stop_chrome()


if __name__ == "__main__":
    api_flow()
    show_flow()
    widget_flow()
    content_flow()
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
