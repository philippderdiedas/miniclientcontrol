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

        print("\n[18b] a reservation does not make the holder look busy to itself")
        # Telling a guest "someone else is casting" while they hold the reservation
        # is a dead end they cannot act on, and the claim endpoint already lets the
        # same address re-claim -- the two answers have to agree.
        check("not busy for the address that holds it",
              http("GET", "/api/cast/info")[1]["screens"][0]["busy"] is False,
              http("GET", "/api/cast/info")[1])
        req = urllib.request.Request(f"https://{LAN}:{TLS}/api/cast/info")
        with urllib.request.urlopen(req, timeout=5, context=ssl._create_unverified_context()) as res:
            other = json.load(res)
        check("but busy for a different address", other["screens"][0]["busy"] is True, other)

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

        status, _ = http("DELETE", "/api/cast/claim")
        check("release returns 204", status == 204, status)
        check("state no longer reserved", http("GET", "/api/cast/state")[1]["reserved"] is False)
        check("another address can claim now", claim_from_lan()[0] == 200)

        print("\n[21] the operator can clear a bare reservation")
        http("DELETE", "/api/cast/session")
        check("operator stop clears the reservation too",
              http("GET", "/api/cast/state")[1]["reserved"] is False)
        check("and the slot is free", claim()[0] == 200)

asyncio.run(main())
print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
sys.exit(1 if failures else 0)
