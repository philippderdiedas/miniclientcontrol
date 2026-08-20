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
LAN = subprocess.run(["python3", "-c",
    "import socket;s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);"
    "s.connect(('10.254.254.254',1));print(s.getsockname()[0])"],
    capture_output=True, text=True).stdout.strip()

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


BOXES = """(() => {
  const hosts = [...document.querySelectorAll('[id^="__mcc_overlay"]')];
  return JSON.stringify({
    count: hosts.length,
    ids: hosts.map((h) => h.id),
    text: hosts.map((h) => (h.shadowRoot ? h.shadowRoot.textContent : '')).join(' | '),
    qr: hosts.flatMap((h) => [...(h.shadowRoot
      ? h.shadowRoot.querySelectorAll('.qr') : [])]
      .map((n) => ({tag: n.tagName.toLowerCase(),
                    rects: n.querySelectorAll ? n.querySelectorAll('rect').length : 0,
                    box: n.getBoundingClientRect().width}))),
    host: location.host,
  });
})()"""


def put(overlay, port=None):
    return http("PUT", "/api/settings", {"overlay": overlay}, port=port)


def qr(text, port=None):
    url = f"http://127.0.0.1:{port or CAST_HTTP}/api/cast/qr.svg"
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
        layers = payload.get("layers") or []
        check("the runtime payload carries one layer with the content", status == 200
              and len(layers) == 1 and layers[0]["text"] == "Kaffee 1 Euro", payload)
        check("and a locale, so the clock is not formatted by chance",
              payload.get("locale") == "de-DE", payload)
        check("with no image chosen there is no image data", layers[0]["image_data"] is None,
              layers)
        check("the QR travels as a module matrix, not a URL",
              isinstance(layers[0].get("qr_modules"), list)
              and len(layers[0]["qr_modules"]) >= 21
              and set("".join(layers[0]["qr_modules"])) <= {"0", "1"},
              str(layers[0].get("qr_modules"))[:120])
        check("and the payload holds no URL pointing back at the controller",
              "base" not in payload
              and f"127.0.0.1:{CAST_HTTP}" not in json.dumps(payload), payload)

        print("\n[41b] a playlist item can add a layer of its own")
        http("POST", "/api/playlist", {"url": "http://127.0.0.1:1/x", "duration": 60})
        items = http("GET", "/api/playlist")[1]
        item_id = items[-1]["id"]
        status, _ = http("PUT", f"/api/playlist/{item_id}",
                         {"overlay": {"enabled": True, "qr_text": "https://example.invalid/item",
                                      "qr_label": "Mehr Info", "position": "top-left"}})
        check("the item's overlay saves", status == 200, status)
        check("and comes back on the playlist",
              (http("GET", "/api/playlist")[1][-1]["overlay_config"] or {}).get("qr_label")
              == "Mehr Info", http("GET", "/api/playlist")[1][-1].get("overlay_config"))

        # Nothing is on screen in this half of the test, so the payload is the
        # global layer only -- the item's layer travels with the item.
        status, body = http("PUT", f"/api/playlist/{item_id}",
                            {"overlay": {"enabled": True, "text": "x",
                                         "image_asset_id": 987654}})
        check("a dangling image on an item is refused too", status == 400, (status, body))

        status, _ = http("PUT", f"/api/playlist/{item_id}",
                         {"overlay": {"enabled": True, "position": "nowhere",
                                      "qr_text": "https://example.invalid/item"}})
        stored = http("GET", "/api/playlist")[1][-1]["overlay_config"] or {}
        check("an unknown corner falls back to the global one (a shared box)",
              status == 200 and stored.get("position") == "", stored)

        status, _ = http("PUT", f"/api/playlist/{item_id}", {"overlay": {"enabled": True}})
        check("an item overlay with no content is stored as none at all",
              http("GET", "/api/playlist")[1][-1]["overlay_config"] is None,
              http("GET", "/api/playlist")[1][-1].get("overlay_config"))

        # The cast QR endpoint stays: our own pages are same-origin over loopback.
        status, svg = qr("https://example.invalid/menu")
        check("the cast QR endpoint still renders an SVG for our own pages",
              status == 200 and "svg" in svg, (status, svg[:60]))

        print("\n[41c] the QR can follow the screen-share address instead of typed text")
        status, body = put({"enabled": True, "qr_source": "cast", "qr_text": "",
                            "qr_label": "Bildschirm teilen", "text": ""})
        check("a cast QR alone counts as content, so it is not refused as empty",
              status == 200 and body["overlay"]["qr_source"] == "cast", (status, body))

        sender_url = http("GET", "/api/cast/info")[1]["sender_url"]
        auto_rows = (http("GET", "/api/overlay")[1]["layers"][0] or {}).get("qr_modules") or []
        check("and it renders a code", len(auto_rows) >= 21, len(auto_rows))
        check("with nothing stored in qr_text", body["overlay"]["qr_text"] == "",
              body["overlay"])

        # The code really encodes the guest address: typing that same address by
        # hand has to produce the identical matrix.
        put({"enabled": True, "qr_source": "text", "qr_text": sender_url, "text": ""})
        typed_rows = (http("GET", "/api/overlay")[1]["layers"][0] or {}).get("qr_modules") or []
        check("and it is the guest address, module for module", auto_rows == typed_rows,
              (sender_url, len(auto_rows), len(typed_rows)))
        print(f"        guest address the display would show: {sender_url}")
        put({"enabled": True, "qr_source": "cast", "qr_text": "", "text": ""})

        status, _ = http("PUT", "/api/settings", {"cast_enabled": False})
        no_qr = (http("GET", "/api/overlay")[1]["layers"][0] or {}).get("qr_modules")
        check("with casting switched off no code is drawn at all", no_qr is None, no_qr)
        http("PUT", "/api/settings", {"cast_enabled": True})

        status, body = put({"enabled": True, "qr_source": "nonsense", "qr_text": "x"})
        check("an unknown source falls back to the typed text",
              body["overlay"]["qr_source"] == "text", body)

        # Back to a plain overlay, so the restart case below checks what it did.
        put({"enabled": True, "text": "Kaffee 1 Euro", "qr_source": "text",
             "qr_text": "https://example.invalid/menu", "qr_label": "Karte"})

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

        boxes = json.loads(await page.eval(BOXES))
        check("the operator's text is on the page",
              "Werkstatt geschlossen" in boxes["text"], boxes)
        check("and the clock rendered a time", any(c.isdigit() for c in boxes["text"]), boxes)
        check("one corner in use means one box", boxes["count"] == 1, boxes)

        print("\n[45] and on a playlist item, and on an override")
        # Served by the controller itself: the device is often offline, and a test
        # that needs the internet fails for the wrong reason.
        page_url = f"http://127.0.0.1:{HTTP}/empty_playlist.html"
        http("POST", "/api/playlist", {"url": page_url, "duration": 600}, port=HTTP)
        item = wait_for(lambda: http("GET", "/api/control/current", port=HTTP)[1].get("item_id"), 40)
        check("the loop picked the item up", item is not None)

        on_item = None
        for _ in range(40):
            on_item = "Werkstatt geschlossen" in json.loads(await page.eval(BOXES))["text"]
            if on_item:
                break
            await asyncio.sleep(0.5)
        check("the badge survived the navigation to a playlist item", on_item is True)

        print("\n[45b] the item's own layer is drawn on top of the global one")
        current = http("GET", "/api/control/current", port=HTTP)[1]["item_id"]
        http("PUT", f"/api/playlist/{current}",
             {"overlay": {"enabled": True, "text": "Mehr Info", "position": ""}}, port=HTTP)
        shared = None
        for _ in range(40):
            shared = json.loads(await page.eval(BOXES))
            if "Mehr Info" in shared["text"]:
                break
            await asyncio.sleep(0.5)
        check("the item's text appears without a navigation",
              "Mehr Info" in shared["text"], shared)
        check("both layers are in the global overlay's box, because the item named no corner",
              shared["count"] == 1 and "Werkstatt geschlossen" in shared["text"], shared)

        http("PUT", f"/api/playlist/{current}",
             {"overlay": {"enabled": True, "text": "Mehr Info", "position": "bottom-left"}},
             port=HTTP)
        split = None
        for _ in range(40):
            split = json.loads(await page.eval(BOXES))
            if split["count"] == 2:
                break
            await asyncio.sleep(0.5)
        check("its own corner gives it its own box", split["count"] == 2, split)
        check("and the two boxes sit where they were asked to",
              sorted(split["ids"]) == ["__mcc_overlay_bottom-left", "__mcc_overlay_top-center"],
              split)

        http("PUT", f"/api/playlist/{current}", {"overlay": {"enabled": False}}, port=HTTP)
        back = None
        for _ in range(40):
            back = json.loads(await page.eval(BOXES))
            if back["count"] == 1:
                break
            await asyncio.sleep(0.5)
        check("switching the item's layer off leaves the global one alone",
              back["count"] == 1 and "Werkstatt geschlossen" in back["text"]
              and "Mehr Info" not in back["text"], back)

        http("POST", "/api/override", {"url": page_url}, port=HTTP)
        on_override = None
        for _ in range(40):
            on_override = "Werkstatt geschlossen" in json.loads(await page.eval(BOXES))["text"]
            if on_override:
                break
            await asyncio.sleep(0.5)
        check("and it is on an override page too", on_override is True)

        # An override can stand for hours, which is exactly when a notice matters.
        put({"enabled": True, "text": "Neuer Hinweis"}, port=HTTP)
        updated = None
        for _ in range(40):
            updated = "Neuer Hinweis" in json.loads(await page.eval(BOXES))["text"]
            if updated:
                break
            await asyncio.sleep(0.5)
        check("an edit reaches a standing override without re-navigating", updated is True)
        http("DELETE", "/api/override", port=HTTP)

        print("\n[46] on a page from a genuinely foreign origin nothing is fetched")
        # Reached by the LAN address on purpose. Chromium's Local Network Access
        # refuses requests to 127.0.0.1 from any origin that is not itself
        # loopback -- measured on Chrome 151 with a fresh profile, where both
        # `fetch` and `<img src="http://127.0.0.1/...">` fail outright. A foreign
        # page served from 127.0.0.1 would pass while a real display failed.
        foreign = subprocess.Popen([sys.executable, f"{SP}/foreign_page.py", "3061"],
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        procs.append(foreign)
        foreign_url = f"http://{LAN}:3061/"
        check("the foreign origin is serving", wait_for(
            lambda: urllib.request.urlopen(foreign_url, timeout=2).status == 200, 15) is not None)

        put({"enabled": True, "text": "Mit QR", "qr_text": "https://example.invalid/x"}, port=HTTP)
        http("POST", "/api/override", {"url": foreign_url}, port=HTTP)

        info = {}
        for _ in range(60):
            info = json.loads(await page.eval(BOXES))
            if info["qr"]:
                break
            await asyncio.sleep(0.5)
        check("the page really is a non-loopback origin",
              info.get("host") == f"{LAN}:3061", info)
        check("the QR is inline SVG, so no request and no img-src to satisfy",
              bool(info["qr"]) and info["qr"][0]["tag"] == "svg", info)
        check("and it drew actual modules at a visible size",
              bool(info["qr"]) and info["qr"][0]["rects"] > 10
              and info["qr"][0]["box"] > 20, info)

        # Deliberately not asserted here: whether a loopback *fetch* from this
        # page is refused. Headless Chrome with --no-sandbox lets it through,
        # while a real profile does not (Chrome 151, fresh profile: `fetch` and
        # `<img>` both fail), so pinning it would make the suite depend on the
        # permission UI rather than on our code. What is asserted is the part we
        # control: the payload names no controller URL, and the QR needs none.
        http("DELETE", "/api/override", port=HTTP)

        print("\n[47] switching it off takes it away again")
        put({"enabled": False}, port=HTTP)
        gone = None
        for _ in range(40):
            gone = json.loads(await page.eval(BOXES))["count"] == 0
            if gone:
                break
            await asyncio.sleep(0.5)
        check("every box is removed from the page", gone is True)


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
