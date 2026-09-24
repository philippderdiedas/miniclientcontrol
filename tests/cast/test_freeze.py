"""A frozen screen is noticed: the page's frame counter stops, the controller
says so, and says so again when it moves. And what a screen shows, as a picture.

The Chrome here is the test's, not the controller's, so the controller must
report a freeze without restarting it -- it only restarts a browser it started.
Uses test_webhook.py's Chrome on 9242: not concurrently with test_display.py,
test_webhook.py or test_castscreens.py.
"""
import asyncio, json, os, shutil, subprocess, sys, time, urllib.error, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cdp
from test_cast import check, failures, http, HTTP, TLS
from test_webhook import (Receiver, Display, add_hook, events_of, until_async, add_item,
                          start_chrome, stop_chrome, CDP_PORT)

PAGE = f"http://127.0.0.1:{HTTP}/login.html"

STOP = """(() => {
  // The counter stops, exactly as when the compositor stops handing out
  // frames; JavaScript keeps answering.
  const frozen = globalThis.__ovFrames;
  Object.defineProperty(globalThis, '__ovFrames', { get: () => frozen, set: () => {}, configurable: true });
  return frozen;
})()"""

START = """(() => {
  let frames = 1;
  Object.defineProperty(globalThis, '__ovFrames', { get: () => frames, set: (v) => { frames = v; }, configurable: true });
  const tick = () => { frames += 1; requestAnimationFrame(tick); };
  requestAnimationFrame(tick);
  return true;
})()"""


async def counting(page):
    for _ in range(80):
        try:
            if await page.eval("typeof globalThis.__ovFrames === 'number' && globalThis.__ovFrames > 5"):
                return True
        except Exception:
            pass
        await asyncio.sleep(0.5)
    return False


async def case_freeze():
    print("\n[170] a stalled counter is reported, and its recovery")
    with Receiver() as receiver, Display(freeze_timeout=10):
        add_hook(receiver.url, ["display.frozen", "display.recovered"])
        add_item(url=PAGE)
        ws_url, _ = cdp.page_ws(CDP_PORT)
        async with cdp.Session(ws_url) as page:
            check("the page counts frames", await counting(page), None)
            await page.eval(STOP)
            check("display.frozen arrives", await until_async(
                lambda: "display.frozen" in events_of(receiver), timeout=40), events_of(receiver))
            frozen = next((b for b in receiver.bodies() if b["event"] == "display.frozen"), {})
            check("not restarted: the controller did not start this browser",
                  frozen.get("data", {}).get("restarted") is False
                  and frozen.get("data", {}).get("seconds", 0) >= 10, frozen)
            listed = next(d for d in http("GET", "/api/displays")[1] if d["name"] == "default")
            check("the operator list says since when", listed.get("frozen_since"), listed)
            await page.eval(START)
            check("display.recovered follows", await until_async(
                lambda: "display.recovered" in events_of(receiver), timeout=20), events_of(receiver))
            listed = next(d for d in http("GET", "/api/displays")[1] if d["name"] == "default")
            check("and the list no longer says frozen", not listed.get("frozen_since"), listed)


async def case_hung():
    print("\n[170b] a page that stops answering counts as frozen too")
    # What kiosk2 showed with Xorg stopped for longer than a few seconds: even
    # `Runtime.evaluate` hangs. A busy main thread is the same from outside.
    with Receiver() as receiver, Display(freeze_timeout=10):
        add_hook(receiver.url, ["display.frozen", "display.recovered"])
        add_item(url=PAGE)
        ws_url, _ = cdp.page_ws(CDP_PORT)
        async with cdp.Session(ws_url) as page:
            check("the page counts frames", await counting(page), None)
            # 45 s of a busy main thread: detection takes the timeout (10 s) plus
            # up to a sample (5 s) plus the 3 s each unanswered evaluate waits.
            await page.eval("setTimeout(() => { const end = Date.now() + 45000; while (Date.now() < end) {} }, 100); true")
        check("display.frozen arrives while the page hangs", await until_async(
            lambda: "display.frozen" in events_of(receiver), timeout=42), events_of(receiver))
        check("and display.recovered once it answers again", await until_async(
            lambda: "display.recovered" in events_of(receiver), timeout=45), events_of(receiver))


SP = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.join(SP, "..", "..", "target", "debug", "miniclientcontrol")
OWN_CDP = 9246
OWN_PROFILE = f"/tmp/miniclientcontrol-chromium-{OWN_CDP}"


def browser_session():
    """The browser's own websocket URL: a new one means a new browser process."""
    with urllib.request.urlopen(f"http://127.0.0.1:{OWN_CDP}/json/version", timeout=2) as res:
        return json.load(res)["webSocketDebuggerUrl"]


def wait(predicate, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except Exception:
            pass
        time.sleep(0.5)
    return None


async def case_restart():
    print("\n[171] a frozen screen's own browser is restarted, once")
    with Receiver() as receiver:
        for leftover in ("f.db", "f.db-wal", "f.db-shm"):
            try:
                os.remove(os.path.join(SP, leftover))
            except FileNotFoundError:
                pass
        shutil.rmtree(OWN_PROFILE, ignore_errors=True)
        # Not the harness's Server: that always passes --no-launch-browser, and
        # this case is about a browser the controller launched itself.
        controller = subprocess.Popen(
            [BIN, "--port", str(HTTP), "--cast-tls-port", str(TLS),
             "--database-path", f"{SP}/f.db", "--assets-dir", f"{SP}/assets",
             "--cast-cert-path", f"{SP}/cert.pem", "--managed-cert", "off", "--guest-pages", "off",
             "--cdp-url", f"http://127.0.0.1:{OWN_CDP}", "--freeze-timeout", "10",
             "--chromium", "/usr/bin/google-chrome-stable", "--no-kiosk",
             "--chromium-arg=--headless=new", "--chromium-arg=--no-sandbox", "--chromium-arg=--disable-gpu"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            check("controller up", wait(lambda: http("GET", "/api/cast/info")[0] == 200, 20) is not None)
            first = wait(browser_session, 40)
            check("the controller started its browser", first is not None, first)
            add_hook(receiver.url, ["display.frozen", "display.recovered"])
            add_item(url=PAGE)

            async def freeze():
                ws_url, _ = cdp.page_ws(OWN_CDP)
                async with cdp.Session(ws_url) as page:
                    if await counting(page):
                        await page.eval(STOP)
                        return True
                return False

            check("its page counts frames, then stops", await freeze(), None)
            check("display.frozen arrives", await until_async(
                lambda: "display.frozen" in events_of(receiver), timeout=40), events_of(receiver))
            frozen = next((b for b in receiver.bodies() if b["event"] == "display.frozen"), {})
            check("and says it restarted", frozen.get("data", {}).get("restarted") is True, frozen)
            second = wait(lambda: (lambda s: s if s and s != first else None)(browser_session()), 40)
            check("a new browser process answers", second is not None, (first, second))
            check("display.recovered follows once the new page paints", await until_async(
                lambda: "display.recovered" in events_of(receiver), timeout=60), events_of(receiver))

            receiver.requests.clear()
            check("its new page counts frames, then stops again", await freeze(), None)
            check("display.frozen arrives again", await until_async(
                lambda: "display.frozen" in events_of(receiver), timeout=40), events_of(receiver))
            again = next((b for b in receiver.bodies() if b["event"] == "display.frozen"), {})
            check("but the brake holds: no second restart",
                  again.get("data", {}).get("restarted") is False, again)
            check("the same browser is still up", browser_session() == second, None)
        finally:
            controller.terminate()
            try:
                controller.wait(timeout=10)
            except Exception:
                controller.kill()
            # The controller leaves its browser running on purpose; the test does not.
            subprocess.run(["pkill", "-f", f"user-data-dir={OWN_PROFILE}"], check=False)
            shutil.rmtree(OWN_PROFILE, ignore_errors=True)


async def case_screenshot():
    print("\n[172] what is on screen, as a picture")
    with Display():
        add_item(url=PAGE)
        name = "default"
        status, body = None, None
        for _ in range(40):
            req = urllib.request.Request(f"http://127.0.0.1:{HTTP}/api/displays/{name}/screenshot")
            try:
                with urllib.request.urlopen(req, timeout=10) as res:
                    status, body, headers = res.status, res.read(), res.headers
                    break
            except urllib.error.HTTPError as e:
                status = e.code
            await asyncio.sleep(0.5)
        check("a JPEG comes back", status == 200 and body[:3] == b"\xff\xd8\xff", status)
        check("never cached by the browser", headers.get("Cache-Control") == "no-store", dict(headers))
        with urllib.request.urlopen(req, timeout=10) as res:
            again = res.read()
        check("a second request within 10 s is the cached picture", again == body, None)
        status, _ = http("GET", "/api/displays/nope/screenshot")
        check("an unknown screen is a 404", status == 404, status)


    print("\n[173] the picture is an operator's, not a guest's")
    with Display():
        http("POST", "/api/users", {"name": "root", "password": "longenough", "role": "admin"})
        status, _ = http("GET", "/api/displays/default/screenshot")
        check("without credentials it is refused", status == 401, status)


CASES = [case_freeze, case_hung, case_restart, case_screenshot]


async def main():
    for case in CASES:
        await case()


if __name__ == "__main__":
    if not start_chrome():
        print(f"  FAIL  headless Chrome did not come up on {CDP_PORT}")
        sys.exit(1)
    try:
        asyncio.run(main())
    finally:
        stop_chrome()
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
