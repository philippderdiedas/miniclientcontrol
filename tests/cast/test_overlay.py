"""The operator's overlay: stored settings, and the badge actually on the page.

The second half is the part worth having. The overlay is injected into pages we
do not own, so "the setting was saved" says nothing about whether anything is on
screen -- this drives a real Chrome through the real browser_loop and reads the
shadow root back out.
"""
import asyncio, json, os, shutil, subprocess, sys, time, urllib.error, urllib.parse, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cdp
import wsclient
from test_cast import HTTP as CAST_HTTP, Server, check, failures, http, ws

SP = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.join(SP, "..", "..", "target", "debug", "miniclientcontrol")
CHROME = "/usr/bin/google-chrome-stable"
HTTP, TLS = 3041, 3484
LAN = subprocess.run(["python3", "-c",
    "import socket;s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);"
    "s.connect(('10.254.254.254',1));print(s.getsockname()[0])"],
    capture_output=True, text=True).stdout.strip()

procs = []



def a_playlist(port=None):
    """The playlist items get added to, created on first use.

    An item belongs to a playlist since playlists became objects, and a fresh
    database has none -- the one-time backfill only fires for a database that
    already had items. Without this the POST fails deserialisation and no item
    is created, which reads as "the loop never picked it up".
    """
    rows = http("GET", "/api/playlists", port=port)[1] or []
    if rows:
        return rows[0]["id"]
    return http("POST", "/api/playlists", {"name": "Test"}, port=port)[1]["id"]

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


STYLED = """(() => {
  const hosts = [...document.querySelectorAll('[id^="__mcc_overlay"]')];
  const box = hosts.map((h) => h.shadowRoot && h.shadowRoot.querySelector('.box'))
                   .find(Boolean);
  if (!box) return JSON.stringify({box: false});
  const cs = getComputedStyle(box);
  return JSON.stringify({
    box: true,
    text: box.textContent,
    position: cs.position,
    background: cs.backgroundColor,
    fontSize: parseFloat(cs.fontSize),
    host: location.host,
  });
})()"""


COLOURS = """(() => {
  const hosts = [...document.querySelectorAll('[id^="__mcc_overlay"]')];
  return JSON.stringify({
    count: hosts.length,
    boxes: hosts.map((h) => {
      const box = h.shadowRoot && h.shadowRoot.querySelector('.box');
      return {
        id: h.id,
        color: box ? getComputedStyle(box).color : null,
        text: box ? box.textContent : '',
      };
    }),
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

        status, body = put({"enabled": True, "text": "x", "size": 900,
                            "background_alpha": 12, "position": "nowhere"})
        check("an impossible size is pulled into range, not rejected",
              status == 200 and body["overlay"]["size"] <= 20, body)
        check("so is an alpha out of range", body["overlay"]["background_alpha"] <= 1.0, body)
        check("an unknown corner falls back instead of failing",
              body["overlay"]["position"] == "bottom-right", body)

        print("\n[40b] the box style is structured, not free-text CSS")
        status, body = put({"enabled": True, "text": "x", "background_color": "#123456",
                            "background_alpha": 0.4, "color": "#00ff00",
                            "color_alpha": 0.5})
        style = body["overlay"]
        check("colour and alpha are stored apart",
              status == 200 and style["background_color"] == "#123456"
              and abs(style["background_alpha"] - 0.4) < 0.01, style)
        check("and the text colour has its own alpha",
              abs(style["color_alpha"] - 0.5) < 0.01, style)

        status, body = put({"enabled": True, "text": "x", "background_color": "not a colour",
                            "color": "#12345"})
        check("a colour that is not one falls back instead of rendering nothing",
              body["overlay"]["background_color"] == "#000000"
              and body["overlay"]["color"] == "#ffffff", body["overlay"])

        status, body = put({"enabled": True, "text": "x", "color_alpha": 0.0})
        check("text cannot be made invisible -- that is indistinguishable from broken",
              body["overlay"]["color_alpha"] >= 0.1, body["overlay"])

        status, body = put({"enabled": True, "text": "x",
                            "background_css": "linear-gradient(#000, #333)"})
        check("the escape hatch keeps a real CSS value",
              body["overlay"]["background_css"].startswith("linear-gradient"),
              body["overlay"])

        status, body = put({"enabled": True, "text": "x",
                            "background_css": "red; } .box { display: none"})
        check("but a value that would end the declaration is dropped",
              body["overlay"]["background_css"] == "", body["overlay"])

        print("\n[40c] a stored background from before the split is migrated")
        # Written the way the old version wrote it, straight into the settings row.
        legacy = json.dumps({"enabled": True, "text": "alt",
                             "background": "rgba(17,34,51,0.5)", "opacity": 0.5,
                             "color": "#abcdef"})
        import sqlite3
        con = sqlite3.connect(f"{SP}/t.db")
        con.execute("INSERT OR REPLACE INTO settings (key, value) VALUES ('overlay_config', ?)",
                    (legacy,))
        con.commit()
        con.close()

    with Server(fresh=False):
        migrated = http("GET", "/api/settings")[1]["overlay"]
        check("the colour survived as a picker value", migrated["background_color"] == "#112233",
              migrated)
        # 0.5 alpha dimmed further by the old whole-box opacity of 0.5.
        check("and the old whole-box opacity folded into its alpha",
              abs(migrated["background_alpha"] - 0.25) < 0.01, migrated)
        check("the legacy fields are gone from the response",
              "background" not in migrated and "opacity" not in migrated, migrated)

    with Server(fresh=True):
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
        http("POST", "/api/playlist",
             {"url": "http://127.0.0.1:1/x", "duration": 60, "playlist_id": a_playlist()})
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

        print("\n[41d] during a cast: the operator decides, the guest does not")
        put({"enabled": True, "text": "Werkstatt schließt 16 Uhr",
             "qr_source": "cast", "qr_label": "Teilen", "hide_during_cast": False})

        dr, dw = await ws("display")
        await wsclient.recv_json(dr)
        sr, sw = await ws("sender")
        await wsclient.recv_json(sr)
        await asyncio.sleep(0.6)
        check("a cast is running", http("GET", "/api/cast/state")[1]["active"] is True)

        layers = http("GET", "/api/overlay")[1]["layers"]
        check("the venue's message stays on screen",
              len(layers) == 1 and layers[0]["text"].startswith("Werkstatt"), layers)
        check("but the cast QR is gone, because the slot is taken",
              layers[0]["qr_modules"] is None, layers[0].get("qr_modules"))

        put({"enabled": True, "text": "Werkstatt schließt 16 Uhr", "hide_during_cast": True})
        check("with the operator's switch set, nothing is drawn at all",
              http("GET", "/api/overlay")[1]["layers"] == [],
              http("GET", "/api/overlay")[1])

        sw.close()
        dw.close()
        await asyncio.sleep(6)
        check("and it comes back once the cast is over",
              len(http("GET", "/api/overlay")[1]["layers"]) == 1,
              http("GET", "/api/overlay")[1])

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

        print("\n[44] the colour and alpha reach the rendered box")
        put({"enabled": True, "text": "Werkstatt geschlossen", "show_clock": True,
             "position": "top-center", "background_color": "#112233",
             "background_alpha": 0.4, "color": "#00ff00", "color_alpha": 0.6}, port=HTTP)
        painted = None
        for _ in range(40):
            painted = json.loads(await page.eval(
                """(() => {
                  const h = document.querySelector('[id^="__mcc_overlay"]');
                  const box = h && h.shadowRoot && h.shadowRoot.querySelector('.box');
                  if (!box) return 'null';
                  const st = getComputedStyle(box);
                  return JSON.stringify({bg: st.backgroundColor, fg: st.color,
                                         pad: st.paddingTop});
                })()"""))
            if painted and painted.get("bg", "").startswith("rgba(17"):
                break
            await asyncio.sleep(0.5)
        check("the background is the picked colour at the picked alpha",
              painted.get("bg") == "rgba(17, 34, 51, 0.4)", painted)
        check("and the text colour carries its own alpha",
              painted.get("fg") == "rgba(0, 255, 0, 0.6)", painted)

        put({"enabled": True, "text": "Werkstatt geschlossen", "show_clock": True,
             "position": "top-center", "plain": True}, port=HTTP)
        plain = None
        for _ in range(40):
            plain = json.loads(await page.eval(
                """(() => {
                  const h = document.querySelector('[id^="__mcc_overlay"]');
                  const box = h && h.shadowRoot && h.shadowRoot.querySelector('.box');
                  if (!box) return 'null';
                  const st = getComputedStyle(box);
                  return JSON.stringify({bg: st.backgroundColor, pad: st.paddingTop,
                                         cls: box.className});
                })()"""))
            if plain and "plain" in plain.get("cls", ""):
                break
            await asyncio.sleep(0.5)
        check("\"no box\" drops the background and the padding with it",
              plain.get("bg") == "rgba(0, 0, 0, 0)" and plain.get("pad") == "0px", plain)

        print("\n[44b] the content lines up with the corner it sits in")
        ALIGN = """(() => {
          const h = document.querySelector('[id^="__mcc_overlay"]');
          const box = h && h.shadowRoot && h.shadowRoot.querySelector('.box');
          const qr = h && h.shadowRoot && h.shadowRoot.querySelector('.qrwrap');
          if (!box) return 'null';
          return JSON.stringify({align: getComputedStyle(box).textAlign,
                                 qr: qr ? getComputedStyle(qr).justifyContent : null});
        })()"""

        for position, want, want_flex in [("bottom-center", "center", "center"),
                                          ("bottom-right", "right", "flex-end"),
                                          ("top-left", "left", "flex-start")]:
            put({"enabled": True, "text": "Werkstatt geschlossen", "show_clock": True,
                 "qr_source": "text", "qr_text": "https://example.invalid/x",
                 "position": position}, port=HTTP)
            got = None
            for _ in range(40):
                got = json.loads(await page.eval(ALIGN))
                # Both, not just the alignment: the previous position may already
                # have matched, and then this would read the box from before the
                # edit and pass without proving anything.
                if got.get("align") == want and got.get("qr") is not None:
                    break
                await asyncio.sleep(0.5)
            check(f"{position} aligns its text {want}", got.get("align") == want, got)
            check(f"and its QR row follows ({want_flex})", got.get("qr") == want_flex, got)

        # Back to a box, so the checks below read what they expect.
        put({"enabled": True, "text": "Werkstatt geschlossen", "show_clock": True,
             "position": "top-center"}, port=HTTP)
        await asyncio.sleep(2)

        print("\n[45] and on a playlist item, and on an override")
        # Served by the controller itself: the device is often offline, and a test
        # that needs the internet fails for the wrong reason.
        page_url = f"http://127.0.0.1:{HTTP}/empty_playlist.html"
        http("POST", "/api/playlist",
             {"url": page_url, "duration": 600, "playlist_id": a_playlist(port=HTTP)},
             port=HTTP)
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

        print("\n[45c] a bright item can recolour the box it sits in")
        # One bright page in an otherwise dark playlist: the global overlay's
        # white clock is the thing that becomes unreadable, so the override has
        # to reach the *global* layer and not only the item's own.
        put({"enabled": True, "text": "Haus", "show_clock": True,
             "position": "top-center", "color": "#ffffff"}, port=HTTP)
        current = http("GET", "/api/control/current", port=HTTP)[1]["item_id"]

        http("PUT", f"/api/playlist/{current}",
             {"overlay": {"enabled": True, "text": "Item", "position": "",
                          "color": "#101010"}}, port=HTTP)
        shared = {}
        for _ in range(40):
            shared = json.loads(await page.eval(COLOURS))
            if shared["count"] == 1 and shared["boxes"][0]["color"] == "rgb(16, 16, 16)":
                break
            await asyncio.sleep(0.5)
        check("the item's colour reaches the shared box",
              shared["count"] == 1 and shared["boxes"][0]["color"] == "rgb(16, 16, 16)", shared)
        check("and it took the global clock with it, which is the whole point",
              "Haus" in shared["boxes"][0]["text"] and "Item" in shared["boxes"][0]["text"],
              shared)

        http("PUT", f"/api/playlist/{current}",
             {"overlay": {"enabled": True, "text": "Item", "position": "bottom-left",
                          "color": "#101010"}}, port=HTTP)
        split = {}
        for _ in range(40):
            split = json.loads(await page.eval(COLOURS))
            if split["count"] == 2:
                break
            await asyncio.sleep(0.5)
        by_id = {b["id"]: b for b in split.get("boxes", [])}
        check("an item in its own corner recolours only its own box",
              by_id.get("__mcc_overlay_bottom-left", {}).get("color") == "rgb(16, 16, 16)",
              split)
        check("and the global box keeps the colour it was given",
              by_id.get("__mcc_overlay_top-center", {}).get("color") == "rgb(255, 255, 255)",
              split)

        # A bright page often wants no badge of its own -- only a readable clock.
        http("PUT", f"/api/playlist/{current}",
             {"overlay": {"enabled": True, "text": "", "qr_text": "",
                          "position": "bottom-left", "color": "#204060"}}, port=HTTP)
        only = {}
        for _ in range(40):
            only = json.loads(await page.eval(COLOURS))
            if only["count"] == 1 and only["boxes"][0]["color"] == "rgb(32, 64, 96)":
                break
            await asyncio.sleep(0.5)
        check("a colour with no content draws no box of its own",
              only["count"] == 1, only)
        check("and recolours the global box, whatever corner it named",
              only["boxes"][0]["color"] == "rgb(32, 64, 96)"
              and only["boxes"][0]["id"] == "__mcc_overlay_top-center", only)
        stored = [i for i in http("GET", "/api/playlist", port=HTTP)[1]
                  if i["id"] == current]
        check("a colour-only overlay is stored rather than dropped as empty",
              bool(stored) and (stored[0].get("overlay_config") or {}).get("color") == "#204060",
              stored[:1])

        http("PUT", f"/api/playlist/{current}",
             {"overlay": {"enabled": True, "text": "Item", "position": "",
                          "color": "keine farbe"}}, port=HTTP)
        bad = {}
        for _ in range(40):
            bad = json.loads(await page.eval(COLOURS))
            if bad["count"] == 1 and "Item" in bad["boxes"][0]["text"]:
                break
            await asyncio.sleep(0.5)
        check("a colour that is not one falls back to inherit, not to nothing",
              bad["count"] == 1 and bad["boxes"][0]["color"] == "rgb(255, 255, 255)", bad)

        http("PUT", f"/api/playlist/{current}",
             {"overlay": {"enabled": True, "text": "Item", "position": "",
                          "color": ""}}, port=HTTP)
        cleared = {}
        for _ in range(40):
            cleared = json.loads(await page.eval(COLOURS))
            if cleared["count"] == 1 and cleared["boxes"][0]["color"] == "rgb(255, 255, 255)":
                break
            await asyncio.sleep(0.5)
        check("clearing it puts the global colour back",
              cleared["count"] == 1 and cleared["boxes"][0]["color"] == "rgb(255, 255, 255)",
              cleared)
        http("PUT", f"/api/playlist/{current}", {"overlay": {"enabled": False}}, port=HTTP)

        print("\n[46] on a page from a genuinely foreign origin nothing is fetched")
        # Reached by the LAN address on purpose. Chromium's Local Network Access
        # refuses requests to 127.0.0.1 from any origin that is not itself
        # loopback -- `fetch` and `<img src="http://127.0.0.1/...">` alike. A
        # foreign page served from 127.0.0.1 would pass here while a real display
        # failed.
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

        print("\n[46a] a page whose CSP forbids inline styles is still styled")
        # A nonce-based `style-src` with no 'unsafe-inline' -- what Next.js sends,
        # and what wom.i.fll.sh sends. A <style> element the overlay appends
        # carries no nonce, so the page's policy drops the *sheet* while leaving
        # the element in the DOM: `styleEl.sheet` is null, the text is still on
        # screen, and every rule is gone. It reads as "the overlay lost its CSS",
        # and only on that one playlist item.
        #
        # Asserted on the computed style, not on the presence of a <style> node:
        # the node was there the whole time the bug existed.
        strict = subprocess.Popen([sys.executable, f"{SP}/foreign_page.py", "3062", "csp"],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        procs.append(strict)
        strict_url = f"http://{LAN}:3062/"
        check("the strict-CSP origin is serving", wait_for(
            lambda: urllib.request.urlopen(strict_url, timeout=2).status == 200, 15) is not None)

        put({"enabled": True, "text": "Streng", "position": "bottom-right",
             "size": 3.0, "background_color": "#000000", "background_alpha": 0.65},
            port=HTTP)
        http("POST", "/api/override", {"url": strict_url}, port=HTTP)

        info = {}
        for _ in range(60):
            info = json.loads(await page.eval(STYLED))
            if info.get("box") and "Streng" in (info.get("text") or ""):
                break
            await asyncio.sleep(0.5)
        check("the badge reached the strict-CSP page at all",
              info.get("box") and "Streng" in (info.get("text") or ""), info)
        check("the page really is the strict-CSP origin",
              info.get("host") == f"{LAN}:3062", info)
        # The three the dropped sheet took with it.
        check("it is still positioned by our own rules",
              info.get("position") == "fixed", info)
        check("it still has its box background",
              info.get("background") not in (None, "rgba(0, 0, 0, 0)"), info)
        check("and it is still sized in vmin, not the page default 16px",
              isinstance(info.get("fontSize"), float) and info["fontSize"] != 16.0, info)
        http("DELETE", "/api/override", port=HTTP)

        print("\n[46b] a connection code makes the overlay stand down")
        # The runtime's own contract first: suspend keeps the configuration, so
        # coming back needs no server round trip.
        put({"enabled": True, "text": "Werkstatt geschlossen", "show_clock": True,
             "position": "bottom-center"}, port=HTTP)
        # Wait for *this* text, not merely for a box: the previous case left one
        # on screen, and "a box exists" would pass before the edit arrived.
        for _ in range(40):
            if "Werkstatt geschlossen" in json.loads(await page.eval(BOXES))["text"]:
                break
            await asyncio.sleep(0.5)
        await page.eval("globalThis.__ov.suspend()")
        state = json.loads(await page.eval(
            "(() => JSON.stringify(globalThis.__ov.state()))()"))
        check("suspend hides every box", state["boxes"] == 0 and state["suspended"], state)
        check("but the layers are still known, so nothing has to be re-fetched",
              state["layers"] >= 1, state)
        await page.eval("globalThis.__ov.resume()")
        back = json.loads(await page.eval(BOXES))
        check("resume brings it back unchanged",
              back["count"] == 1 and "Werkstatt geschlossen" in back["text"], back)

        # The flag a page sets *before* the runtime exists -- the order that really
        # happens on a display, where a pairing code arrives within a second and
        # the runtime is injected after the readiness waits. Re-evaluating the
        # script is exactly what the controller does after navigation.
        script = urllib.request.urlopen(
            f"http://127.0.0.1:{HTTP}/overlay.js", timeout=5).read().decode()
        seeded = await page.eval(
            "(() => { delete globalThis.__ov; globalThis.__ovSuspend = true; return 'gone'; })()")
        check("the runtime can be removed for the test", seeded == "gone", seeded)
        await page.eval(script)
        state = json.loads(await page.eval(
            "(() => JSON.stringify(globalThis.__ov.state()))()"))
        check("a runtime that loads after the flag starts out suspended",
              state["suspended"] is True, state)
        await page.eval("globalThis.__ov.resume()")

        print("\n[46c] the idle screen's standing code wins over the overlay")
        # code mode with a code: the idle page renders it and stands the overlay
        # down. Disabling every item is what sends the display to that page.
        http("PUT", "/api/settings", {"cast_auth": "code", "cast_code": "AB12"}, port=HTTP)
        for item in http("GET", "/api/playlist", port=HTTP)[1]:
            http("PUT", f"/api/playlist/{item['id']}", {"enabled": False}, port=HTTP)

        idle = wait_for(
            lambda: "empty_playlist" in (cdp.page_ws(9232)[1] or {}).get("url", ""), 60)
        check("the display went to the idle screen", idle is not None,
              (cdp.page_ws(9232)[1] or {}).get("url"))

        idle_ws, _ = cdp.page_ws(9232, lambda t: "empty_playlist" in t.get("url", ""))
        async with cdp.Session(idle_ws) as idle_page:
            shown = None
            for _ in range(40):
                shown = json.loads(await idle_page.eval(
                    """(() => JSON.stringify({
                         code: (document.getElementById('code') || {}).hidden === false,
                         flag: !!globalThis.__ovSuspend,
                         boxes: globalThis.__ov ? globalThis.__ov.state().boxes : null,
                       }))()"""))
                if shown["code"]:
                    break
                await asyncio.sleep(0.5)
            check("the idle screen shows the standing code", shown["code"] is True, shown)
            check("and the overlay drew nothing over it",
                  shown["flag"] is True and (shown["boxes"] in (0, None)), shown)

        http("PUT", "/api/settings", {"cast_auth": "none"}, port=HTTP)

    # Back to a playlist item, and on a fresh page session: the idle screen above
    # is a different target, so the handle from earlier no longer points at what
    # is on screen.
    for item in http("GET", "/api/playlist", port=HTTP)[1]:
        http("PUT", f"/api/playlist/{item['id']}", {"enabled": True}, port=HTTP)
    # Not by URL: the item in this test *points at* empty_playlist.html, because a
    # page served by the controller is the only reliably offline one. What tells
    # the two apart is whether the loop reports an item at all.
    back_on_item = wait_for(
        lambda: http("GET", "/api/control/current", port=HTTP)[1].get("item_id"), 60)
    check("the display is playing an item again", back_on_item is not None,
          http("GET", "/api/control/current", port=HTTP)[1])

    print("\n[47] switching it off takes it away again")
    live_ws, _ = cdp.page_ws(9232)
    async with cdp.Session(live_ws) as live:
        put({"enabled": True, "text": "noch da", "position": "top-left"}, port=HTTP)
        for _ in range(40):
            if "noch da" in json.loads(await live.eval(BOXES))["text"]:
                break
            await asyncio.sleep(0.5)

        put({"enabled": False}, port=HTTP)
        gone = None
        for _ in range(40):
            gone = json.loads(await live.eval(BOXES))["count"] == 0
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
