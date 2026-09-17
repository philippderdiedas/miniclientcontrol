"""The claim step: a session is held before anyone opens a screen picker."""
import asyncio, json, os, ssl, sys, urllib.error, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wsclient
from test_cast import Server, http, ws, claim, check, failures, HTTP, TLS

# A second source address, so "someone else" is a real peer and not a fiction.
LAN = os.popen("python3 -c \"import socket;s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);"
               "s.connect(('10.254.254.254',1));print(s.getsockname()[0])\"").read().strip()

def claim_from_lan(code=None):
    """Claim over TLS from the LAN address -- the way a real guest arrives.

    Plain HTTP is loopback-only now, so this also proves the TLS listener carries
    the peer address through to the reservation check.
    """
    body = json.dumps({"code": code} if code else {}).encode()
    req = urllib.request.Request(f"https://{LAN}:{TLS}/api/cast/claim", data=body,
                                 method="POST", headers={"Content-Type": "application/json"})
    ctx = ssl._create_unverified_context()
    try:
        with urllib.request.urlopen(req, timeout=5, context=ctx) as res:
            return res.status, json.loads(res.read().decode())
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read().decode())

async def main():
    print(f"\n[18] a claim reserves the session without touching the screen (peer {LAN})")
    with Server():
        status, body = claim()
        check("claim succeeds and returns a ticket", status == 200 and body.get("ticket"), (status, body))
        ticket = body["ticket"]

        status, st = http("GET", "/api/cast/state")
        check("state reports reserved but not yet active",
              st["reserved"] and not st["active"], st)
        check("the playlist keeps running while someone is in the picker",
              http("GET", "/api/override")[1]["active"] is False)

        status, body = claim_from_lan()
        check("a different address is refused while reserved", status == 409, (status, body))

        status, body = claim()
        check("the same address may re-claim (reload, second click)", status == 200, (status, body))
        ticket = body["ticket"]

        print("\n[19] the ticket is what opens the socket")
        sr, sw = await ws("sender", ticket="deadbeef" * 4)
        frame = await wsclient.recv_json(sr)
        check("a wrong ticket is refused", frame["type"] == "error" and frame["code"] == "claim", frame)
        sw.close()
        await asyncio.sleep(0.3)
        check("and the real reservation survives it",
              http("GET", "/api/cast/state")[1]["reserved"] is True)

        sr, sw = await ws("sender", ticket=ticket)
        frame = await wsclient.recv_json(sr)
        check("the real ticket opens the socket", frame["type"] == "welcome", frame)
        await asyncio.sleep(0.4)
        status, st = http("GET", "/api/cast/state")
        check("the reservation is consumed once streaming starts",
              st["active"] and not st["reserved"], st)
        sw.close()
        await asyncio.sleep(6.0)

        print("\n[20] releasing a claim frees the slot immediately")
        status, body = claim()
        check("claim again", status == 200, (status, body))
        check("blocked for others", claim_from_lan()[0] == 409)

        status, _ = http("DELETE", "/api/cast/claim", {})
        check("release returns 204", status == 204, status)
        check("state no longer reserved", http("GET", "/api/cast/state")[1]["reserved"] is False)
        check("another address can claim now", claim_from_lan()[0] == 200)

        print("\n[21] the operator can clear a bare reservation")
        http("DELETE", "/api/displays/default/cast/session")
        check("operator stop clears the reservation too",
              http("GET", "/api/cast/state")[1]["reserved"] is False)
        check("and the slot is free", claim()[0] == 200)

    print("\n[18b] busy is judged per screen, not once for the whole venue")
    # [18] already proved a holder is not "busy" to itself, but on a single
    # default display that would pass identically even if `busy` were computed
    # once for the venue and just repeated into every entry -- a mechanical,
    # per-screen-unaware patch could do exactly that, indexing "screens"[0] and
    # never noticing the array could hold more than one truth. Two declared
    # screens, only one of them reserved, is what actually tells them apart:
    # the reserved screen has to flip with the asking address while the
    # untouched one stays "not busy" for both.
    with Server(display="foyer:9931,werkstatt:9932"):
        status, body = claim(display="werkstatt")
        check("claim on werkstatt succeeds", status == 200 and body.get("ticket"), (status, body))

        def screen(info, name):
            return next(s for s in info["screens"] if s["name"] == name)

        status, info = http("GET", "/api/cast/info")
        check("not busy on the screen the holder itself reserved",
              screen(info, "werkstatt")["busy"] is False, info)
        check("the untouched screen is not busy either",
              screen(info, "foyer")["busy"] is False, info)

        req = urllib.request.Request(f"https://{LAN}:{TLS}/api/cast/info")
        with urllib.request.urlopen(req, timeout=5, context=ssl._create_unverified_context()) as res:
            other = json.load(res)
        check("but busy for a different address, on the reserved screen only",
              screen(other, "werkstatt")["busy"] is True, other)
        check("the screen nobody claimed stays not-busy for that address too",
              screen(other, "foyer")["busy"] is False, other)

asyncio.run(main())
print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
sys.exit(1 if failures else 0)
