"""Outbound webhooks: the controller telling somebody else what happened.

The case that matters most is [59]. Everything else here checks that the right
payload reaches the right target; that one checks that a target which never
answers cannot reach the playlist. `Dispatcher::fire` is called from inside the
control loop in `browser.rs`, and the whole design exists to keep a stranger's
HTTP server out of the path of what is on the screen.

Which is also why this file starts a headless Chrome. `playback.item_changed`,
`playback.playlist_empty` and `display.connected` are fired *by the control
loop*, and the control loop does not run at all without a CDP connection -- with
`--no-launch-browser` and nothing listening, `browser_loop` sits in its outer
reconnect loop and no playback event is ever emitted. A suite that skipped the
browser would silently test only the four events that come from an HTTP handler,
and [59] would have nothing to stall.
"""
import asyncio
import atexit
import base64
import json
import os
import re
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
# Not `import http.server`: `http` is the request helper imported from
# test_cast below, and the module would shadow it (the same trap test_browser.py
# calls out).
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wsclient
from test_cast import Server, http, ws, check, failures, SP, claim, HTTP as HTTP_PORT

CHROME = "/usr/bin/google-chrome-stable"
CDP_PORT = 9242
CDP_URL = f"http://127.0.0.1:{CDP_PORT}"
PROFILE = f"{SP}/webhook-profile"
CERT = f"{SP}/cert.pem"
# Discard: nothing listens there, ever. A case that wants no control loop has to
# say so, because `Server`'s own default is the standard CDP port 9222 -- and a
# stray Chrome on that port (the one tests/cast/README.md warns about) would
# attach, run the loop, and inject `display.connected` and `playlist_empty`
# deliveries into receivers that are counting.
DEAD_CDP = "http://127.0.0.1:9"

# Long enough that an item fires `playback.item_changed` exactly once and then
# sits there, so a count is a count and not a race with the playlist looping.
LONG = 600

_chrome = None


def start_chrome():
    global _chrome
    shutil.rmtree(PROFILE, ignore_errors=True)
    _chrome = subprocess.Popen(
        [CHROME, "--headless=new", f"--remote-debugging-port={CDP_PORT}",
         f"--user-data-dir={PROFILE}", "--no-first-run", "--no-sandbox",
         "--disable-gpu", "--window-size=800,600", "about:blank"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(150):
        try:
            urllib.request.urlopen(f"{CDP_URL}/json/version", timeout=1)
            return True
        except Exception:
            time.sleep(0.2)
    return False


def stop_chrome():
    if _chrome:
        _chrome.terminate()
        try:
            _chrome.wait(timeout=10)
        except Exception:
            _chrome.kill()
    shutil.rmtree(PROFILE, ignore_errors=True)


class Display(Server):
    """A controller attached to the Chrome above, so the control loop runs."""

    def __init__(self, **flags):
        flags.setdefault("cdp_url", CDP_URL)
        super().__init__(**flags)


class Alone(Server):
    """A controller with nowhere to attach, for the cases that need no playback.

    Pointing it at `DEAD_CDP` is not tidiness: the plain `Server` default is CDP
    port 9222, and a browser left running there makes the control loop start and
    deliver events these cases are not expecting.
    """

    def __init__(self, **flags):
        flags.setdefault("cdp_url", DEAD_CDP)
        super().__init__(**flags)


class Receiver:
    """An HTTP server that records what it was sent.

    `behaviour` decides the answer: an int status, "hang" to accept and never
    reply, or ("redirect", location). `tls` serves the same thing over HTTPS
    with the self-signed certificate the controller generated for its own cast
    listener -- an unknown issuer, which is the whole point of `insecure_tls`.
    """

    def __init__(self, behaviour=204, tls=False):
        self.behaviour = behaviour
        self.tls = tls
        self.requests = []
        self._lock = threading.Lock()
        receiver = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def _record(self):
                length = int(self.headers.get("content-length", 0))
                body = self.rfile.read(length).decode() if length else ""
                # hyper writes the names it was given, and `render` lower-cases
                # every one of them, so look them up that way rather than by the
                # spelling the operator typed.
                headers = {k.lower(): v for k, v in self.headers.items()}
                with receiver._lock:
                    receiver.requests.append(
                        (self.command, self.path, headers, body))

            def do_POST(self):
                self._record()
                if receiver.behaviour == "hang":
                    # Accept, record, and never answer. The delivery gives up
                    # after ten seconds; the playlist must not wait for it.
                    time.sleep(40)
                    return
                if isinstance(receiver.behaviour, tuple):
                    self.send_response(301)
                    self.send_header("Location", receiver.behaviour[1])
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                self.send_response(receiver.behaviour)
                self.send_header("Content-Length", "0")
                self.end_headers()

            do_PUT = do_POST
            do_PATCH = do_POST

            def log_message(self, *a):
                pass

        class Quiet(ThreadingHTTPServer):
            daemon_threads = True
            # A refused TLS handshake -- case [66]'s whole point -- raises here.
            # Without this every run prints a traceback that looks like a fault.
            def handle_error(self, request, address):
                pass

        sock = socket.socket()
        sock.bind(("127.0.0.1", 0))
        self.port = sock.getsockname()[1]
        sock.close()
        self.server = Quiet(("127.0.0.1", self.port), Handler)
        if tls:
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(certfile=_combined_pem())
            self.server.socket = context.wrap_socket(self.server.socket, server_side=True)

    def __enter__(self):
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return self

    def __exit__(self, *a):
        self.server.shutdown()

    @property
    def url(self):
        scheme = "https" if self.tls else "http"
        return f"{scheme}://127.0.0.1:{self.port}/hook"

    def count(self):
        with self._lock:
            return len(self.requests)

    def bodies(self):
        with self._lock:
            return [json.loads(b) for (_, _, _, b) in self.requests if b]

    def wait(self, n=1, timeout=20.0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.count() >= n:
                return True
            time.sleep(0.1)
        return False


_pem_cache = []


def _combined_pem():
    """`cert.pem` is written key-first; OpenSSL wants the certificate first."""
    if _pem_cache:
        return _pem_cache[0]
    text = open(CERT).read()
    cert = re.search(r"-----BEGIN CERTIFICATE-----.*?-----END CERTIFICATE-----\n",
                     text, re.S).group(0)
    key = re.search(r"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----\n",
                    text, re.S).group(0)
    handle, path = tempfile.mkstemp(suffix=".pem")
    with os.fdopen(handle, "w") as fh:
        fh.write(cert + key)
    # It holds the cast listener's private key. Leaving a copy of that in the
    # system temp directory after every run is not something a test gets to do.
    atexit.register(lambda: os.path.exists(path) and os.remove(path))
    _pem_cache.append(path)
    return path


def add_hook(url, events, **extra):
    body = {"name": extra.pop("name", "Test"), "url": url, "events": events}
    body.update(extra)
    status, data = http("POST", "/api/webhooks", body)
    return status, (data or {}).get("id")


def add_item(url="http://127.0.0.1:9/x", duration=LONG):
    """A playlist item, and its id.

    `POST /api/playlist` answers 201 with no body, so the id has to be read
    back. The URL is never reachable; the loop navigates, fails, and moves on,
    which is enough -- `item_changed` fires before the navigation.
    """
    http("POST", "/api/playlist",
         {"url": url, "advance": {"on": "time", "seconds": duration}, "playlist_id": a_playlist()})
    rows = http("GET", "/api/playlist")[1] or []
    return rows[-1]["id"] if rows else None


def a_playlist():
    """The playlist items get added to, created on first use.

    An item belongs to a playlist since playlists became objects, and a fresh
    database has none -- the one-time backfill only fires for a database that
    already had items. Without this every POST here would fail deserialisation
    and no item would ever be created, which reads as "the loop never ran".
    """
    rows = http("GET", "/api/playlists")[1] or []
    playlist_id = (rows[0]["id"] if rows
                   else http("POST", "/api/playlists", {"name": "Test"})[1]["id"])
    # The control loop plays the playlist its display is assigned and nothing
    # else, and a database created fresh here has none at startup for the
    # registration to hand over -- so without this nothing ever reaches the
    # screen and every playback event below goes missing. This is the
    # operator's step on the displays page, done from the test.
    for row in http("GET", "/api/displays")[1] or []:
        if (row.get("schedule") or {}).get("default_playlist_id") != playlist_id:
            http("PUT", f"/api/displays/{row['name']}/schedule",
                 {"default_playlist_id": playlist_id, "windows": []})
    return playlist_id


AUTH = ("ops", "hunter2!!")


def authed(method, path, body=None, auth=None):
    """`http`, but able to present -- or deliberately withhold -- credentials.

    The shared `http` helper sends none at all, which is the right default
    everywhere else in this suite and useless for the one case that is about
    whether they are demanded.
    """
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        f"http://127.0.0.1:{HTTP_PORT}{path}", data=data, method=method,
        headers={"Content-Type": "application/json"} if data else {})
    if auth:
        token = base64.b64encode(f"{auth[0]}:{auth[1]}".encode()).decode()
        req.add_header("Authorization", "Basic " + token)
    try:
        with urllib.request.urlopen(req, timeout=5) as res:
            raw = res.read().decode()
            return res.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, raw


def hooks():
    return http("GET", "/api/webhooks")[1] or []


def until(predicate, timeout=20.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if predicate():
                return True
        except Exception:
            pass
        time.sleep(0.2)
    return False


async def until_async(predicate, timeout=20.0):
    """`until` for the one case holding a socket.

    `time.sleep` inside a coroutine parks the event loop, so a `writer.close()`
    waiting on it is never actually carried out -- the server would see the
    sender still connected and no `cast.ended` would ever fire.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if predicate():
                return True
        except Exception:
            pass
        await asyncio.sleep(0.2)
    return False


def events_of(receiver):
    return [b.get("event") for b in receiver.bodies()]


# The three helpers below exist so that a wait which failed produces a FAIL line
# and not an IndexError: a traceback escaping `main` would skip every remaining
# case *and* the failure summary, and would read as a crash rather than a test
# telling you what went wrong.
def first_hook():
    rows = hooks()
    return rows[0] if rows else {}


def last_result(name=None):
    rows = [h for h in hooks() if name is None or h.get("name") == name]
    return (rows[0].get("last_result") if rows else None) or {}


def body_of(receiver, event):
    """The first delivered body for `event`, or `{}` if there is none."""
    for payload in receiver.bodies():
        if payload.get("event") == event:
            return payload
    return {}


async def case_52():
    print("\n[52] a delivery arrives, with the body and the custom header")
    with Receiver() as receiver, Display():
        add_hook(receiver.url, ["playback.item_changed"], headers={"X-Token": "abc"})
        item_id = add_item()
        check("a request arrived", receiver.wait(1), f"{receiver.count()} requests")
        if receiver.count():
            method, path, headers, _ = receiver.requests[0]
            payload = receiver.bodies()[0]
            check("the default method is POST", method == "POST", method)
            check("the path is the one configured", path == "/hook", path)
            check("the custom header arrived", headers.get("x-token") == "abc", dict(headers))
            check("the content type defaults to JSON",
                  headers.get("content-type") == "application/json", dict(headers))
            check("the user agent names the controller",
                  (headers.get("user-agent") or "").startswith("miniclientcontrol/"),
                  dict(headers))
            check("the event names itself",
                  payload.get("event") == "playback.item_changed", payload)
            check("the item id is in the data",
                  payload.get("data", {}).get("item_id") == item_id, payload)
            check("the device is named", isinstance(payload.get("device"), str), payload)
            check("the timestamp is UTC",
                  str(payload.get("timestamp", "")).endswith("Z"), payload)
            check("the envelope is not flagged as a test", "test" not in payload, payload)


async def case_53():
    print("\n[53] a body template renders")
    with Receiver() as plain, Receiver() as shaped, Display():
        add_hook(plain.url, ["playback.item_changed"], name="Plain")
        add_hook(shaped.url, ["playback.item_changed"], name="Shaped",
                 body='{"text": {{ data.url | tojson }}, "n": {{ data.advance.seconds }}}',
                 # Header values are templates too, and an unfiltered one is the
                 # only place auto-escaping would show: `tojson` escapes `&` to
                 # `\u0026` on its own, so the body cannot prove anything here.
                 headers={"X-Url": "{{ data.url }}",
                          "X-Missing": "[{{ data.invented }}]"})
        add_item(url="http://127.0.0.1:9/a?x=1&y=2")
        check("both arrived", plain.wait(1) and shaped.wait(1),
              (plain.count(), shaped.count()))
        if plain.count() and shaped.count():
            raw = shaped.requests[0][3]
            try:
                payload = json.loads(raw)
            except ValueError:
                payload = None
            # `tojson` quoting the value is what makes the surrounding literal
            # parse at all, so this assertion is the escaping assertion.
            check("the rendered body is valid JSON", payload is not None, raw)
            reference = plain.bodies()[0]["data"]
            check("the placeholder resolved to the field it names",
                  (payload or {}).get("text") == reference["url"],
                  (payload, reference["url"]))
            check("a number renders bare", (payload or {}).get("n") == LONG, payload)
            sent = shaped.requests[0][2]
            check("an unfiltered value is not HTML-escaped (auto-escaping is off)",
                  sent.get("x-url", "").endswith("?x=1&y=2"), sent)
            check("a field this event does not carry renders empty, not an error",
                  sent.get("x-missing") == "[]", sent)


async def case_54():
    print("\n[54] a target not subscribed to the event gets nothing")
    with Receiver() as wrong, Receiver() as right, Display():
        add_hook(wrong.url, ["cast.started"], name="Wrong")
        add_hook(right.url, ["playback.item_changed"], name="Right")
        add_item()
        # The barrier: the delivery round demonstrably happened.
        check("the subscribed target received", right.wait(1), right.count())
        check("the unsubscribed one did not", wrong.count() == 0, wrong.requests)


async def case_55():
    print("\n[55] a disabled target gets nothing")
    with Receiver() as off, Receiver() as on, Display():
        add_hook(off.url, ["playback.item_changed"], name="Off", is_enabled=False)
        add_hook(on.url, ["playback.item_changed"], name="On")
        add_item()
        check("the enabled target received", on.wait(1), on.count())
        check("the disabled one did not", off.count() == 0, off.requests)


async def case_56():
    print("\n[56] credentials in a URL never reach a receiver, at either emit site")
    with Receiver() as receiver, Display():
        # Two emit sites, one receiver: redaction is a property of every place
        # that puts an operator-supplied URL on the wire, not of one of them.
        add_hook(receiver.url, ["playback.item_changed", "override.set"])
        add_item(url="http://bob:hunter2@127.0.0.1:9/panel")
        check("a request arrived", receiver.wait(1), receiver.count())
        if receiver.count():
            raw = receiver.requests[0][3]
            check("the password did not travel", "hunter2" not in raw, raw)
            check("the username did not travel", "bob" not in raw, raw)
            check("the host did travel", "127.0.0.1:9/panel" in raw, raw)
            payload = receiver.bodies()[0]
            check("the title is redacted too, not only the url",
                  "hunter2" not in str(payload.get("data", {}).get("title")), payload)

        before = receiver.count()
        http("POST", "/api/override", {"url": "http://bob:hunter2@127.0.0.1:9/panel"})
        check("the override was announced",
              until(lambda: "override.set" in events_of(receiver)), events_of(receiver))
        if receiver.count() > before:
            raw = "".join(r[3] for r in receiver.requests[before:])
            check("an override URL travels without its password",
                  "hunter2" not in raw, raw)
            check("and without its username", "bob" not in raw, raw)
            check("but with its host",
                  "127.0.0.1:9/panel" in str(body_of(receiver, "override.set")),
                  body_of(receiver, "override.set"))
        http("DELETE", "/api/override")


async def case_57():
    print("\n[57] a failing target does not stop another, and is tried once")
    with Receiver(500) as broken, Receiver(204) as fine, Display():
        add_hook(broken.url, ["playback.item_changed"], name="Broken")
        add_hook(fine.url, ["playback.item_changed"], name="Fine")
        add_item()
        check("the working target still received", fine.wait(1), fine.count())
        check("the broken one was tried too", broken.wait(1), broken.count())
        # One attempt, no retry: well past any plausible backoff.
        time.sleep(6)
        check("and tried exactly once -- no retry", broken.count() == 1, broken.requests)
        check("the failure is recorded against it",
              until(lambda: any(h["name"] == "Broken" and h["last_result"]
                                and not h["last_result"]["ok"] for h in hooks())),
              [h.get("last_result") for h in hooks()])
        recorded = last_result("Broken")
        check("the record names the status", "500" in recorded.get("outcome", ""),
              recorded)
        check("and is not flagged as a test", recorded.get("test") is False,
              recorded)


async def case_58():
    print("\n[58] a redirect is refused and the body never follows it")
    with Receiver(204) as elsewhere:
        with Receiver(("redirect", elsewhere.url)) as moved, Display():
            add_hook(moved.url, ["playback.item_changed"])
            add_item()
            check("the original was tried", moved.wait(1), moved.count())
            time.sleep(3)
            check("the redirect was not followed", elsewhere.count() == 0,
                  elsewhere.requests)
            check("the failure was recorded",
                  until(lambda: last_result().get("ok") is False), hooks())
            result = last_result()
            check("the failure names the status",
                  "301" in result.get("outcome", ""), result)
            check("and says it was not followed",
                  "not followed" in result.get("outcome", ""), result)


async def case_59():
    print("\n[59] a receiver that never answers does not stall the playlist")
    with Receiver("hang") as receiver, Display():
        add_hook(receiver.url, ["playback.item_changed"])
        first = add_item(duration=1)
        second = add_item(duration=1)
        check("the hanging receiver was reached", receiver.wait(1), receiver.count())
        started = time.time()
        # The control loop calls `fire` at the top of every item, and the clock
        # starts at the moment the delivery for *this* item reached the
        # receiver. The budget is what makes the check able to fail: a one
        # second item costs about three seconds all told, most of it the
        # navigation to an address nothing answers on, while the delivery this
        # receiver is refusing to answer does not give up for ten. So a `fire`
        # that waited for its own delivery could not get the next item on
        # screen inside BUDGET, and an unbounded window would have called that
        # a pass.
        BUDGET = 8
        seen = set()
        moved = False
        while time.time() - started < BUDGET:
            current = http("GET", "/api/control/current")[1] or {}
            item = current.get("item_id")
            if item is not None:
                seen.add(item)
            if len(seen) > 1:
                moved = True
                break
            time.sleep(0.2)
        check(f"the playlist advanced within {BUDGET}s of the delivery starting", moved,
              f"only ever saw {seen} in {round(time.time() - started, 1)}s, "
              f"expected both {first} and {second}")


async def case_60():
    print("\n[60] the test send delivers for real and says so")
    with Receiver() as receiver, Alone():
        _, hook_id = add_hook(receiver.url, ["cast.started"])
        status, data = http("POST", f"/api/webhooks/{hook_id}/test",
                            {"event": "cast.started"})
        check("the endpoint answered ok", status == 200 and data.get("ok"), (status, data))
        check("the outcome names the status", "204" in (data or {}).get("outcome", ""), data)
        check("a request arrived", receiver.wait(1), receiver.count())
        if receiver.count():
            payload = receiver.bodies()[0]
            check("it is flagged as a test", payload.get("test") is True, payload)
            check("it is the event asked for",
                  payload.get("event") == "cast.started", payload)
        check("the record says it was a test", last_result().get("test") is True,
              last_result())

        # The field is optional and the server picks.
        status, data = http("POST", f"/api/webhooks/{hook_id}/test", {})
        check("the event may be omitted", status == 200 and data.get("ok"), (status, data))
        check("a second request arrived", receiver.wait(2), receiver.count())
        if receiver.count() >= 2:
            catalogue = http("GET", "/api/webhooks/events")[1] or {}
            check("the server chose a real event",
                  receiver.bodies()[1].get("event") in
                  [e["name"] for e in catalogue.get("events", [])],
                  receiver.bodies()[1])

        status, data = http("POST", f"/api/webhooks/{hook_id}/test", {"event": "nope"})
        check("an unknown event is refused", status == 400, (status, data))
        status, data = http("POST", "/api/webhooks/9999/test", {})
        check("an unknown target is a 404", status == 404, (status, data))


async def case_61():
    print("\n[61] validation refuses what cannot work")
    with Alone():
        def refused(what, body):
            status, data = http("POST", "/api/webhooks", body)
            check(what, status == 400 and isinstance((data or {}).get("error"), str),
                  (status, data))
            return data

        base = {"name": "x", "url": "https://example.test/h", "events": []}
        refused("a non-HTTP scheme is refused", dict(base, url="ftp://example.test/h"))
        refused("a URL that is not one is refused", dict(base, url="nonsense"))
        refused("an unknown event is refused", dict(base, events=["nope.invented"]))
        refused("a method outside POST/PUT/PATCH is refused", dict(base, method="DELETE"))
        refused("a broken body template is refused", dict(base, body="{{ unclosed "))
        refused("a broken header template is refused",
                dict(base, headers={"X-A": "{% for %}"}))
        refused("an invalid header name is refused", dict(base, headers={"X A": "v"}))
        refused("an empty name is refused", dict(base, name="   "))
        refused("an oversized template is refused", dict(base, body="x" * (16 * 1024 + 1)))
        refused("an oversized header template is refused",
                dict(base, headers={"X-A": "x" * (16 * 1024 + 1)}))

        status, data = http("POST", "/api/webhooks", dict(base, name="  Padded  "))
        check("a valid one is accepted", status == 200 and "id" in (data or {}), (status, data))
        check("and its name is stored trimmed",
              [h["name"] for h in hooks()] == ["Padded"], hooks())
        check("a target with no events is allowed (it just never fires)",
              first_hook().get("events") == [], hooks())


async def case_62():
    print("\n[62] the catalogue describes what is actually delivered")
    # `Alone`, not `Server`: the assertions below pair the nth test send with the
    # nth delivery, and a control loop attached to a stray browser would slip a
    # `display.connected` in between.
    with Receiver() as receiver, Alone():
        status, cat = http("GET", "/api/webhooks/events")
        check("the catalogue is served", status == 200 and "events" in (cat or {}),
              (status, cat))
        names = [e["name"] for e in cat["events"]]
        check("it offers ten events", len(names) == 10, names)
        check("the prefixes are published, not hard-coded in the page",
              cat["field_prefix"] == "data." and cat["placeholder_suffix"] == " | tojson",
              cat)
        check("the envelope fields are published",
              cat["envelope"] == ["event", "timestamp", "device", "display"], cat["envelope"])
        check('"test" is not offered as a placeholder', "test" not in cat["envelope"], cat)

        _, hook_id = add_hook(receiver.url, names, name="All")
        for entry in cat["events"]:
            # Two counters on purpose: `wait` counts requests, while the slice
            # below indexes `bodies()`, which drops any request whose body was
            # empty. They cannot be the same number the moment a delivery
            # renders to nothing.
            before = receiver.count()
            before_bodies = len(receiver.bodies())
            status, data = http("POST", f"/api/webhooks/{hook_id}/test",
                                {"event": entry["name"]})
            if not receiver.wait(before + 1):
                check(f"{entry['name']} was delivered", False, data)
                continue
            # Located by name rather than trusted to be the next one along, so a
            # stray delivery makes this case say what it found instead of
            # comparing the wrong pair of field lists.
            payload = next((b for b in receiver.bodies()[before_bodies:]
                            if b.get("event") == entry["name"]), {})
            check(f"{entry['name']} carries exactly the fields the catalogue lists",
                  sorted(payload.get("data", {}).keys()) == sorted(entry["fields"]),
                  (entry["fields"], payload.get("data")))

        # And a chip composed the way the page composes one actually renders.
        chip = "{{ " + cat["field_prefix"] + "reconnect" + cat["placeholder_suffix"] + " }}"
        http("PUT", f"/api/webhooks/{hook_id}",
             {"name": "All", "url": receiver.url, "events": names,
              "body": '{"r": ' + chip + "}"})
        before = receiver.count()
        before_bodies = len(receiver.bodies())
        http("POST", f"/api/webhooks/{hook_id}/test", {"event": "display.connected"})
        check("a composed chip renders as JSON", receiver.wait(before + 1), receiver.count())
        if len(receiver.bodies()) > before_bodies:
            rendered = receiver.bodies()[before_bodies]
            check("and carries the boolean unquoted",
                  rendered.get("r") in (True, False), rendered)


async def case_63():
    print("\n[63] the CRUD surface round-trips, and the method reaches the wire")
    with Receiver() as receiver, Alone():
        status, hook_id = add_hook(receiver.url, ["cast.started"], name="One")
        check("create answers with an id", status == 200 and isinstance(hook_id, int),
              (status, hook_id))
        row = first_hook()
        check("the defaults are POST, enabled, no template, verified TLS",
              (row.get("method"), row.get("is_enabled"), row.get("body"),
               row.get("insecure_tls")) == ("POST", True, None, False), row)
        check("a target that never fired has no result",
              row.get("last_result", "missing") is None, row)

        status, _ = http("PUT", f"/api/webhooks/{hook_id}",
                         {"name": "Two", "url": receiver.url, "method": "PATCH",
                          "events": ["cast.ended"], "headers": {"X-B": "{{ event }}"},
                          "body": "{}", "is_enabled": True})
        check("update answers ok", status == 200, status)
        row = first_hook()
        check("every field came back changed",
              (row.get("name"), row.get("method"), row.get("events"), row.get("body"))
              == ("Two", "PATCH", ["cast.ended"], "{}"), row)

        http("POST", f"/api/webhooks/{hook_id}/test", {"event": "cast.ended"})
        check("a request arrived", receiver.wait(1), receiver.count())
        if receiver.count():
            method, _, headers, _ = receiver.requests[0]
            check("the configured method is what was sent", method == "PATCH", method)
            check("a header value is a template too",
                  headers.get("x-b") == "cast.ended", dict(headers))

        status, _ = http("PUT", "/api/webhooks/9999",
                         {"name": "x", "url": "https://example.test/h", "events": []})
        check("updating a row that is not there is a 404", status == 404, status)
        http("DELETE", f"/api/webhooks/{hook_id}")
        check("delete removes it", hooks() == [], hooks())


async def case_64():
    print("\n[64] the idle screen and the browser announce themselves once each")
    with Receiver() as receiver, Display():
        add_hook(receiver.url, ["playback.playlist_empty", "display.connected",
                                "playback.item_changed"])
        check("both arrive with an empty playlist", receiver.wait(2), receiver.bodies())
        seen = events_of(receiver)
        check("the browser announced itself", "display.connected" in seen, seen)
        # Only the first-connection half of `reconnect` is exercised. The other
        # half (`connected_before` in browser.rs) needs the CDP connection to
        # drop and come back, which means killing and restarting the browser
        # mid-case -- slower and markedly more fragile than what it would buy.
        # This case is not proof that `reconnect: true` ever happens.
        check("and not as a reconnect",
              body_of(receiver, "display.connected").get("data", {})
              .get("reconnect") is False, receiver.bodies())
        check("the idle screen announced itself", "playback.playlist_empty" in seen, seen)
        # The idle branch re-runs every five seconds. Edge-triggered means it
        # says so once, not every pass.
        time.sleep(13)
        check("the idle screen said so once, not once per pass",
              events_of(receiver).count("playback.playlist_empty") == 1,
              events_of(receiver))
        # The barrier: the loop is alive and still firing.
        item_id = add_item()
        check("the loop is still running", until(
            lambda: "playback.item_changed" in events_of(receiver)), events_of(receiver))
        http("DELETE", f"/api/playlist/{item_id}")
        check("and the second emptying is announced afresh", until(
            lambda: events_of(receiver).count("playback.playlist_empty") == 2, 20),
            events_of(receiver))
        check("the browser still announced itself only once",
              events_of(receiver).count("display.connected") == 1, events_of(receiver))


async def case_65():
    print("\n[65] a cast announces its whole life cycle")
    # In the default auth mode the display is pinned at the same instant the
    # sender registers, so nothing here can tell the two apart. That is [67]'s
    # job; this case is the life cycle -- claim, socket, pin, drop, grace, hand
    # the playlist back.
    with Receiver() as receiver, Alone():
        add_hook(receiver.url, ["cast.started", "cast.ended", "override.set",
                                "override.cleared"])
        status, body = claim()
        check("the claim was accepted", status == 200, (status, body))
        await asyncio.sleep(2)
        check("a claim alone announces nothing", receiver.count() == 0, receiver.requests)

        reader, writer = await wsclient.connect(
            "127.0.0.1", HTTP_PORT, f"/api/cast/ws?role=sender&ticket={body['ticket']}",
            secure=False)
        await wsclient.recv_json(reader)
        check("the sender's socket announces the cast",
              await until_async(lambda: "cast.started" in events_of(receiver)),
              events_of(receiver))
        started = [b for b in receiver.bodies() if b["event"] == "cast.started"]
        check("it carries the sender's address and mode",
              started and started[0]["data"] == {"sender_ip": "127.0.0.1", "mode": "cast"},
              started)
        check("and the display being pinned is its own event",
              await until_async(lambda: any(b["event"] == "override.set"
                                            and b["data"]["source"] == "cast"
                                            for b in receiver.bodies())),
              receiver.bodies())
        check("the cast is announced once, not once per socket",
              events_of(receiver).count("cast.started") == 1, events_of(receiver))

        writer.close()
        try:
            await writer.wait_closed()
        except Exception:
            pass
        check("and the end is announced after the grace period",
              await until_async(lambda: "cast.ended" in events_of(receiver), 25),
              events_of(receiver))
        ended = [b for b in receiver.bodies() if b["event"] == "cast.ended"]
        check("with a reason and a duration",
              ended and isinstance(ended[0]["data"]["reason"], str)
              and isinstance(ended[0]["data"]["duration_secs"], int), ended)
        check("and the playlist is handed back",
              await until_async(lambda: any(b["event"] == "override.cleared"
                                            and b["data"]["source"] == "cast"
                                            for b in receiver.bodies())),
              receiver.bodies())


async def case_66():
    print("\n[66] a self-signed receiver is refused unless the target opts in")
    with Alone():
        # Inside the Server: the certificate is the one it generates for its own
        # cast listener, so it exists by the time the receiver needs it.
        with Receiver(tls=True) as receiver:
            _, strict = add_hook(receiver.url, ["cast.started"], name="Strict")
            status, data = http("POST", f"/api/webhooks/{strict}/test",
                                {"event": "cast.started"})
            check("verification is on by default, so the delivery fails",
                  status == 200 and data.get("ok") is False, (status, data))
            check("and the reason is the handshake, not a timeout or a refusal",
                  "TLS" in (data or {}).get("outcome", ""), data)
            check("nothing reached the receiver", receiver.count() == 0, receiver.requests)

            _, loose = add_hook(receiver.url, ["cast.started"], name="Loose",
                                insecure_tls=True)
            status, data = http("POST", f"/api/webhooks/{loose}/test",
                                {"event": "cast.started"})
            check("with insecure_tls the same certificate is accepted",
                  status == 200 and data.get("ok") is True, (status, data))
            check("and the payload arrived over TLS", receiver.wait(1), receiver.count())
            if receiver.count():
                check("it is the event asked for",
                      receiver.bodies()[0].get("event") == "cast.started",
                      receiver.bodies()[0])
            check("the flag survives a round trip",
                  sorted((h["name"], h["insecure_tls"]) for h in hooks())
                  == [("Loose", True), ("Strict", False)], hooks())


async def case_67():
    print("\n[67] cast.started fires where the sender registers, not where the "
          "display is pinned")
    # The discriminating case, and the reason it needs pairing mode: in the
    # default mode `activate_display` and `register_peer` happen at the same
    # instant, so every assertion in [65] would still pass with the event moved
    # back into `activate_display`. That move *was* the bug -- `start_pairing`
    # pins the cast page to show the code, and `activate_display` then early
    # returns for the real sender because `holding_override` is already true, so
    # the event fired for the code and never for the cast. Here the two are
    # thirty seconds apart if they need to be.
    with Receiver() as receiver, Alone(cast_auth="pairing"):
        add_hook(receiver.url, ["cast.started", "override.set"])
        display_reader, display_writer = await ws("display")
        await wsclient.recv_json(display_reader)

        status, _ = http("POST", "/api/cast/pair", {})
        check("pairing was started", status == 200, status)
        pushed = await wsclient.recv_json(display_reader)
        code = pushed.get("code")
        check("the display was handed a code", isinstance(code, str) and len(code) == 4,
              pushed)

        # The barrier: pinning the display for the code demonstrably reached the
        # receiver, so the absence of `cast.started` below is an absence and not
        # a delivery round that never happened.
        check("pinning the display for the code is announced as an override",
              await until_async(lambda: body_of(receiver, "override.set")
                                .get("data", {}).get("source") == "cast"),
              receiver.bodies())
        check("and nothing has announced a cast yet",
              "cast.started" not in events_of(receiver), events_of(receiver))

        sender_reader, sender_writer = await ws("sender", code)
        welcome = await wsclient.recv_json(sender_reader)
        check("the code was accepted", welcome.get("type") == "welcome", welcome)
        check("the sender registering is what announces the cast",
              await until_async(lambda: "cast.started" in events_of(receiver)),
              events_of(receiver))
        check("carrying the sender's address, which only that site knows",
              body_of(receiver, "cast.started").get("data", {}).get("sender_ip")
              == "127.0.0.1", body_of(receiver, "cast.started"))
        check("and exactly once",
              events_of(receiver).count("cast.started") == 1, events_of(receiver))
        sender_writer.close()
        display_writer.close()


async def case_68():
    print("\n[68] a guest page redacts its URL and is not a cast")
    with Receiver() as receiver, Alone(guest_pages="on"):
        add_hook(receiver.url, ["guest_page.shown", "override.set", "cast.started"])
        reader, writer = await ws("sender", mode="page")
        await wsclient.recv_json(reader)
        await wsclient.send_json(
            writer, {"type": "present",
                     "url": "http://bob:hunter2@example.test/wiki", "scroll": "none"})
        check("the page being shown is announced",
              await until_async(lambda: "guest_page.shown" in events_of(receiver)),
              events_of(receiver))
        raw = "".join(r[3] for r in receiver.requests)
        check("the guest's password never reaches a receiver", "hunter2" not in raw, raw)
        check("nor their username", "bob" not in raw, raw)
        check("but the address itself does",
              "example.test/wiki" in str(body_of(receiver, "guest_page.shown")),
              body_of(receiver, "guest_page.shown"))
        check("the pinned page is its own override, sourced to the guest",
              await until_async(lambda: body_of(receiver, "override.set")
                                .get("data", {}).get("source") == "guest_page"),
              receiver.bodies())
        # The second discriminator: a page-mode sender registers, and registering
        # is where `cast.started` lives -- but a page is not a cast.
        check("and a page-mode sender announces no cast",
              "cast.started" not in events_of(receiver), events_of(receiver))
        writer.close()


async def case_69():
    print("\n[69] the webhook API is operator-only, and something asserts it")
    # The property is otherwise guarded by two *absences* -- not in
    # `is_display_path`, not in `cast::is_cast_public_path` -- and an absence is
    # exactly what a later edit removes without anything going red. A target's
    # headers are where a third party's API token lives, so this is the one
    # route set worth pinning down from outside the binary.
    with Alone(basic_auth_user="ops", basic_auth_password="hunter2!!"):
        check("reading the targets without credentials is refused",
              authed("GET", "/api/webhooks")[0] == 401)
        # From 127.0.0.1, which is the point: the loopback exemption covers
        # `is_display_path` only, and a webhook route is not one.
        status, rows = authed("GET", "/api/webhooks", auth=AUTH)
        check("and succeeds with them", status == 200 and isinstance(rows, list), (status, rows))

        target = {"name": "Gated", "url": "http://127.0.0.1:9/hook",
                  "events": ["playback.item_changed"]}
        check("creating one without credentials is refused",
              authed("POST", "/api/webhooks", target)[0] == 401)
        status, body = authed("POST", "/api/webhooks", target, auth=AUTH)
        check("and succeeds with them", status == 200 and body.get("id"), (status, body))
        hook_id = (body or {}).get("id")

        check("the test send without credentials is refused",
              authed("POST", f"/api/webhooks/{hook_id}/test", {})[0] == 401)
        check("editing one without credentials is refused",
              authed("PUT", f"/api/webhooks/{hook_id}", target)[0] == 401)
        check("deleting one without credentials is refused",
              authed("DELETE", f"/api/webhooks/{hook_id}")[0] == 401)
        check("the row is still there",
              len(authed("GET", "/api/webhooks", auth=AUTH)[1] or []) == 1)
        check("and deleting it with credentials works",
              authed("DELETE", f"/api/webhooks/{hook_id}", auth=AUTH)[0] == 200)

        # The catalogue is a route too, and it is the one a page hits first.
        check("even the event catalogue is behind the credentials",
              authed("GET", "/api/webhooks/events")[0] == 401)


async def case_70():
    print("\n[70] a delivery names the display it is about")
    # Not `Display`: that helper defaults `--cdp-url` to the running Chrome,
    # and `--display` refuses to coexist with a pinned `--cdp-url` (its port is
    # derived from the display's position instead). Naming the port after the
    # colon points this one declared display at the same Chrome directly.
    with Receiver() as receiver, Server(display=f"werkstatt:{CDP_PORT}"):
        add_hook(receiver.url, ["playback.item_changed"])
        add_item()
        check("a request arrived", receiver.wait(1), receiver.count())
        if receiver.count():
            payload = receiver.bodies()[0]
            check("the display names the screen the event is about",
                  payload.get("display") == "werkstatt", payload)
            # Additive: a target configured before several displays existed
            # keeps receiving everything it received before, in the same shape.
            check("the device is still named beside it",
                  isinstance(payload.get("device"), str), payload)
            check("and the event itself is unaffected",
                  payload.get("event") == "playback.item_changed", payload)


CASES = [case_52, case_53, case_54, case_55, case_56, case_57, case_58, case_59,
         case_60, case_61, case_62, case_63, case_64, case_65, case_66, case_67,
         case_68, case_69, case_70]


async def main(wanted):
    for case in CASES:
        if wanted and case.__name__.removeprefix("case_") not in wanted:
            continue
        await case()


if __name__ == "__main__":
    # A case number or several runs just those. The whole file takes about 50
    # seconds (measured, and what tests/cast/README.md says), and iterating on
    # one case should not.
    wanted = sys.argv[1:]
    if not start_chrome():
        print("  FAIL  headless Chrome did not come up on {}".format(CDP_PORT))
        sys.exit(1)
    try:
        asyncio.run(main(wanted))
    finally:
        stop_chrome()
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
