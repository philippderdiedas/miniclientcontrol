"""The operator's overlay: stored settings, and the badge actually on the page.

The second half is the part worth having. The overlay is injected into pages we
do not own, so "the setting was saved" says nothing about whether anything is on
screen -- this drives a real Chrome through the real browser_loop and reads the
shadow root back out.
"""
import asyncio, json, os, shutil, subprocess, sys, time, urllib.error, urllib.parse, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cdp
from test_cast import HTTP as CAST_HTTP, Server, check, failures, http

SP = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.join(SP, "..", "..", "target", "debug", "miniclientcontrol")
CHROME = "/usr/bin/google-chrome-stable"
HTTP, TLS = 3041, 3484

procs = []


def spawn(cmd):
    p = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    procs.append(p)
    return p


def chrome(port, profile, *extra):
    shutil.rmtree(f"{SP}/{profile}", ignore_errors=True)
    return spawn([CHROME, "--headless=new", f"--remote-debugging-port={port}",
                  f"--user-data-dir={SP}/{profile}", "--no-first-run", "--no-sandbox",
                  "--disable-gpu", "--window-size=1280,720", *extra, "about:blank"])


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


def put(overlay, port=None):
    return http("PUT", "/api/settings", {"overlay": overlay}, port=port)


def qr(text, port=None):
    url = (f"http://127.0.0.1:{port or CAST_HTTP}/api/qr.svg"
           f"?text={urllib.parse.quote(text)}")
    try:
        with urllib.request.urlopen(url, timeout=5) as res:
            return res.status, res.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


async def settings_flow():
    print("\n[40] the overlay is stored, clamped and sanity-checked")
    with Server():
        status, body = http("GET", "/api/settings")
        check("it starts switched off", status == 200 and body["overlay"]["enabled"] is False,
              body.get("overlay"))

        status, body = put({"enabled": True, "text": "Heute ab 16 Uhr geschlossen",
                            "position": "top-left", "size": 3.5, "show_clock": True})
        check("a text overlay saves", status == 200 and body["overlay"]["text"].startswith("Heute"),
              (status, body))
        check("and keeps the corner it was given", body["overlay"]["position"] == "top-left", body)

        status, body = put({"enabled": True, "text": "x", "size": 900, "opacity": 12,
                            "position": "nowhere"})
        check("an impossible size is pulled into range, not rejected",
              status == 200 and body["overlay"]["size"] <= 20, body)
        check("so is the opacity", body["overlay"]["opacity"] <= 1.0, body)
        check("an unknown corner falls back instead of failing",
              body["overlay"]["position"] == "bottom-right", body)

        status, body = put({"enabled": True, "text": "  ", "show_clock": False,
                            "show_date": False, "qr_text": ""})
        check("an overlay that would draw nothing is refused",
              status == 400 and "nichts" in body.get("error", ""), (status, body))

        status, body = put({"enabled": True, "text": "ok", "image_asset_id": 987654})
        check("a dangling image id is refused rather than shown as a broken image",
              status == 400, (status, body))

        print("\n[41] the display gets a resolved configuration and a QR renderer")
        put({"enabled": True, "text": "Kaffee 1 Euro", "qr_text": "https://example.invalid/menu",
             "qr_label": "Karte"})
        status, payload = http("GET", "/api/overlay")
        check("the runtime payload carries the content", status == 200
              and payload["text"] == "Kaffee 1 Euro", payload)
        check("and a locale, so the clock is not formatted by chance",
              payload.get("locale") == "de-DE", payload)
        check("with no image chosen the path is null", payload["image_path"] is None, payload)
        check("and the payload names the controller's own origin as the base",
              payload.get("base", "").startswith("http://127.0.0.1:"), payload)

        status, svg = qr("https://example.invalid/menu")
        check("the QR endpoint renders an SVG", status == 200 and svg.startswith("<?xml")
              and "svg" in svg, (status, svg[:60]))
        status, _ = qr("")
        check("empty text is refused", status == 400, status)

        print("\n[42] the settings survive a restart")
    with Server(fresh=False):
        status, body = http("GET", "/api/settings")
        check("the overlay came back from the database",
              body["overlay"]["text"] == "Kaffee 1 Euro" and body["overlay"]["enabled"], body)


async def browser_flow():
    print("\n[43] the badge is really on the page")
    if not os.path.exists(CHROME):
        print("  SKIP  no Chrome at " + CHROME)
        return

    chrome(9232, "overlay-display")
    check("display chrome up", wait_for(lambda: cdp.targets(9232)) is not None)

    for leftover in ("o.db", "o.db-wal", "o.db-shm"):
        try:
            os.remove(os.path.join(SP, leftover))
        except FileNotFoundError:
            pass

    spawn([BIN, "--port", str(HTTP), "--cast-tls-port", str(TLS),
           "--database-path", f"{SP}/o.db", "--assets-dir", f"{SP}/assets",
           "--cast-cert-path", f"{SP}/cert.pem", "--no-launch-browser",
           "--cdp-url", "http://127.0.0.1:9232"])
    check("controller up", wait_for(
        lambda: http("GET", "/api/cast/info", port=HTTP)[0] == 200) is not None)

    # No playlist, so the loop parks on the idle page -- which is a page like any
    # other as far as the overlay is concerned.
    on_idle = wait_for(lambda: "empty_playlist" in (cdp.page_ws(9232)[1] or {}).get("url", ""), 40)
    check("browser_loop took over the display browser", on_idle is not None,
          (cdp.page_ws(9232)[1] or {}).get("url"))

    status, body = put({"enabled": True, "text": "Werkstatt geschlossen",
                        "show_clock": True, "position": "top-center"}, port=HTTP)
    check("overlay switched on", status == 200, (status, body))

    ws_url, _ = cdp.page_ws(9232)
    async with cdp.Session(ws_url) as page:
        # The signal is what makes this arrive without waiting for a navigation.
        shown = None
        for _ in range(40):
            shown = await page.eval(
                "(() => globalThis.__ov ? JSON.stringify(globalThis.__ov.state()) : null)()")
            if shown and json.loads(shown).get("attached"):
                break
            await asyncio.sleep(0.5)
        state = json.loads(shown) if shown else {}
        check("the runtime is installed and attached without a navigation",
              state.get("installed") and state.get("attached"), state)
        check("and it put itself in the top layer, above any fullscreen element",
              state.get("inTopLayer") is True, state)

        text = await page.eval(
            "(() => { const h = document.getElementById('__mcc_overlay');"
            " return h && h.shadowRoot ? h.shadowRoot.textContent : null; })()")
        check("the operator's text is on the page", "Werkstatt geschlossen" in (text or ""), text)
        check("and the clock rendered a time", any(c.isdigit() for c in (text or "")), text)

        print("\n[45] and on a playlist item, and on an override")
        # Served by the controller itself: the device is often offline, and a test
        # that needs the internet fails for the wrong reason.
        page_url = f"http://127.0.0.1:{HTTP}/empty_playlist.html"
        http("POST", "/api/playlist", {"url": page_url, "duration": 600}, port=HTTP)
        item = wait_for(lambda: http("GET", "/api/control/current", port=HTTP)[1].get("item_id"), 40)
        check("the loop picked the item up", item is not None)

        on_item = None
        for _ in range(40):
            on_item = await page.eval(
                "(() => { const h = document.getElementById('__mcc_overlay');"
                " return !!(h && h.shadowRoot && h.shadowRoot.textContent"
                " .includes('Werkstatt geschlossen')); })()")
            if on_item:
                break
            await asyncio.sleep(0.5)
        check("the badge survived the navigation to a playlist item", on_item is True)

        http("POST", "/api/override", {"url": page_url}, port=HTTP)
        on_override = None
        for _ in range(40):
            on_override = await page.eval(
                "(() => { const h = document.getElementById('__mcc_overlay');"
                " return !!(h && h.shadowRoot && h.shadowRoot.textContent"
                " .includes('Werkstatt geschlossen')); })()")
            if on_override:
                break
            await asyncio.sleep(0.5)
        check("and it is on an override page too", on_override is True)

        # An override can stand for hours, which is exactly when a notice matters.
        put({"enabled": True, "text": "Neuer Hinweis"}, port=HTTP)
        updated = None
        for _ in range(40):
            updated = await page.eval(
                "(() => { const h = document.getElementById('__mcc_overlay');"
                " return !!(h && h.shadowRoot && h.shadowRoot.textContent"
                " .includes('Neuer Hinweis')); })()")
            if updated:
                break
            await asyncio.sleep(0.5)
        check("an edit reaches a standing override without re-navigating", updated is True)
        http("DELETE", "/api/override", port=HTTP)

        print("\n[46] on a page from a *different* origin the fetches still resolve")
        # The overlay's DOM lives in the displayed page's document. A relative
        # /api/qr.svg would be fetched from that page's host -- which is not us --
        # so this serves the item from a second port to make the mistake visible.
        foreign = subprocess.Popen(
            [sys.executable, "-c",
             "import http.server, socketserver;"
             "h = http.server.BaseHTTPRequestHandler;"
             "\nclass H(h):\n"
             "  def do_GET(s):\n"
             "    s.send_response(200); s.send_header('Content-Type','text/html'); s.end_headers();\n"
             "    s.wfile.write(b'<!doctype html><title>foreign</title><p>foreign page')\n"
             "  def log_message(s, *a): pass\n"
             "socketserver.TCPServer(('127.0.0.1', 3061), H).serve_forever()"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        procs.append(foreign)
        check("the foreign origin is serving", wait_for(
            lambda: urllib.request.urlopen("http://127.0.0.1:3061/", timeout=2).status == 200,
            15) is not None)

        put({"enabled": True, "text": "Mit QR", "qr_text": "https://example.invalid/x"}, port=HTTP)
        http("POST", "/api/override", {"url": "http://127.0.0.1:3061/"}, port=HTTP)

        loaded = None
        for _ in range(60):
            loaded = await page.eval(
                "(() => { const h = document.getElementById('__mcc_overlay');"
                " if (!h || !h.shadowRoot) return null;"
                " const img = h.shadowRoot.querySelector('img.qr');"
                " if (!img) return null;"
                " return JSON.stringify({src: img.src, w: img.naturalWidth,"
                "   host: location.host}); })()")
            if loaded and json.loads(loaded)["w"] > 0:
                break
            await asyncio.sleep(0.5)
        info = json.loads(loaded) if loaded else {}
        check("the page really is the foreign one", info.get("host") == "127.0.0.1:3061", info)
        check("the QR src points at the controller, not at the displayed page",
              str(info.get("src", "")).startswith(f"http://127.0.0.1:{HTTP}/api/qr.svg"), info)
        check("and the image actually loaded", info.get("w", 0) > 0, info)
        http("DELETE", "/api/override", port=HTTP)

        print("\n[47] switching it off takes it away again")
        put({"enabled": False}, port=HTTP)
        gone = None
        for _ in range(40):
            gone = await page.eval(
                "(() => !document.getElementById('__mcc_overlay'))()")
            if gone:
                break
            await asyncio.sleep(0.5)
        check("the badge is removed from the page", gone is True)


async def main():
    try:
        await settings_flow()
        await browser_flow()
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            try:
                p.wait(timeout=10)
            except Exception:
                p.kill()
        shutil.rmtree(f"{SP}/overlay-display", ignore_errors=True)

    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)


asyncio.run(main())
