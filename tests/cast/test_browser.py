"""Real WebRTC session: two Chrome instances, the real browser_loop, real media.

The sender shares a synthetic camera rather than a screen, because a headless
Chrome has no desktop to pick. Everything after the getMedia call -- addTrack,
offer, relay, answer, ontrack, playback -- is the identical code path.
"""
import asyncio, json, os, shutil, socketserver, subprocess, sys, threading, time, urllib.request
# Not `import http.server`: that would bind the name `http` and shadow the
# request helper imported from test_cast below.
from http.server import BaseHTTPRequestHandler
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cdp
from test_cast import check, failures, http

SP = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "target", "debug", "miniclientcontrol")
CHROME = "/usr/bin/google-chrome-stable"
HTTP, TLS = 3031, 3474
LAN = subprocess.run(["python3", "-c",
    "import socket;s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);s.connect(('10.254.254.254',1));print(s.getsockname()[0])"],
    capture_output=True, text=True).stdout.strip()

procs = []

def spawn(cmd):
    p = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    procs.append(p)
    return p

def chrome(port, profile, *extra, prefs=None):
    shutil.rmtree(f"{SP}/{profile}", ignore_errors=True)
    if prefs:
        # Written after the wipe and before the launch. The controller only
        # writes Preferences for a browser it starts itself, and here Chrome is
        # already listening, so it connects instead and leaves these alone.
        os.makedirs(f"{SP}/{profile}/Default", exist_ok=True)
        with open(f"{SP}/{profile}/Default/Preferences", "w") as fh:
            json.dump(prefs, fh)
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

async def main():
    print(f"\n[10] real WebRTC through two Chrome instances (LAN {LAN})")

    shutil.rmtree(f"{SP}/downloads", ignore_errors=True)
    os.makedirs(f"{SP}/downloads", exist_ok=True)
    chrome(9222, "display-profile", prefs={
        # Somewhere checkable, so case [12] can assert nothing was written.
        "download": {"default_directory": f"{SP}/downloads", "prompt_for_download": False},
        "savefile": {"default_directory": f"{SP}/downloads"},
    })
    check("display chrome up", wait_for(lambda: cdp.targets(9222)) is not None)

    spawn([BIN, "--port", str(HTTP), "--cast-tls-port", str(TLS),
           "--database-path", f"{SP}/browser.db", "--assets-dir", f"{SP}/assets",
           "--cast-cert-path", f"{SP}/cert.pem", "--cdp-url", "http://127.0.0.1:9222"])
    check("controller up", wait_for(lambda: http("GET", "/api/cast/info", port=HTTP)[0] == 200) is not None)

    # the controller drives the display browser to the empty-playlist placeholder
    got = wait_for(lambda: "empty_playlist" in (cdp.page_ws(9222)[1] or {}).get("url", ""), 40)
    check("browser_loop took over the display browser", got is not None,
          (cdp.page_ws(9222)[1] or {}).get("url"))

    chrome(9223, "sender-profile", "--use-fake-ui-for-media-stream",
           "--use-fake-device-for-media-stream", "--ignore-certificate-errors",
           "--autoplay-policy=no-user-gesture-required")
    check("sender chrome up", wait_for(lambda: cdp.targets(9223)) is not None)

    sender_url = f"https://{LAN}:{TLS}/"
    ws_url, _ = cdp.page_ws(9223)
    async with cdp.Session(ws_url) as sender:
        await sender.call("Page.enable")
        await sender.call("Page.navigate", {"url": sender_url})
        loaded = None
        for _ in range(60):
            loaded = await sender.eval("document.readyState === 'complete' && !!window.Cast")
            if loaded:
                break
            await asyncio.sleep(0.4)
        check("sender page loaded over TLS", bool(loaded))
        check("TLS origin is a secure context (WebRTC precondition)",
              await sender.eval("isSecureContext") is True)
        check("getDisplayMedia is available there",
              await sender.eval("!!(navigator.mediaDevices && navigator.mediaDevices.getDisplayMedia)") is True)

        await sender.eval("document.getElementById('shareCamera').click()")

        # the controller should pin the display and the display browser follow
        pinned = wait_for(lambda: http("GET", "/api/override", port=HTTP)[1]["url"].endswith("cast_display.html"), 20)
        check("cast pinned the display override", pinned is not None,
              http("GET", "/api/override", port=HTTP)[1])

        on_cast_page = wait_for(lambda: "cast_display" in (cdp.page_ws(9222)[1] or {}).get("url", ""), 40)
        check("browser_loop navigated the display to the cast page", on_cast_page is not None,
              (cdp.page_ws(9222)[1] or {}).get("url"))

        display_ws, _ = cdp.page_ws(9222, lambda t: "cast_display" in t.get("url", ""))
        async with cdp.Session(display_ws) as display:
            streaming = None
            for _ in range(75):
                streaming = await display.eval(
                    "(() => { const v = document.getElementById('video');"
                    " return document.body.classList.contains('streaming')"
                    " && v.videoWidth > 0 && v.readyState >= 2; })()")
                if streaming:
                    break
                await asyncio.sleep(0.4)
            check("video is actually playing on the display", bool(streaming),
                  await display.eval("document.getElementById('video').videoWidth"))

            dims = await display.eval(
                "(() => { const v = document.getElementById('video');"
                " return v.videoWidth + 'x' + v.videoHeight; })()")
            print(f"        received video: {dims}")

            state = await sender.eval(
                "(() => document.getElementById('status').textContent)()")
            print(f"        sender status: {state!r}")
            check("sender reports a live connection", "läuft" in (state or ""), state)

            status, st = http("GET", "/api/cast/state", port=HTTP)
            check("state shows sender and display connected",
                  st["active"] and st["display_connected"], st)

            # The whole round trip: the display page measures its own GPU and
            # panel, the relay carries the number, and the sender holds it. This
            # is what stops a frame wider than the display's texture limit from
            # arriving and compositing as a black rectangle.
            limits = st.get("display_limits") or {}
            check("the display announced the largest frame it can show",
                  isinstance(limits.get("max_edge"), int) and limits["max_edge"] >= 320, st)
            sender_edge = await sender.eval("maxEdge")
            print(f"        display limit: {limits.get('max_edge')}, sender holds: {sender_edge}")
            check("the sender learned that limit", sender_edge == limits.get("max_edge"),
                  (sender_edge, limits))
            check("the capture is inside it", await display.eval(
                "(() => { const v = document.getElementById('video');"
                " return Math.max(v.videoWidth, v.videoHeight); })()") <= limits.get("max_edge", 0))

            # The guest's audio panel is the same code as the operator's
            # (web/audio.js). Mounting it on this page once broke the share
            # buttons, so it is worth one assertion.
            available = http("GET", "/api/cast/audio", port=HTTP)[1].get("available")
            panel_shown = await sender.eval(
                "(() => !document.getElementById('audioPanel').hidden)()")
            check("the shared audio panel follows what the device can do",
                  bool(panel_shown) == bool(available), (panel_shown, available))
            check("and the share buttons still exist after mounting it",
                  await sender.eval(
                      "(() => !!document.getElementById('shareScreen')"
                      " && !!document.getElementById('stopShare'))()") is True)

            print("\n[11] stopping the cast returns the display to the playlist")
            await sender.eval("document.getElementById('stopShare').click()")
            back = wait_for(lambda: http("GET", "/api/override", port=HTTP)[1]["active"] is False, 20)
            check("override cleared on stop", back is not None,
                  http("GET", "/api/override", port=HTTP)[1])

        recovered = wait_for(lambda: "empty_playlist" in (cdp.page_ws(9222)[1] or {}).get("url", ""), 40)
        check("display browser returned to the playlist", recovered is not None,
              (cdp.page_ws(9222)[1] or {}).get("url"))

    print("\n[12] a URL that is a file downloads nothing")
    # A URL need not be a page. Left alone, Chromium writes the file, and enough
    # of those fill an SD card and take the database and the certificate with it.
    blob = b"x" * 200_000

    class Attachment(BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Disposition", 'attachment; filename="blob.bin"')
            self.send_header("Content-Length", str(len(blob)))
            self.end_headers()
            self.wfile.write(blob)

        def log_message(self, *a):
            pass

    blobsrv = socketserver.TCPServer(("127.0.0.1", 0), Attachment)
    threading.Thread(target=blobsrv.serve_forever, daemon=True).start()
    try:
        http("POST", "/api/override",
             {"url": f"http://127.0.0.1:{blobsrv.server_address[1]}/blob.bin"}, port=HTTP)
        # Long enough that a download would have finished: 200 KB over loopback.
        time.sleep(8)
        written = os.listdir(f"{SP}/downloads")
        check("nothing was written to disk", written == [], written)
    finally:
        blobsrv.shutdown()
        http("DELETE", "/api/override", port=HTTP)

try:
    asyncio.run(main())
finally:
    for p in procs:
        p.terminate()
    for p in procs:
        try:
            p.wait(timeout=10)
        except Exception:
            p.kill()

print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
sys.exit(1 if failures else 0)
