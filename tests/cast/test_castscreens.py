"""Two screens casting at once, end to end.

Every declared display now has its own cast session, its own claim/reservation,
its own pairing code and its own room-audio contest -- this file is the proof
that "its own" really means independent rather than merely differently named.
It reuses `test_display.py`'s harness (declared screens on explicit CDP ports,
one Chrome profile per screen, `atexit` cleanup) and `test_cast.py`'s signaling
helpers (`ws`, `claim`, `wsclient`).

Only `[86]` needs a real, running control loop -- it is the one case whose
claim is about the *playlist and current item*, not just the session state, so
it uses `test_display.Screens` (real headless Chrome per screen). Every other
case is about HTTP/WebSocket state that does not depend on a browser being
attached at all, so it uses `test_display.Alone` (CDP ports 9 and 10, where
nothing ever listens) -- cheaper, and it keeps a stray loop from writing
anything into a case that never asked for one.

Pass a case number (`python3 test_castscreens.py 86`) to run just one.
"""
import asyncio
import inspect
import json
import os
import ssl
import sys
import urllib.error
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wsclient
from test_cast import http, ws, claim, check, failures, HTTP, TLS, is_cast_display
from test_display import (
    Screens, Alone, until, make_playlist, add_item, assign, item_of,
    playlist_id_of, start_chromes, stop_chromes,
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
    print("\n[81] a ticket minted for foyer is refused on a socket claiming werkstatt")
    with Alone():
        status, body = claim(display="foyer")
        check("foyer's claim succeeds", status == 200 and body.get("ticket"), (status, body))
        ticket = body["ticket"]

        # `?screen=` on the socket is read for the display role alone -- a
        # sender's screen comes only from its ticket. Appending it here is
        # exactly the attack `cast_ws`'s own comment names: trusting it would
        # let a sender claim a screen its ticket was never issued for.
        path = f"/api/cast/ws?role=sender&ticket={ticket}&screen=werkstatt"
        sr, sw = await wsclient.connect("127.0.0.1", HTTP, path, secure=False)
        welcome = await wsclient.recv_json(sr)
        check("the socket still opens", welcome.get("type") == "welcome", welcome)

        # The barrier: the cast genuinely landed somewhere before claiming it
        # did not land on werkstatt means anything.
        check("the cast lands on the ticket's own screen, foyer",
              until(lambda: override_of("foyer").get("active") is True),
              override_of("foyer"))

        check("werkstatt's own override stays untouched",
              override_of("werkstatt").get("active") is False, override_of("werkstatt"))
        check("and werkstatt never becomes busy for anyone",
              screen(http("GET", "/api/cast/info")[1], "werkstatt")["busy"] is False,
              http("GET", "/api/cast/info")[1])

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
        sr1, sw1 = await ws("sender", display="foyer")
        await wsclient.recv_json(sr1)
        await asyncio.sleep(0.4)

        status, body = http("POST", "/api/cast/audio?screen=foyer",
                             {"target": "sink", "action": "mute", "value": False})
        check("the first caster holds the room audio", status != 409, (status, body))

        sr2, sw2 = await ws("sender", display="werkstatt")
        await wsclient.recv_json(sr2)
        await asyncio.sleep(0.4)

        status, body = http("POST", "/api/cast/audio?screen=werkstatt",
                             {"target": "sink", "action": "mute", "value": False})
        check("the second is told who has it",
              status == 409 and "foyer" in body.get("error", ""), (status, body))

        status, body = http("GET", "/api/cast/audio?screen=werkstatt")
        check("reading its own panel is unaffected by not owning the room audio",
              status == 200, (status, body))

        status, body = http("GET", "/api/audio")
        check("the operator's own route answers regardless of who owns the room audio",
              status == 200, (status, body))
        status, body = http("POST", "/api/audio", {"target": "sink", "action": "mute", "value": False})
        check("and the operator can act through it too, unblocked by the guest contest",
              status != 409, (status, body))

        sw1.close()
        await asyncio.sleep(6.5)  # SENDER_GRACE (5s) plus margin

        status, body = http("POST", "/api/cast/audio?screen=werkstatt",
                             {"target": "sink", "action": "mute", "value": False})
        check("the first cast's teardown frees the room for the next screen",
              status != 409, (status, body))

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

        status, body = claim(werkstatt_code2, display="foyer")
        check("werkstatt's live code does not open foyer",
              status == 403 and "Falscher Code" in body.get("error", ""), (status, body))
        status, body = claim(foyer_code2, display="werkstatt")
        check("foyer's live code does not open werkstatt",
              status == 403 and "Falscher Code" in body.get("error", ""), (status, body))


async def case_86():
    print("\n[86] a cast on one screen leaves the other screen's overlay, "
          "playlist and current item untouched -- the negative that matters most")
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


CASES = [case_80, case_81, case_82, case_83, case_84, case_85, case_86, case_87, case_88, case_89]


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
    if not start_chromes():
        print("  FAIL  headless Chrome did not come up")
        sys.exit(1)
    try:
        main(wanted)
    finally:
        stop_chromes()
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
