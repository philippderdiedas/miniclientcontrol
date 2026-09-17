"""Venue audio control, and that only the connected caster may use it."""
import asyncio, json, os, sys, urllib.error, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wsclient
from test_cast import Server, http, ws, claim, check, failures, HTTP

def audio(method="GET", body=None):
    return http(method, "/api/cast/audio", body)

async def main():
    print("\n[30b] the operator has their own way in")
    with Server():
        status, state = http("GET", "/api/audio")
        check("reachable with no cast running, unlike the guest route",
              status == 200 and "available" in state, (status, state))
        check("while the guest route still refuses",
              http("GET", "/api/cast/audio")[0] == 403)

        # Not in is_cast_public_path, so credentials decide -- and a guest on the
        # LAN must not be able to turn the room up at three in the morning. The
        # request has to go over TLS: the plain listener is loopback-only.
        import ssl, subprocess
        from test_cast import TLS
        lan = subprocess.run(["python3", "-c",
            "import socket;s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);"
            "s.connect(('10.254.254.254',1));print(s.getsockname()[0])"],
            capture_output=True, text=True).stdout.strip()
        http("PUT", "/api/settings",
             {"auth_enabled": True, "auth_user": "op", "auth_password": "geheim!!"})
        try:
            with urllib.request.urlopen(f"https://{lan}:{TLS}/api/audio", timeout=5,
                                        context=ssl._create_unverified_context()) as res:
                code = res.status
        except urllib.error.HTTPError as e:
            code = e.code
        check("a remote request without credentials is refused", code == 401, code)
        # Loopback is exempt only for the *display* paths, and this is not one:
        # an operator route behaves like /api/settings even on the device itself.
        status, _ = http("GET", "/api/audio")
        check("and loopback is not a way around the credentials either",
              status == 401, status)

        import base64
        token = base64.b64encode(b"op:geheim!!").decode()
        req = urllib.request.Request(f"http://127.0.0.1:{HTTP}/api/audio",
                                     headers={"Authorization": "Basic " + token})
        with urllib.request.urlopen(req, timeout=5) as res:
            check("with the credentials it answers", res.status == 200, res.status)

        http("PUT", "/api/settings", {"auth_enabled": False,
                                      "auth_user": "op", "auth_password": "geheim!!"})

    print("\n[31] the audio panel belongs to the active caster")
    with Server():
        status, body = audio()
        check("refused with no cast running", status == 403, (status, body))

        dr, dw = await ws("display"); await wsclient.recv_json(dr)
        sr, sw = await ws("sender"); await wsclient.recv_json(sr)
        await asyncio.sleep(0.5)

        status, state = audio()
        check("the caster may read it", status == 200, (status, state))
        if status != 200:
            return
        check("reports availability", "available" in state, state)
        print(f"        available={state['available']} "
              f"sinks={len(state.get('sinks', []))} streams={len(state.get('streams', []))}")

        if not state["available"]:
            print("        (no sound server here -- backend correctly reports unavailable)")
        else:
            for sink in state["sinks"]:
                print(f"        sink: {sink['description'][:44]!r} vol={sink['volume']} "
                      f"default={sink['is_default']}")
            for stream in state["streams"][:4]:
                print(f"        stream: {stream['label'][:40]!r} vol={stream['volume']} "
                      f"cast={stream['is_cast']}")

            default = next((s for s in state["sinks"] if s["is_default"]), None)
            if default:
                before = default["volume"]
                target = 42 if before != 42 else 43
                status, after = audio("POST", {"target": "sink", "action": "volume", "value": target})
                now = (next((s for s in after["sinks"] if s["is_default"]), {}).get("volume")
                       if isinstance(after, dict) else after)
                check("setting the output volume takes effect", status == 200 and now == target,
                      (status, before, target, now))
                # put it back, this is someone's actual machine
                audio("POST", {"target": "sink", "action": "volume", "value": before})
                restored = next((s for s in audio()[1]["sinks"] if s["is_default"]), {}).get("volume")
                check("and can be restored", restored == before, (before, restored))

            status, body = audio("POST", {"target": "sink", "action": "select", "name": "does-not-exist"})
            check("an unknown sink is rejected", status == 502, (status, body))

        sw.close()
        await asyncio.sleep(6.5)
        status, body = audio()
        check("access ends with the cast", status == 403, (status, body))
        dw.close()

    # [32] "the room has one speaker pair, whichever screen claims it first"
    # used to live here, racing two screens' senders for the room audio with a
    # bare `status != 409` and a fixed `sleep(6.5)` margin. Both are weaker
    # than `test_castscreens.py`'s [84], which covers the identical claim /
    # 409-naming / teardown-release sequence: it reads `/api/audio`'s
    # `available` flag once up front so it can assert the actual expected
    # status either way instead of a check that cannot fail regardless of the
    # backend (`status != 409` is also true of an unrelated 502), and it polls
    # for the release instead of sleeping past the grace period. Removed
    # rather than fixed in place, to avoid keeping two copies of the same
    # scenario that could drift.

asyncio.run(main())
print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
sys.exit(1 if failures else 0)
