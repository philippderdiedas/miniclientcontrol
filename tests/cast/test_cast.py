"""End-to-end checks for the cast signaling relay and its override coupling."""
import asyncio, json, os, socket, subprocess, sys, time, urllib.request, urllib.error
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wsclient

BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "target", "debug", "miniclientcontrol")
SP = os.path.dirname(os.path.abspath(__file__))
HTTP, TLS = 3021, 3464
failures = []

def check(name, ok, detail=""):
    print(("  PASS  " if ok else "  FAIL  ") + name + (("  -- " + str(detail)) if not ok else ""))
    if not ok:
        failures.append(name)

def http(method, path, body=None, port=None):
    url = f"http://127.0.0.1:{port or HTTP}{path}"
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method,
                                 headers={"Content-Type": "application/json"} if data else {})
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

class Server:
    """A controller process.

    `fresh` wipes the database first. Settings are persistent now, so without
    this the outcome of one file would depend on which file ran before it.
    """
    def __init__(self, fresh=True, **flags):
        self.flags = flags
        self.fresh = fresh
        self.proc = None
    def __enter__(self):
        if self.fresh:
            for leftover in ("t.db", "t.db-wal", "t.db-shm"):
                try:
                    os.remove(os.path.join(SP, leftover))
                except FileNotFoundError:
                    pass
        # No browser: these tests exercise HTTP and signaling. Without this the
        # controller would helpfully start a Chrome for every case.
        cmd = [BIN, "--port", str(HTTP), "--cast-tls-port", str(TLS),
               "--database-path", f"{SP}/t.db", "--assets-dir", f"{SP}/assets",
               "--cast-cert-path", f"{SP}/cert.pem", "--no-launch-browser"]
        flags = dict(self.flags)
        # Off unless a case asks for it. `auto` reaches out to the certificate
        # API and depends on this machine having a private address, which would
        # make every unrelated test here network-dependent and slower.
        flags.setdefault("managed_cert", "off")
        for k, v in flags.items():
            cmd += ["--" + k.replace("_", "-")] + ([str(v)] if v is not True else [])
        self.proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(60):
            try:
                http("GET", "/api/cast/info"); return self
            except Exception:
                time.sleep(0.15)
        raise RuntimeError("server did not come up")
    def __exit__(self, *a):
        self.proc.terminate(); self.proc.wait(timeout=10)

def claim(code=None, port=None):
    """POST /api/cast/claim -> (status, body). This is where a code is checked."""
    return http("POST", "/api/cast/claim", {"code": code} if code else {}, port=port)


async def ws(role, code=None, host="127.0.0.1", ticket="auto"):
    """Open a signaling socket, claiming the session first for senders.

    `ticket="auto"` claims with `code`; pass an explicit ticket (or None) to
    exercise the socket's own admission checks.
    """
    if role == "sender" and ticket == "auto":
        status, body = claim(code)
        assert status == 200, f"claim refused: {status} {body}"
        ticket = body["ticket"]
    elif ticket == "auto":
        ticket = None
    path = f"/api/cast/ws?role={role}" + (f"&ticket={ticket}" if ticket else "")
    return await wsclient.connect(host, HTTP, path, secure=False)


async def main_flow():
    print("\n[1] default mode: no code required")
    with Server():
        status, info = http("GET", "/api/cast/info")
        check("info reports enabled + auth none", status == 200 and info["enabled"] and info["auth"] == "none", info)

        # an operator override that the cast must put back afterwards
        http("POST", "/api/override", {"url": "http://example.invalid/before"})

        dr, dw = await ws("display")
        welcome = await wsclient.recv_json(dr)
        check("display welcome", welcome["type"] == "welcome" and welcome["role"] == "display"
              and welcome["peer"] is False, welcome)

        sr, sw = await ws("sender")
        s_welcome = await wsclient.recv_json(sr)
        check("sender welcome sees the display", s_welcome["role"] == "sender" and s_welcome["peer"] is True, s_welcome)
        check("display learns the peer arrived", (await wsclient.recv_json(dr)) == {"type": "peer", "connected": True})

        await asyncio.sleep(0.4)
        status, ov = http("GET", "/api/override")
        check("cast pinned the override to the cast page",
              ov["active"] and ov["url"].endswith("/cast_display.html"), ov)

        # relay in both directions, payload untouched
        offer = {"sdp": {"type": "offer", "sdp": "v=0 fake"}}
        await wsclient.send_json(sw, {"type": "signal", "data": offer})
        relayed = await wsclient.recv_json(dr)
        check("offer relayed verbatim to the display", relayed == {"type": "signal", "data": offer}, relayed)

        cand = {"ice": {"candidate": "candidate:1 1 udp", "sdpMLineIndex": 0}}
        await wsclient.send_json(dw, {"type": "signal", "data": cand})
        relayed = await wsclient.recv_json(sr)
        check("candidate relayed verbatim to the sender", relayed == {"type": "signal", "data": cand}, relayed)

        status, st = http("GET", "/api/cast/state")
        check("state reports an active cast", st["active"] and st["display_connected"] and st["sender"] == "127.0.0.1", st)

        # a second guest is turned away before they ever reach the screen picker
        status, body = claim()
        check("second sender is refused at claim time with a reason",
              status == 409 and "error" in body, (status, body))

        # and a socket that skips the claim step is refused too
        s2r, s2w = await ws("sender", ticket=None)
        refused = await wsclient.recv_json(s2r)
        check("a socket without a ticket is refused",
              refused["type"] == "error" and refused["code"] == "claim", refused)
        s2w.close()

        print("\n[2] sender drops -> grace period -> playlist restored")
        sw.close()
        await asyncio.sleep(1.0)
        status, ov = http("GET", "/api/override")
        check("override still held during the grace period", ov["active"] and ov["url"].endswith("cast_display.html"), ov)

        await asyncio.sleep(5.5)
        status, ov = http("GET", "/api/override")
        check("previous override restored after the grace period",
              ov["active"] and ov["url"] == "http://example.invalid/before", ov)
        status, st = http("GET", "/api/cast/state")
        check("state reports no cast", not st["active"], st)
        dw.close()

    print("\n[3] operator can cut a cast short")
    with Server():
        dr, dw = await ws("display"); await wsclient.recv_json(dr)
        sr, sw = await ws("sender"); await wsclient.recv_json(sr)
        await asyncio.sleep(0.4)
        check("override pinned", http("GET", "/api/override")[1]["url"].endswith("cast_display.html"))
        status, _ = http("DELETE", "/api/cast/session")
        check("DELETE session returns 204", status == 204, status)
        await asyncio.sleep(0.3)
        check("override cleared immediately", http("GET", "/api/override")[1]["active"] is False)
        sw.close(); dw.close()

    print("\n[4] code mode")
    with Server(cast_auth="code", cast_code="QT7X"):
        status, info = http("GET", "/api/cast/info")
        check("info reports auth code", info["auth"] == "code", info)

        status, body = claim("WRNG")
        check("wrong code is refused immediately, before any sharing",
              status == 403 and "Falscher Code" in body.get("error", ""), (status, body))
        await asyncio.sleep(0.3)
        check("a refused sender does not touch the display", http("GET", "/api/override")[1]["active"] is False)
        check("and does not hold the session", http("GET", "/api/cast/info")[1]["busy"] is False)

        status, body = claim("QT7X")
        check("correct code reserves the session",
              status == 200 and len(body.get("ticket", "")) == 32, (status, body))
        # `busy` is answered relative to the asking address, so the holder is not
        # told the slot is taken -- test_reserve covers both directions. What is
        # observable from here is that the slot is held at all.
        check("the reservation holds the slot",
              http("GET", "/api/cast/state")[1]["reserved"] is True,
              http("GET", "/api/cast/state")[1])
        check("but does not take over the display yet",
              http("GET", "/api/override")[1]["active"] is False)

        sr, sw = await wsclient.connect(
            "127.0.0.1", HTTP, f"/api/cast/ws?role=sender&ticket={body['ticket']}", secure=False)
        frame = await wsclient.recv_json(sr)
        check("the ticket opens the socket", frame["type"] == "welcome", frame)
        sw.close()

    print("\n[5] display role is loopback only")
    with Server():
        # Over TLS from the LAN address: plain HTTP is bound to loopback, so this is
        # the only way a non-loopback peer can reach the endpoint at all.
        lan = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        lan.connect(("10.254.254.254", 1))
        lan_ip = lan.getsockname()[0]
        lan.close()
        try:
            r, w = await wsclient.connect(lan_ip, TLS, "/api/cast/ws?role=display", secure=True)
            w.close()
            check("remote display rejected", False, "upgrade succeeded")
        except AssertionError as e:
            check("remote display rejected", b"403" in e.args[0], e.args[0])

    print("\n[6] misconfigured code auth fails closed, but still boots")
    # Casting must never keep the signage from starting -- the operator can fix
    # this in the admin UI, so it is a loud warning plus a closed door.
    with Server(cast_auth="code"):
        status, body = claim("ANY1")
        check("a sender is refused when no code is configured",
              status == 403 and "nicht konfiguriert" in body.get("error", ""), (status, body))
        await asyncio.sleep(0.3)
        check("and the display is left alone", http("GET", "/api/override")[1]["active"] is False)

if __name__ == "__main__":
    asyncio.run(main_flow())
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
