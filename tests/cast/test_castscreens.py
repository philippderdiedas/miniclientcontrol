"""Two screens casting at once, end to end.

Every declared display now has its own cast session, its own claim/reservation,
its own pairing code and its own room-audio contest -- this file is the proof
that "its own" really means independent rather than merely differently named.
It reuses `test_display.py`'s harness (declared screens on explicit CDP ports,
one Chrome profile per screen, `atexit` cleanup) and `test_cast.py`'s signaling
helpers (`ws`, `claim`, `wsclient`).

Only `[86]` and `[90]` need a real, running control loop -- `[86]`'s claim is
about the *playlist and current item*, not just the session state, and `[90]`'s
is about what a real browser actually navigated to. Both use
`test_display.Screens` (real headless Chrome per screen). Every other case is
about HTTP/WebSocket state that does not depend on a browser being attached at
all, so it uses `test_display.Alone` (CDP ports 9 and 10, where nothing ever
listens) -- cheaper, and it keeps a stray loop from writing anything into a
case that never asked for one. `start_chromes`/`stop_chromes` are therefore
only called when the run actually includes one of those two, not
unconditionally -- a lone `python3 test_castscreens.py 85` has no use for two
Chromes it will never attach to.

`until()` (from `test_display`) defaults to a 30s timeout, the same as
`src/cast/mod.rs`'s `DISPLAY_TIMEOUT`: a `Reservation` or an `Alone` session
that never gets a display peer is torn down by `watch_display_arrival` at
exactly that deadline. A case whose own `until()` burns the full 30s waiting on
something else is therefore racing that teardown, and later assertions in the
same case would fail for the wrong reason -- a session that watch_display_arrival
already killed, not the behaviour under test.

Pass a case number (`python3 test_castscreens.py 86`) to run just one.
"""
import asyncio
import inspect
import json
import os
import re
import ssl
import sys
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cdp
import wsclient
from test_cast import http, ws, claim, check, failures, HTTP, TLS, is_cast_display
from test_display import (
    Screens, Alone, until, make_playlist, add_item, assign, item_of,
    playlist_id_of, start_chromes, stop_chromes, PORTS,
)

# A second source address, the same trick `test_reserve.py` and `test_audio.py`
# use: a UDP "connect" to a bogus address never sends a packet, it only makes
# the kernel pick which local address would carry it, which is enough to reach
# this device's own TLS listener as something other than loopback.
LAN = os.popen("python3 -c \"import socket;s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);"
               "s.connect(('10.254.254.254',1));print(s.getsockname()[0])\"").read().strip()


def info_from_lan():
    """`/api/cast/info` as a stranger sees it -- `busy` is judged per address."""
    req = urllib.request.Request(f"https://{LAN}:{TLS}/api/cast/info")
    with urllib.request.urlopen(req, timeout=5, context=ssl._create_unverified_context()) as res:
        return json.loads(res.read().decode())


def screen(info, name):
    return next(s for s in info["screens"] if s["name"] == name)


def override_of(display):
    return http("GET", f"/api/displays/{display}/override")[1] or {}


def state_of(display):
    return http("GET", f"/api/cast/state?screen={display}")[1] or {}


def qr_svg(screen=None, port=None):
    """Raw bytes from `/api/cast/qr.svg`, bypassing `http()`'s JSON parsing --
    an SVG body is not JSON, and `http()` would raise trying to decode it (see
    `test_overlay.py`'s own `qr()` helper, which sidesteps the same thing)."""
    path = "/api/cast/qr.svg" + (f"?screen={screen}" if screen else "")
    req = urllib.request.Request(f"http://127.0.0.1:{port or HTTP}{path}")
    with urllib.request.urlopen(req, timeout=5) as res:
        return res.status, res.read().decode()


def svg_dark_cells(svg):
    """The set of dark-module grid coordinates a served QR SVG draws.

    Not a general SVG/QR decoder: `qr_svg` in `src/cast/url.rs` renders through
    the vendored `qrcode` crate's fixed svg output -- a background `<rect>` plus
    one `<path>` whose `d` is a run of `M<x> <y>h<n>v<n>H<x>V<y>` per dark
    module, one square cell each. The cell size is not hardcoded: it grows with
    the QR version (`min_dimensions` scales to fit), and a longer encoded URL --
    exactly the difference between a screen's own address and the bare chooser
    one -- already produced 6-unit cells for one and 7-unit for the other while
    this test was written. Normalised against the top-left dark module actually
    drawn, and against the cell size actually used, rather than trusting the
    absolute quiet-zone offset -- so two matrices of the same shape compare
    equal regardless of either.
    """
    matches = re.findall(r"M(\d+) (\d+)h(\d+)v\d+H", svg)
    if not matches:
        return frozenset()
    cell = int(matches[0][2])
    coords = [(int(x), int(y)) for x, y, _ in matches]
    min_x = min(x for x, _ in coords)
    min_y = min(y for _, y in coords)
    return frozenset(((x - min_x) // cell, (y - min_y) // cell) for x, y in coords)


def overlay_dark_cells(text):
    """The matrix `/api/overlay` renders for arbitrary `text`, as the same
    dark-cell-coordinate shape `svg_dark_cells` returns for a served SVG.

    This is "the matrix generated for the expected URL", the established idiom
    `test_overlay.py`'s `[41c]`/`[41e]` use (`qr_source: text` echoes `qr_text`
    verbatim -- see `settings::overlay_payload` -- regardless of which display
    is asked, so this needs no screen of its own). It is built from the exact
    same `qrcode` crate call `qr_svg` makes, just read back as a JSON grid
    instead of rendered, which is what makes comparing the two a real check of
    content and not just of the two being non-empty.
    """
    http("PUT", "/api/settings",
         {"overlay": {"enabled": True, "qr_source": "text", "qr_text": text, "text": ""}})
    rows = (http("GET", "/api/overlay")[1]["layers"][0] or {}).get("qr_modules") or []
    return frozenset((col, row) for row, line in enumerate(rows)
                      for col, ch in enumerate(line) if ch == "1")


async def case_80():
    print("\n[80] two senders cast to two screens simultaneously; each screen's "
          "override is its own and neither disturbs the other")
    with Alone():
        sr1, sw1 = await ws("sender", display="foyer")
        w1 = await wsclient.recv_json(sr1)
        check("foyer's sender is admitted", w1.get("type") == "welcome", w1)

        sr2, sw2 = await ws("sender", display="werkstatt")
        w2 = await wsclient.recv_json(sr2)
        check("werkstatt's sender is admitted too, simultaneously", w2.get("type") == "welcome", w2)

        # The barrier: both screens are genuinely active before anything about
        # "whose override is whose" is asked.
        check("both screens report an active cast",
              until(lambda: state_of("foyer").get("active") is True
                    and state_of("werkstatt").get("active") is True),
              (state_of("foyer"), state_of("werkstatt")))

        foyer_ov = override_of("foyer")
        werkstatt_ov = override_of("werkstatt")
        foyer_url = foyer_ov.get("url") or ""
        werkstatt_url = werkstatt_ov.get("url") or ""
        check("foyer's own override is the cast page naming foyer",
              foyer_ov.get("active") and is_cast_display(foyer_url) and "screen=foyer" in foyer_url,
              foyer_ov)
        check("werkstatt's own override is the cast page naming werkstatt",
              werkstatt_ov.get("active") and is_cast_display(werkstatt_url)
              and "screen=werkstatt" in werkstatt_url, werkstatt_ov)
        check("neither override names the other screen",
              "screen=werkstatt" not in foyer_url and "screen=foyer" not in werkstatt_url,
              (foyer_ov, werkstatt_ov))

        sw1.close()
        sw2.close()


async def case_81():
    print("\n[81] a ticket minted for werkstatt is refused on a socket claiming foyer")
    # Minted for werkstatt -- the *non*-primary screen -- and not foyer,
    # deliberately: `display_for_ticket` (src/cast/signaling.rs) is exactly the
    # function a mutant could replace with `state.primary()` and leave this case
    # green if the ticket happened to be foyer's own, since foyer is
    # `displays[0]`. Reversing the direction catches both the trust-the-query
    # bug this case is named for and the primary-fallback shape.
    with Alone():
        status, body = claim(display="werkstatt")
        check("werkstatt's claim succeeds", status == 200 and body.get("ticket"), (status, body))
        ticket = body["ticket"]

        # `?screen=` on the socket is read for the display role alone -- a
        # sender's screen comes only from its ticket. Appending it here is
        # exactly the attack `cast_ws`'s own comment names: trusting it would
        # let a sender claim a screen its ticket was never issued for.
        path = f"/api/cast/ws?role=sender&ticket={ticket}&screen=foyer"
        sr, sw = await wsclient.connect("127.0.0.1", HTTP, path, secure=False)
        welcome = await wsclient.recv_json(sr)
        check("the socket still opens", welcome.get("type") == "welcome", welcome)

        # The barrier: the cast genuinely landed somewhere before claiming it
        # did not land on foyer means anything.
        check("the cast lands on the ticket's own screen, werkstatt",
              until(lambda: override_of("werkstatt").get("active") is True),
              override_of("werkstatt"))

        check("foyer's own override stays untouched",
              override_of("foyer").get("active") is False, override_of("foyer"))
        # From a genuinely different address, not 127.0.0.1 -- `busy` is
        # `taken_by_other(peer.ip())`, and the only sender in this case *is*
        # 127.0.0.1, for which that predicate is always false regardless of
        # what the server does. `info_from_lan()` is what `[83]` already uses
        # for the same reason.
        check("and foyer never becomes busy for anyone",
              screen(info_from_lan(), "foyer")["busy"] is False,
              info_from_lan())

        sw.close()


async def case_82():
    print("\n[82] claiming a busy screen is refused; claiming the other succeeds "
          "in the same breath")
    with Alone():
        sr, sw = await ws("sender", display="foyer")
        welcome = await wsclient.recv_json(sr)
        check("foyer's sender is admitted, so foyer is genuinely busy",
              welcome.get("type") == "welcome", welcome)

        status, body = claim(display="foyer")
        check("the busy screen refuses a second claim", status == 409, (status, body))

        status, body = claim(display="werkstatt")
        check("the other screen is claimed in the same breath",
              status == 200 and body.get("ticket"), (status, body))

        sw.close()


async def case_83():
    print("\n[83] /api/cast/info's busy flags flip as sessions come and go, "
          "and max_edge is per screen")
    with Alone():
        info = http("GET", "/api/cast/info")[1]
        check("neither screen starts busy",
              screen(info, "foyer")["busy"] is False and screen(info, "werkstatt")["busy"] is False,
              info)
        check("neither screen starts with a known frame limit",
              screen(info, "foyer")["max_edge"] is None and screen(info, "werkstatt")["max_edge"] is None,
              info)

        dr1, dw1 = await ws("display", display="foyer")
        await wsclient.recv_json(dr1)
        await wsclient.send_json(dw1, {"type": "limits", "max_edge": 2048})
        dr2, dw2 = await ws("display", display="werkstatt")
        await wsclient.recv_json(dr2)
        await wsclient.send_json(dw2, {"type": "limits", "max_edge": 1600})

        check("foyer's limit lands on foyer",
              until(lambda: screen(http("GET", "/api/cast/info")[1], "foyer")["max_edge"] == 2048),
              http("GET", "/api/cast/info")[1])
        info = http("GET", "/api/cast/info")[1]
        check("and werkstatt's is its own, and different",
              screen(info, "werkstatt")["max_edge"] == 1600, info)

        sr, sw = await ws("sender", display="foyer")
        await wsclient.recv_json(sr)
        check("foyer becomes busy for a stranger",
              until(lambda: screen(info_from_lan(), "foyer")["busy"] is True),
              info_from_lan())
        check("but not for the holder's own address",
              screen(http("GET", "/api/cast/info")[1], "foyer")["busy"] is False,
              http("GET", "/api/cast/info")[1])
        check("the untouched screen stays free for the stranger too",
              screen(info_from_lan(), "werkstatt")["busy"] is False, info_from_lan())

        sw.close()
        # `until()` blocks the thread with `time.sleep`, which never yields to
        # this coroutine's event loop -- without a real `await` here the close
        # above is only scheduled, never actually flushed, and the poll below
        # would run forever against a socket the server has not yet noticed
        # went away.
        await sw.wait_closed()
        check("closing the sender flips busy back to false",
              until(lambda: screen(info_from_lan(), "foyer")["busy"] is False),
              info_from_lan())

        dw1.close()
        dw2.close()


async def case_84():
    print("\n[84] room audio: first caster holds it, the second is told who has "
          "it, teardown releases it, the operator can take it through /api/audio")
    with Alone():
        # `claim_audio` records ownership -- and can 409 -- before `apply_audio`
        # ever touches a backend, so the 409-naming and post-grace-release
        # checks below hold whatever this machine has. A bare `status != 409`
        # after a *successful* claim does not: with no PulseAudio/PipeWire here,
        # `apply_audio` always answers 502 (never 200), and "502 is not 409" is
        # true whether or not the request actually acted. Read once, up front,
        # and used to make the two claim-side checks assert the actual expected
        # code either way, instead of a check that cannot fail regardless of the
        # backend.
        available = bool((http("GET", "/api/audio")[1] or {}).get("available"))

        sr1, sw1 = await ws("sender", display="foyer")
        await wsclient.recv_json(sr1)
        check("foyer's sender is registered before the audio contest starts",
              until(lambda: state_of("foyer").get("active") is True),
              state_of("foyer"))

        status, body = http("POST", "/api/cast/audio?screen=foyer",
                             {"target": "sink", "action": "mute", "value": False})
        if available:
            check("the first caster holds the room audio", status == 200, (status, body))
        else:
            check("the first caster's claim is at least not refused as busy "
                  "(no audio backend here to prove it actually took hold)",
                  status == 502, (status, body))

        sr2, sw2 = await ws("sender", display="werkstatt")
        await wsclient.recv_json(sr2)
        check("werkstatt's sender is registered too",
              until(lambda: state_of("werkstatt").get("active") is True),
              state_of("werkstatt"))

        status, body = http("POST", "/api/cast/audio?screen=werkstatt",
                             {"target": "sink", "action": "mute", "value": False})
        check("the second is told who has it",
              status == 409 and "foyer" in body.get("error", ""), (status, body))

        status, body = http("GET", "/api/cast/audio?screen=werkstatt")
        check("reading its own panel is unaffected by not owning the room audio",
              status == 200, (status, body))

        status, body = http("GET", "/api/audio")
        check("the operator's own route answers regardless of who owns the room audio",
              status == 200 and body.get("available") == available, (status, body))
        status, body = http("POST", "/api/audio", {"target": "sink", "action": "mute", "value": False})
        if available:
            check("and the operator can act through it too, unblocked by the guest contest",
                  status == 200, (status, body))
        else:
            check("and the operator route is at least not blocked by the guest "
                  "contest (no audio backend here to prove it acted)",
                  status == 502, (status, body))

        sw1.close()
        await sw1.wait_closed()
        # Polled rather than a fixed `SENDER_GRACE` (5s) plus margin sleep,
        # which is tight on a loaded machine; this is deterministic and no
        # slower than the grace actually takes.
        check("the first cast's teardown frees the room for the next screen",
              until(lambda: http("POST", "/api/cast/audio?screen=werkstatt",
                                  {"target": "sink", "action": "mute", "value": False})[0] != 409,
                    timeout=15.0),
              http("POST", "/api/cast/audio?screen=werkstatt",
                   {"target": "sink", "action": "mute", "value": False}))

        sw2.close()


def case_85():
    print("\n[85] pairing codes are per screen and unique across screens while alive")
    with Alone(cast_auth="pairing"):
        http("POST", "/api/cast/pair", {"display": "foyer"})
        foyer_code = (state_of("foyer").get("pairing") or {}).get("code")
        http("POST", "/api/cast/pair", {"display": "werkstatt"})
        werkstatt_code = (state_of("werkstatt").get("pairing") or {}).get("code")
        check("both screens minted a live 4-character code",
              bool(foyer_code) and bool(werkstatt_code)
              and len(foyer_code) == 4 and len(werkstatt_code) == 4,
              (foyer_code, werkstatt_code))
        check("the two codes differ while both are alive",
              foyer_code != werkstatt_code, (foyer_code, werkstatt_code))

        # The barrier: each code genuinely opens its own screen, proving the
        # claim endpoint actually checks codes rather than refusing everything
        # -- which would otherwise make the cross-screen refusals below vacuous.
        status, body = claim(foyer_code, display="foyer")
        check("foyer's own code opens foyer", status == 200 and body.get("ticket"), (status, body))
        status, body = claim(werkstatt_code, display="werkstatt")
        check("werkstatt's own code opens werkstatt", status == 200 and body.get("ticket"), (status, body))

        # Both codes above are now consumed (single-use) and both screens hold
        # a live reservation from the successful claims, so the reservations
        # are released and fresh codes minted -- a refusal against an
        # already-spent code would prove nothing about screen isolation.
        http("DELETE", "/api/cast/claim", {"display": "foyer"})
        http("DELETE", "/api/cast/claim", {"display": "werkstatt"})
        http("POST", "/api/cast/pair", {"display": "foyer"})
        foyer_code2 = (state_of("foyer").get("pairing") or {}).get("code")
        http("POST", "/api/cast/pair", {"display": "werkstatt"})
        werkstatt_code2 = (state_of("werkstatt").get("pairing") or {}).get("code")
        # If the re-mint regressed, `code2` is `None` and `claim(None, ...)`
        # sends no code at all -- which `authorize_sender` refuses with the same
        # 403 "Falscher Code." a wrong-screen code gets. Without this, both
        # cross-screen checks below would pass while proving nothing.
        check("both screens minted fresh, live codes the second time too",
              bool(foyer_code2) and bool(werkstatt_code2),
              (foyer_code2, werkstatt_code2))
        check("and the fresh codes differ from each other",
              foyer_code2 != werkstatt_code2, (foyer_code2, werkstatt_code2))

        status, body = claim(werkstatt_code2, display="foyer")
        check("werkstatt's live code does not open foyer",
              status == 403 and "Falscher Code" in body.get("error", ""), (status, body))
        status, body = claim(foyer_code2, display="werkstatt")
        check("foyer's live code does not open werkstatt",
              status == 403 and "Falscher Code" in body.get("error", ""), (status, body))


async def case_86():
    print("\n[86] a cast on one screen leaves the other screen's overlay, "
          "playlist and current item untouched -- the negative that matters most")
    # The negative only: `/api/overlay` is primary-only by design, so there is
    # no positive to add here about a cast *hiding* an overlay on the screen it
    # actually lands on -- that is `test_overlay.py`'s `[41d]`.
    with Screens():
        http("PUT", "/api/settings",
             {"overlay": {"enabled": True, "text": "Foyer Hinweis", "hide_during_cast": True}})

        foyer_list = make_playlist("Foyer")
        werkstatt_list = make_playlist("Werkstatt")
        foyer_item = add_item(foyer_list)
        werkstatt_item = add_item(werkstatt_list)
        assign("foyer", foyer_list)
        assign("werkstatt", werkstatt_list)

        check("foyer is genuinely playing", until(lambda: item_of("foyer") == foyer_item),
              (item_of("foyer"), foyer_item))
        check("werkstatt is genuinely playing", until(lambda: item_of("werkstatt") == werkstatt_item),
              (item_of("werkstatt"), werkstatt_item))

        # `/api/overlay` always answers for the primary display (foyer, the
        # first declared). Baseline before any cast starts, so a wrongly
        # toggled "is this screen casting" flag on the wrong screen shows up
        # as a changed answer below.
        baseline = http("GET", "/api/overlay")[1]
        check("the overlay is drawn before any cast starts",
              len((baseline or {}).get("layers", [])) == 1, baseline)

        # The non-primary screen, deliberately: a mutation that always resolves
        # `state.displays[0]` (foyer, the primary) would be invisible if the
        # cast under test were aimed at foyer too.
        sr, sw = await ws("sender", display="werkstatt")
        welcome = await wsclient.recv_json(sr)
        check("the sender is admitted to werkstatt", welcome.get("type") == "welcome", welcome)

        check("the cast really lands on werkstatt",
              until(lambda: override_of("werkstatt").get("active") is True),
              override_of("werkstatt"))
        # Barrier: the real browser attached to werkstatt's own CDP port loaded
        # the cast page and opened its own display-role socket back -- proof
        # this is genuinely running end to end, not merely a database write
        # nobody is reading.
        check("werkstatt's own display browser connected back",
              until(lambda: state_of("werkstatt").get("display_connected") is True),
              state_of("werkstatt"))

        # The negative that matters most.
        check("foyer's own override stays untouched",
              override_of("foyer").get("active") is False, override_of("foyer"))
        check("foyer's playlist assignment is unchanged",
              playlist_id_of("foyer") == foyer_list, playlist_id_of("foyer"))
        check("foyer keeps playing its own current item",
              item_of("foyer") == foyer_item, item_of("foyer"))
        after = http("GET", "/api/overlay")[1]
        check("foyer's overlay is unchanged by a cast that never touched it",
              after == baseline, (baseline, after))

        sw.close()


def case_87():
    print("\n[87] ?screen= on the idle page: each screen shows its own code")
    with Alone(cast_auth="pairing"):
        http("POST", "/api/cast/pair", {"display": "foyer"})
        http("POST", "/api/cast/pair", {"display": "werkstatt"})

        foyer_state = state_of("foyer")
        werkstatt_state = state_of("werkstatt")
        check("foyer's state carries its own live code",
              bool((foyer_state.get("pairing") or {}).get("code")), foyer_state)
        check("werkstatt's state carries its own, different code",
              bool((werkstatt_state.get("pairing") or {}).get("code"))
              and werkstatt_state["pairing"]["code"] != foyer_state["pairing"]["code"],
              werkstatt_state)

        again = state_of("foyer")
        check("polling again -- as an idle page would -- still shows foyer's own "
              "code, unchanged",
              (again.get("pairing") or {}).get("code") == foyer_state["pairing"]["code"], again)

        status, body = http("GET", "/api/cast/state")
        check("an idle page with no ?screen= is refused once several screens are declared",
              status == 409, (status, body))
        check("and it names the declared screens instead of guessing",
              (body or {}).get("displays") == ["foyer", "werkstatt"], body)


def case_88():
    print("\n[88] an unscoped claim resolves with one declared screen and 409s "
          "with two")
    with Alone(names=("foyer",)):
        status, body = claim()
        check("a single declared screen resolves the unscoped claim",
              status == 200 and "ticket" in (body or {}), (status, body))

    with Alone():
        status, body = claim()
        check("two declared screens refuse the unscoped claim", status == 409, (status, body))
        check("and name both of them",
              (body or {}).get("displays") == ["foyer", "werkstatt"], body)
        # The barrier: the scoped path still answers, so the refusal above is a
        # routing decision and not a broken deployment.
        status, body = claim(display="foyer")
        check("while the scoped claim still succeeds",
              status == 200 and "ticket" in (body or {}), (status, body))


async def case_89():
    print("\n[89] cast_enabled switched off ends every session")
    with Alone():
        sr1, sw1 = await ws("sender", display="foyer")
        await wsclient.recv_json(sr1)
        sr2, sw2 = await ws("sender", display="werkstatt")
        await wsclient.recv_json(sr2)

        check("both screens are genuinely active before the switch is touched",
              until(lambda: state_of("foyer").get("active") is True
                    and state_of("werkstatt").get("active") is True),
              (state_of("foyer"), state_of("werkstatt")))

        status, _ = http("PUT", "/api/settings", {"cast_enabled": False})
        check("the switch is accepted", status == 200, status)

        check("foyer's session ends", until(lambda: state_of("foyer").get("active") is False),
              state_of("foyer"))
        check("werkstatt's session ends too",
              until(lambda: state_of("werkstatt").get("active") is False), state_of("werkstatt"))

        sw1.close()
        sw2.close()


def case_90():
    print("\n[90] a screen with no playlist lands on its own ?screen= idle page, "
          "not another screen's")
    # `[87]` tests `/api/cast/state?screen=`, the endpoint the idle page
    # *calls* once it is loaded with the right query string. Nothing anywhere
    # else tests the wiring that puts that query string on the page in the
    # first place: `empty_playlist_url` in `src/browser.rs`. Drop that query
    # parameter and every idle screen in a two-display deployment would 409
    # against `/api/cast/state` and show no pairing code -- and this entire
    # suite would stay green, since every other case here assigns a playlist
    # before it ever looks at a screen. Read directly off each screen's own
    # real Chrome over CDP, the way `test_browser.py` does, rather than through
    # the HTTP API -- this is a claim about what the loop actually navigated
    # to, not about database state.
    with Screens():
        check("foyer's own browser lands on its own idle page",
              until(lambda: "empty_playlist.html?screen=foyer"
                    in (cdp.page_ws(PORTS["foyer"])[1] or {}).get("url", "")),
              (cdp.page_ws(PORTS["foyer"])[1] or {}).get("url"))
        check("werkstatt's own browser lands on its own idle page, not foyer's",
              until(lambda: "empty_playlist.html?screen=werkstatt"
                    in (cdp.page_ws(PORTS["werkstatt"])[1] or {}).get("url", "")),
              (cdp.page_ws(PORTS["werkstatt"])[1] or {}).get("url"))


def case_91():
    print("\n[91] /api/cast/qr.svg follows ?screen= and cast_qr_target, exactly "
          "like /api/cast/state -- the picture on an idle or standby screen "
          "must encode the address printed beside it, not always the bare "
          "chooser address")
    with Alone():
        http("PUT", "/api/settings", {"cast_qr_target": "screen"})

        foyer_url = state_of("foyer")["sender_url"]
        werkstatt_url = state_of("werkstatt")["sender_url"]
        check("the two screens' own addresses differ",
              foyer_url != werkstatt_url and "screen=foyer" in foyer_url
              and "screen=werkstatt" in werkstatt_url, (foyer_url, werkstatt_url))

        status, foyer_svg = qr_svg(screen="foyer")
        check("foyer's qr.svg is served", status == 200, status)
        foyer_expected = overlay_dark_cells(foyer_url)
        foyer_served = svg_dark_cells(foyer_svg)
        check("under 'screen', foyer's QR encodes foyer's own address, module "
              "for module -- not always the bare chooser URL",
              len(foyer_served) > 0 and foyer_served == foyer_expected,
              (len(foyer_served), len(foyer_expected)))

        status, werkstatt_svg = qr_svg(screen="werkstatt")
        check("werkstatt's qr.svg is served", status == 200, status)
        werkstatt_expected = overlay_dark_cells(werkstatt_url)
        werkstatt_served = svg_dark_cells(werkstatt_svg)
        check("and werkstatt's own QR is werkstatt's address -- not foyer's, "
              "and not the chooser's",
              werkstatt_served == werkstatt_expected and werkstatt_served != foyer_served,
              (len(werkstatt_served), len(foyer_served)))

        http("PUT", "/api/settings", {"cast_qr_target": "chooser"})
        # `/api/cast/info`'s `sender_url` is unaffected by `cast_qr_target` --
        # always the bare chooser address -- so it is the reference for what
        # 'chooser' ought to produce here too, the same reasoning
        # `test_overlay.py`'s `[41e]` uses.
        chooser_url = http("GET", "/api/cast/info")[1]["sender_url"]
        check("the chooser address carries no ?screen= scoping",
              "screen=" not in chooser_url, chooser_url)
        chooser_expected = overlay_dark_cells(chooser_url)

        _, foyer_chooser_svg = qr_svg(screen="foyer")
        _, werkstatt_chooser_svg = qr_svg(screen="werkstatt")
        foyer_chooser_served = svg_dark_cells(foyer_chooser_svg)
        werkstatt_chooser_served = svg_dark_cells(werkstatt_chooser_svg)
        check("under 'chooser', foyer's picture is the bare chooser address",
              foyer_chooser_served == chooser_expected, len(foyer_chooser_served))
        check("and so is werkstatt's -- the setting decides now, not the screen",
              werkstatt_chooser_served == chooser_expected, len(werkstatt_chooser_served))
        check("which is a different code than either screen's own address above",
              chooser_expected != foyer_expected and chooser_expected != werkstatt_expected,
              (len(chooser_expected), len(foyer_expected), len(werkstatt_expected)))


CASES = [case_80, case_81, case_82, case_83, case_84, case_85, case_86, case_87,
         case_88, case_89, case_90, case_91]

# The only two cases that touch a real browser. `python3 test_castscreens.py 85`
# (say) has no use for two Chromes it will never attach to, and starting them
# unconditionally would launch both for every single-case run.
NEEDS_CHROME = {"86", "90"}


def main(wanted):
    for case in CASES:
        name = case.__name__.removeprefix("case_")
        if wanted and name not in wanted:
            continue
        if inspect.iscoroutinefunction(case):
            asyncio.run(case())
        else:
            case()


if __name__ == "__main__":
    wanted = sys.argv[1:]
    needs_chrome = not wanted or any(name in NEEDS_CHROME for name in wanted)
    if needs_chrome and not start_chromes():
        print("  FAIL  headless Chrome did not come up")
        sys.exit(1)
    try:
        main(wanted)
    finally:
        if needs_chrome:
            stop_chromes()
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
