"""Several screens from one controller, end to end.

One process, one browser per declared display, one control loop each, and a
playlist assigned per screen. Everything here is about the two properties that
make that worth having and are invisible from inside a unit test: that two loops
really are independent (they play different lists, an override on one leaves the
other alone, a reassignment lands on the screen it names), and that a caller who
does not say which screen they mean is told rather than guessed at.

Like `test_webhook.py` this file needs a real Chrome -- one per declared display,
on its own profile directory, because two Chromiums sharing a profile corrupt it.
`current_item_id` is written by the control loop and by nothing else, and the
loop does not run at all without a CDP connection: with `--no-launch-browser` and
nothing listening, `browser_loop` sits in its outer reconnect loop. A suite that
skipped the browsers would assert `null == null` in every case here and pass
against a controller that drives no screen whatsoever.

Pass a case number (`python3 test_display.py 70`) to run just one.
"""
import atexit
import json
import os
import shutil
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
# Not `import http.server`: `http` is the request helper imported from test_cast
# below, and the module would shadow it (the same trap test_browser.py and
# test_webhook.py call out).
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import test_cast
from test_cast import Server, http, check, failures, SP, HTTP as HTTP_PORT

CHROME = "/usr/bin/google-chrome-stable"
# One Chrome per declared display. 9242 is `test_webhook.py`'s too, which is why
# the two must never run concurrently; 9243 is this file's own.
PORTS = {"foyer": 9242, "werkstatt": 9243}
# Named to match the `tests/cast/*-profile/` line in .gitignore, so a run that
# crashes before `stop_chromes` leaves nothing for `git status` to report.
PROFILES = {name: f"{SP}/display-{name}-profile" for name in PORTS}
# Discard and unassigned: nothing listens there, ever. A case that wants no
# control loop declares its displays on these instead. That is not tidiness --
# an undeclared port would fall back to 9222 by declaration index, and a stray
# Chrome on 9222 (the one tests/cast/README.md warns about) would attach, run a
# loop, and write item ids into cases that are asserting an absence of them.
DEAD = {"foyer": 9, "werkstatt": 10}
# Long enough that an item is entered once and then sits there, so an item id is
# a stable thing to assert rather than a race with the playlist looping round.
LONG = 600
# The controller's stderr for the one case that asserts an absence of errors.
# Removed on the way out: it is scratch, and an untracked file left next to the
# suite is noise in every `git status` afterwards.
LOG = f"{SP}/display-stderr.log"
atexit.register(lambda: os.path.exists(LOG) and os.remove(LOG))

_chromes = {}


def start_chromes():
    for name, port in PORTS.items():
        shutil.rmtree(PROFILES[name], ignore_errors=True)
        _chromes[name] = subprocess.Popen(
            [CHROME, "--headless=new", f"--remote-debugging-port={port}",
             f"--user-data-dir={PROFILES[name]}", "--no-first-run", "--no-sandbox",
             "--disable-gpu", "--window-size=800,600", "about:blank"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for name, port in PORTS.items():
        for _ in range(150):
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version", timeout=1)
                break
            except Exception:
                time.sleep(0.2)
        else:
            return False
    return True


def stop_chromes():
    for proc in _chromes.values():
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except Exception:
            proc.kill()
    _chromes.clear()
    for path in PROFILES.values():
        shutil.rmtree(path, ignore_errors=True)


# Registered as well as called from the `finally` below: the next suite in
# tests/cast/README.md's list wants 9242 for itself, and a browser this file left
# behind would make it fail in a way that reads exactly like a code regression.
atexit.register(stop_chromes)


class Screens(Server):
    """A controller driving the Chromes above, one declared display each.

    Deliberately not `test_webhook.py`'s `Display`, which pins `--cdp-url`:
    `display::configure` refuses `--cdp-url`, `--chromium-class` and
    `--chromium-user-data-dir` alongside any `--display`, because the declared
    branch derives all three from the display's name and position. A declared
    display names its CDP port after the colon instead, and naming it explicitly
    rather than letting it fall out of the declaration index is what keeps a case
    that declares the two screens the other way round from silently moving to
    another browser.

    `--display` takes a comma-separated list (clap `value_delimiter = ','`), so
    two screens are still one flag and `Server`'s one-flag-per-key command line
    needs no change to express them.
    """

    def __init__(self, names=("foyer", "werkstatt"), ports=None, **flags):
        ports = ports or PORTS
        self.names = list(names)
        flags.setdefault("display", ",".join(f"{n}:{ports[n]}" for n in names))
        super().__init__(**flags)


class Alone(Screens):
    """Declared screens with nowhere to attach, for the cases with no playback.

    The routing, the refusals and the playlist guard are all answered by an HTTP
    handler and need no loop -- and a loop that did run would have to be waited
    out before every assertion.
    """

    def __init__(self, names=("foyer", "werkstatt"), **flags):
        super().__init__(names=names, ports=DEAD, **flags)


class _LogTo:
    """Enough of the `subprocess` module for `Server.__enter__`, keeping the log.

    `Server` hands both streams to `DEVNULL`, which is right everywhere else and
    useless for the one case whose claim is that nothing was logged. Swapping the
    module object `test_cast` looks the name up on, rather than patching
    `subprocess.Popen` globally, keeps this out of the Chrome launches above.

    **Both** streams, and stdout is the one that matters:
    `tracing_subscriber::fmt::init()` writes to stdout, not stderr. Capturing
    only stderr collects nothing whatever the controller logs, and `errors() ==
    []` then passes against a controller logging an error every five seconds --
    which is exactly what this wrote before it was checked against a start that
    is known to fail. stderr is taken too so that a panic, which does go there,
    is not the one failure this cannot see.
    """

    DEVNULL = subprocess.DEVNULL

    def __init__(self, dest):
        self.dest = dest

    def Popen(self, cmd, **kw):
        kw["stdout"] = self.dest
        kw["stderr"] = self.dest
        return subprocess.Popen(cmd, **kw)


class Logged(Screens):
    """`Screens`, with the controller's log kept on disk."""

    def __enter__(self):
        self._log = open(LOG, "w")
        real = test_cast.subprocess
        test_cast.subprocess = _LogTo(self._log)
        try:
            return super().__enter__()
        finally:
            test_cast.subprocess = real

    def __exit__(self, *a):
        super().__exit__(*a)
        self._log.close()

    def errors(self):
        """The lines that say something went wrong -- a panic included."""
        self._log.flush()
        with open(LOG) as fh:
            return [line for line in fh.read().splitlines()
                    if "ERROR" in line or "panicked" in line]


class Receiver:
    """An HTTP server that records what it was sent.

    `test_webhook.py`'s shape, cut down to the one behaviour this file needs:
    accept, record, answer `204`.
    """

    def __init__(self):
        self.requests = []
        self._lock = threading.Lock()
        receiver = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_POST(self):
                length = int(self.headers.get("content-length", 0))
                body = self.rfile.read(length).decode() if length else ""
                with receiver._lock:
                    receiver.requests.append(body)
                self.send_response(204)
                self.send_header("Content-Length", "0")
                self.end_headers()

            def log_message(self, *a):
                pass

        class Quiet(ThreadingHTTPServer):
            daemon_threads = True

            def handle_error(self, request, address):
                pass

        self.server = Quiet(("127.0.0.1", 0), Handler)
        self.port = self.server.server_address[1]

    def __enter__(self):
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return self

    def __exit__(self, *a):
        self.server.shutdown()

    @property
    def url(self):
        return f"http://127.0.0.1:{self.port}/hook"

    def bodies(self):
        with self._lock:
            return [json.loads(b) for b in self.requests if b]


def until(predicate, timeout=30.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if predicate():
                return True
        except Exception:
            pass
        time.sleep(0.2)
    return False


def make_playlist(name):
    status, body = http("POST", "/api/playlists", {"name": name})
    assert status == 200 and (body or {}).get("id"), f"playlist refused: {status} {body}"
    return body["id"]


def add_item(playlist_id, url=None, duration=LONG):
    """An item in a named playlist, and its id.

    `POST /api/playlist` answers with no body, so the id is read back. The URL is
    never reachable: the loop navigates, fails, and moves on, which is enough --
    `current_item_id` is written before the navigation.
    """
    url = url or f"http://127.0.0.1:9/{playlist_id}"
    status, _ = http("POST", "/api/playlist",
                     {"url": url, "advance": {"on": "time", "seconds": duration}, "playlist_id": playlist_id})
    assert status in (200, 201), f"item refused: {status}"
    rows = http("GET", f"/api/playlist?playlist_id={playlist_id}")[1] or []
    return rows[-1]["id"]


def put_schedule(display, default, windows=()):
    """The whole timetable of one screen, as the displays page saves it."""
    return http("PUT", f"/api/displays/{display}/schedule",
                {"default_playlist_id": default, "windows": list(windows)})


def assign(display, playlist_id):
    """A screen's default playlist, and no windows."""
    return put_schedule(display, playlist_id)


def schedule_of(display):
    for row in http("GET", "/api/displays")[1] or []:
        if row["name"] == display:
            return row.get("schedule") or {}
    return {}


def playlist_id_of(display):
    for row in http("GET", "/api/displays")[1] or []:
        if row["name"] == display:
            return (row.get("schedule") or {}).get("default_playlist_id")
    return "no such display"


def current(display):
    """(status, item_id) for one screen."""
    status, body = http("GET", f"/api/displays/{display}/control/current")
    return status, (body or {}).get("item_id")


def item_of(display):
    return current(display)[1]


def case_70():
    print("\n[70] two screens with different playlists each play their own")
    with Screens():
        foyer_list = make_playlist("Foyer")
        werkstatt_list = make_playlist("Werkstatt")
        foyer_item = add_item(foyer_list)
        werkstatt_item = add_item(werkstatt_list)
        check("the two playlists hold different items", foyer_item != werkstatt_item,
              (foyer_item, werkstatt_item))
        assign("foyer", foyer_list)
        assign("werkstatt", werkstatt_list)

        check("the foyer plays the playlist it was assigned",
              until(lambda: item_of("foyer") == foyer_item),
              (item_of("foyer"), foyer_item))
        check("the workshop plays the one it was assigned",
              until(lambda: item_of("werkstatt") == werkstatt_item),
              (item_of("werkstatt"), werkstatt_item))
        # The assertion the whole feature is for, and the one the mutation check
        # in the task turns red: two loops, two lists, no bleed between them.
        check("and the two screens are on different items",
              item_of("foyer") != item_of("werkstatt"),
              (item_of("foyer"), item_of("werkstatt")))
        # Neither is showing something from the other's list, which "different"
        # alone would not say -- both could be wrong in different ways.
        check("neither screen shows the other's item",
              item_of("foyer") == foyer_item and item_of("werkstatt") == werkstatt_item,
              (item_of("foyer"), item_of("werkstatt")))


def case_71():
    print("\n[71] two screens assigned the same playlist mirror it")
    # The case the playlist-as-object model exists to make free: mirroring is not
    # a mode, it is what happens when both rows point at the same id.
    with Screens():
        shared = make_playlist("Gemeinsam")
        item = add_item(shared)
        assign("foyer", shared)
        assign("werkstatt", shared)

        check("the foyer picked it up", until(lambda: item_of("foyer") == item),
              (item_of("foyer"), item))
        check("the workshop picked it up too",
              until(lambda: item_of("werkstatt") == item), (item_of("werkstatt"), item))
        check("and both are on the same item",
              item_of("foyer") == item and item_of("werkstatt") == item,
              (item_of("foyer"), item_of("werkstatt")))
        check("the assignment reads back on both",
              (playlist_id_of("foyer"), playlist_id_of("werkstatt")) == (shared, shared),
              http("GET", "/api/displays")[1])


def case_72():
    print("\n[72] a screen with no playlist plays nothing, and says nothing about it")
    with Logged() as server:
        foyer_list = make_playlist("Foyer")
        # A page that actually loads, unlike everywhere else here: the
        # unreachable address the other cases use makes the *assigned* screen log
        # "Navigation failed: net::ERR_UNSAFE_PORT", and an assertion that had to
        # allow that line through would be allowing through the shape of the
        # thing it is looking for. The controller serves its own idle page, so
        # this is reachable by construction.
        foyer_item = add_item(
            foyer_list, url=f"http://127.0.0.1:{HTTP_PORT}/empty_playlist.html")
        assign("foyer", foyer_list)
        # The barrier: the other loop demonstrably ran, so `null` below is a
        # screen with nothing to play and not a controller that never started.
        check("the assigned screen plays its item",
              until(lambda: item_of("foyer") == foyer_item), (item_of("foyer"), foyer_item))

        check("nothing was assigned to the other screen",
              playlist_id_of("werkstatt") is None, http("GET", "/api/displays")[1])
        status, item = current("werkstatt")
        check("it answers, rather than refusing", status == 200, status)
        check("and reports no current item", item is None, item)
        # The idle branch re-runs every five seconds. Several passes, so a screen
        # that logged its own emptiness once per pass would be caught.
        time.sleep(12)
        check("it still reports no current item", item_of("werkstatt") is None,
              item_of("werkstatt"))
        check("and having no playlist was never an error",
              server.errors() == [], server.errors())
        # And the barrier again at the end: the process is still the one that was
        # playing, so the silence above is silence and not a dead controller.
        check("the assigned screen is still playing", item_of("foyer") == foyer_item,
              item_of("foyer"))


def case_73():
    print("\n[73] reassigning a playlist reaches the screen that is already playing")
    with Screens():
        first = make_playlist("Erste")
        second = make_playlist("Zweite")
        # A third list for the workshop, rather than parking it on `second`.
        # Parking it there is the one arrangement of two playlists in which the
        # "not moved with it" check below cannot fail: the write under test moves
        # the foyer *to* `second`, so a `PUT /api/displays/{name}` that wrote
        # every row -- exactly the bug that check names -- would leave the
        # workshop reading the same as it did before.
        third = make_playlist("Dritte")
        first_item = add_item(first)
        second_item = add_item(second)
        third_item = add_item(third)
        assign("foyer", first)
        assign("werkstatt", third)
        check("the foyer starts on the first playlist",
              until(lambda: item_of("foyer") == first_item), (item_of("foyer"), first_item))

        # Mid-item, with a ten-minute item on screen: the assignment has to
        # interrupt it rather than be picked up whenever it happens to end.
        started = time.time()
        assign("foyer", second)
        check("it moves to an item from the new playlist",
              until(lambda: item_of("foyer") == second_item), (item_of("foyer"), second_item))
        # Below the 30s `until` above, or the wait would already have bounded this
        # and the line would be saying nothing. Measured at well under a second:
        # the write pokes `playlist_signal` and the per-item `select!` takes it.
        check("without waiting out the item it was showing",
              time.time() - started < 15, round(time.time() - started, 1))
        check("and the assignment reads back", playlist_id_of("foyer") == second,
              http("GET", "/api/displays")[1])
        check("the other screen was not moved with it",
              item_of("werkstatt") == third_item and playlist_id_of("werkstatt") == third,
              (item_of("werkstatt"), playlist_id_of("werkstatt")))

        # Clearing it is the other half of the same write, and the one an
        # operator reaches for when a screen should go dark.
        assign("foyer", None)
        check("clearing the assignment empties the screen",
              until(lambda: item_of("foyer") is None), item_of("foyer"))
        # The stored assignment as well as what is on screen: the other loop is
        # parked on a ten-minute item, so a clearing write that reached its row
        # too would not show up in `current_item_id` for another nine minutes.
        # The row is what changed, so the row is what this has to read.
        check("and leaves the other one playing, still assigned",
              item_of("werkstatt") == third_item and playlist_id_of("werkstatt") == third,
              (item_of("werkstatt"), playlist_id_of("werkstatt")))


def case_74():
    print("\n[74] an override on one screen leaves the other's playback alone")
    with Screens():
        foyer_list = make_playlist("Foyer")
        werkstatt_list = make_playlist("Werkstatt")
        # Two items on the foyer: the loop records where it was interrupted and
        # resumes at the *next* one, which is what makes "the override reached
        # this loop" observable from outside rather than just stored.
        foyer_first = add_item(foyer_list)
        foyer_second = add_item(foyer_list)
        # Two on the workshop for the mirror-image reason. A loop woken by
        # somebody else's `override_signal` -- the bug this case is about --
        # finds no override of its own, falls past the `override_active` break
        # and lands on `index += 1`. With one item that walks off the end,
        # re-reads the playlist and reports the very same id, so the check below
        # would survive the leak it is written to catch; with two, the screen
        # visibly moves on.
        werkstatt_first = add_item(werkstatt_list)
        werkstatt_second = add_item(werkstatt_list)
        assign("foyer", foyer_list)
        assign("werkstatt", werkstatt_list)
        check("both screens are playing",
              until(lambda: item_of("foyer") == foyer_first
                    and item_of("werkstatt") == werkstatt_first),
              (item_of("foyer"), item_of("werkstatt")))

        status, _ = http("POST", "/api/displays/foyer/override",
                         {"url": "http://127.0.0.1:9/override"})
        check("the override was accepted", status == 200, status)
        check("it is active on the screen it names",
              until(lambda: (http("GET", "/api/displays/foyer/override")[1] or {})
                    .get("active") is True),
              http("GET", "/api/displays/foyer/override")[1])
        check("and on no other",
              (http("GET", "/api/displays/werkstatt/override")[1] or {}).get("active")
              is False, http("GET", "/api/displays/werkstatt/override")[1])

        http("DELETE", "/api/displays/foyer/override")
        # The barrier: resuming on the *second* item is only possible if the
        # foyer's loop actually broke out of the first one for the override.
        # Without it "the other screen did not change" would pass against a
        # controller where nothing changed anywhere.
        check("clearing it resumes the foyer at the next item",
              until(lambda: item_of("foyer") == foyer_second),
              (item_of("foyer"), foyer_second))
        check("the other screen never left its own first item",
              item_of("werkstatt") == werkstatt_first,
              (item_of("werkstatt"), werkstatt_first, werkstatt_second))
        check("and never had an override of its own",
              (http("GET", "/api/displays/werkstatt/override")[1] or {}).get("active")
              is False, http("GET", "/api/displays/werkstatt/override")[1])


def case_75():
    print("\n[75] an unscoped request is answered while one screen is declared "
          "and refused once several are")
    with Alone():
        status, body = http("GET", "/api/control/current")
        check("the legacy path refuses with a conflict", status == 409, (status, body))
        check("the refusal says what is wrong",
              str((body or {}).get("error", "")).startswith("Mehrere Displays"), body)
        # The names, not only the status: an operator whose script broke on an
        # upgrade needs to be told what to say instead, and a refusal carrying an
        # empty list would still be a 409.
        check("and names every declared screen",
              (body or {}).get("displays") == ["foyer", "werkstatt"], body)

        status, body = http("POST", "/api/control/current", {"item_id": 1})
        check("so does the write half", status == 409, (status, body))
        check("naming them too", (body or {}).get("displays") == ["foyer", "werkstatt"], body)
        for method, payload in (("GET", None), ("POST", {"url": "http://127.0.0.1:9/x"}),
                                ("DELETE", None)):
            status, body = http(method, "/api/override", payload)
            check(f"the unscoped override refuses {method} as well", status == 409,
                  (status, body))

        # The barrier: the scoped path is there and works, so the refusals above
        # are a routing decision and not a broken deployment.
        check("while the scoped path answers", current("foyer")[0] == 200, current("foyer"))
        status, body = http("GET", "/api/displays/kueche/control/current")
        check("a screen that is not declared is a 404", status == 404, (status, body))
        check("which also says what could have been asked for",
              (body or {}).get("displays") == ["foyer", "werkstatt"], body)

    with Alone(names=("foyer",)):
        status, body = http("GET", "/api/control/current")
        check("with one screen declared the legacy path answers normally",
              status == 200 and "item_id" in (body or {}), (status, body))
        status, body = http("GET", "/api/override")
        check("and so does the legacy override path",
              status == 200 and "active" in (body or {}), (status, body))


def case_76():
    print("\n[76] a delivery names the screen the event is about")
    with Receiver() as receiver, Screens():
        http("POST", "/api/webhooks",
             {"name": "Test", "url": receiver.url, "events": ["playback.item_changed"]})
        foyer_list = make_playlist("Foyer")
        werkstatt_list = make_playlist("Werkstatt")
        foyer_item = add_item(foyer_list)
        werkstatt_item = add_item(werkstatt_list)
        assign("foyer", foyer_list)
        assign("werkstatt", werkstatt_list)

        def named(display):
            return [b for b in receiver.bodies()
                    if b.get("event") == "playback.item_changed"
                    and b.get("display") == display]

        check("both screens announced an item",
              until(lambda: named("foyer") and named("werkstatt")),
              [(b.get("display"), b.get("data", {}).get("item_id"))
               for b in receiver.bodies()])
        # The discriminating half: a hard-coded or primary-display name would
        # satisfy "the field is present" and fail here.
        check("the foyer's delivery carries the foyer's item",
              any(b["data"]["item_id"] == foyer_item for b in named("foyer")),
              named("foyer"))
        check("the workshop's carries the workshop's",
              any(b["data"]["item_id"] == werkstatt_item for b in named("werkstatt")),
              named("werkstatt"))
        check("and neither screen is credited with the other's item",
              not any(b["data"]["item_id"] == werkstatt_item for b in named("foyer"))
              and not any(b["data"]["item_id"] == foyer_item for b in named("werkstatt")),
              [(b.get("display"), b.get("data", {}).get("item_id"))
               for b in receiver.bodies()])
        # Additive: a target configured before several screens existed still gets
        # everything it got before, in the same shape.
        check("the device is still named beside the display",
              all(isinstance(b.get("device"), str) for b in receiver.bodies()),
              receiver.bodies())


def case_77():
    print("\n[77] a playlist holding items cannot be deleted, and is told why")
    with Alone():
        empty = make_playlist("Leer")
        held = make_playlist("Voll")
        add_item(held)

        # The barrier: deleting a playlist works, so the refusal below is about
        # the items and not about the endpoint.
        status, _ = http("DELETE", f"/api/playlists/{empty}")
        check("an empty playlist deletes", status == 200, status)

        status, body = http("DELETE", f"/api/playlists/{held}")
        check("one holding an item is refused", status == 409, (status, body))
        # The count, not the sentence: singular and plural are spelled out
        # separately in the source, so matching the whole string would tie this
        # to one of the two branches.
        check("and the refusal names how many are in the way",
              "1" in str((body or {}).get("error", "")), body)
        check("the playlist is still there",
              any(p["id"] == held for p in http("GET", "/api/playlists")[1] or []),
              http("GET", "/api/playlists")[1])

        add_item(held)
        status, body = http("DELETE", f"/api/playlists/{held}")
        check("the count follows what is actually in it", status == 409
              and "2" in str((body or {}).get("error", "")), (status, body))

        rows = http("GET", f"/api/playlist?playlist_id={held}")[1] or []
        for row in rows:
            http("DELETE", f"/api/playlist/{row['id']}")
        status, _ = http("DELETE", f"/api/playlists/{held}")
        check("emptying it makes it deletable", status == 200, status)
        check("and it is gone",
              not any(p["id"] == held for p in http("GET", "/api/playlists")[1] or []),
              http("GET", "/api/playlists")[1])


def case_78():
    print("\n[78] a screen's timetable is stored in order, checked, and read back")
    # The routes and the checks are an HTTP handler's; no loop needed. On the
    # non-primary screen, so an implementation that resolved every request to
    # `displays[0]` would read the foyer's empty timetable here and fail.
    with Alone():
        office = make_playlist("Büro")
        night = make_playlist("Nacht")
        lunch = make_playlist("Mittag")
        windows = [
            {"weekdays": [1, 2, 3, 4, 5], "from": "12:00", "to": "13:00", "playlist_id": lunch},
            {"weekdays": [1, 2, 3, 4, 5], "from": "08:00", "to": "18:00", "playlist_id": office},
            {"weekdays": [5], "from": "22:00", "to": "00:00", "playlist_id": night},
        ]
        status, body = put_schedule("werkstatt", night, windows)
        check("a timetable saves", status == 200, (status, body))
        check("the answer is the timetable, windows in order",
              [w["playlist_id"] for w in (body or {}).get("windows", [])] == [lunch, office, night],
              body)
        check("an end of 00:00 reads back as the end of the day",
              body["windows"][2]["to"] == "24:00", body["windows"][2])
        check("the lunch window hides the office one where they meet",
              body.get("overlaps") == [[0, 1]], body.get("overlaps"))
        check("and what is live now is reported",
              "playlist_id" in (body.get("now") or {}) and "window" in body["now"], body.get("now"))

        stored = schedule_of("werkstatt")
        check("the list of displays embeds the same timetable",
              stored.get("default_playlist_id") == night and len(stored.get("windows", [])) == 3,
              stored)
        check("and the other screen's is untouched",
              schedule_of("foyer").get("windows") == [], schedule_of("foyer"))

        print("\n[78b] a bad row is refused, names itself, and writes nothing")
        refusals = [
            ({"weekdays": [], "from": "08:00", "to": "18:00", "playlist_id": office}, "Zeile 2"),
            ({"weekdays": [8], "from": "08:00", "to": "18:00", "playlist_id": office}, "Zeile 2"),
            ({"weekdays": [1], "from": "8 Uhr", "to": "18:00", "playlist_id": office}, "Zeile 2"),
            ({"weekdays": [1], "from": "08:00", "to": "08:00", "playlist_id": office}, "Zeile 2"),
            ({"weekdays": [1], "from": "08:00", "to": "18:00", "playlist_id": 99999}, "Zeile 2"),
        ]
        for bad, row in refusals:
            status, body = put_schedule("werkstatt", office, [windows[0], bad])
            check(f"{bad} is a 400 naming {row}",
                  status == 400 and row in (body or {}).get("error", ""), (status, body))
        after = schedule_of("werkstatt")
        check("and none of them changed anything -- the default included",
              after.get("default_playlist_id") == night and len(after.get("windows", [])) == 3,
              after)

        status, body = put_schedule("werkstatt", 99999, [])
        check("an unknown default is a 400 too", status == 400 and "error" in (body or {}),
              (status, body))

        status, _ = http("PUT", "/api/displays/werkstatt/schedule", {"windows": []})
        check("leaving out the default is refused rather than read as none", status == 400, status)

        print("\n[78c] replacing the timetable removes what was not sent")
        status, body = put_schedule("werkstatt", office, [windows[1]])
        check("one window left", status == 200 and len(body["windows"]) == 1, body)
        check("no overlap left to warn about", body["overlaps"] == [], body)

        print("\n[78d] the display's own route no longer takes a playlist")
        status, _ = http("PUT", "/api/displays/werkstatt", {"playlist_id": night})
        check("a script still sending playlist_id is refused, not ignored",
              status == 422, status)
        check("and nothing moved", playlist_id_of("werkstatt") == office, playlist_id_of("werkstatt"))
        status, _ = http("PUT", "/api/displays/werkstatt", {"label": "Werkstatt hinten"})
        check("the label still saves there", status == 200, status)


def local_minute(offset=0):
    """`HH:MM` for the local minute `offset` minutes from now -- the device's
    clock, which is the one the controller reads."""
    t = time.localtime(time.time() + offset * 60)
    return f"{t.tm_hour:02d}:{t.tm_min:02d}"


EVERY_DAY = [1, 2, 3, 4, 5, 6, 7]


def case_79():
    print("\n[79] a window that covers now switches the screen to its playlist")
    # On the non-primary screen: the foyer is displays[0], and a loop that
    # resolved every screen's timetable through `state.primary()` would pass a
    # foyer-only case unchanged.
    with Screens():
        day = make_playlist("Tag")
        window_list = make_playlist("Fenster")
        foyer_list = make_playlist("Foyer")
        day_item = add_item(day)
        window_item = add_item(window_list)
        foyer_item = add_item(foyer_list)
        assign("foyer", foyer_list)

        put_schedule("werkstatt", day, [
            {"weekdays": EVERY_DAY, "from": local_minute(-2), "to": local_minute(30),
             "playlist_id": window_list},
        ])
        check("the workshop plays the window's playlist, not its default",
              until(lambda: item_of("werkstatt") == window_item),
              (item_of("werkstatt"), window_item))
        check("the foyer keeps its own", until(lambda: item_of("foyer") == foyer_item),
              (item_of("foyer"), foyer_item))
        now = schedule_of("werkstatt").get("now") or {}
        check("and the timetable says which window is live",
              now.get("playlist_id") == window_list and now.get("window") == 0, now)

        print("\n[79b] a window starting at the next minute interrupts the item on screen")
        # At least 20 s away, so "not yet" below is not a race with the minute.
        wait = 1 if time.localtime().tm_sec < 40 else 2
        # Taken now, from the same clock reading the window is built from: taken
        # after the wait below it could already be a minute later.
        boundary = (int(time.time() // 60) + wait) * 60
        put_schedule("werkstatt", day, [
            {"weekdays": EVERY_DAY, "from": local_minute(wait), "to": local_minute(wait + 30),
             "playlist_id": window_list},
        ])
        check("until then the default plays",
              until(lambda: item_of("werkstatt") == day_item), (item_of("werkstatt"), day_item))
        check("the window is not live early", item_of("werkstatt") == day_item,
              item_of("werkstatt"))
        # The day item is 600 s long, so only the boundary timer can move it.
        switched = until(lambda: item_of("werkstatt") == window_item,
                         timeout=boundary - time.time() + 20)
        late = round(time.time() - boundary, 1)
        check("at the boundary the screen switches, mid-item", switched,
              (item_of("werkstatt"), window_item))
        check("within a few seconds of the minute, not at the end of the item",
              switched and late < 10, late)
        check("and the foyer never noticed", item_of("foyer") == foyer_item, item_of("foyer"))


CASES = [case_70, case_71, case_72, case_73, case_74, case_75, case_76, case_77, case_78, case_79]


def main(wanted):
    for case in CASES:
        if wanted and case.__name__.removeprefix("case_") not in wanted:
            continue
        case()


if __name__ == "__main__":
    wanted = sys.argv[1:]
    if not start_chromes():
        print("  FAIL  headless Chrome did not come up on {}".format(list(PORTS.values())))
        sys.exit(1)
    try:
        main(wanted)
    finally:
        stop_chromes()
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
