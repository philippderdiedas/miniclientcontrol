"""Venue audio control, and that only the connected caster may use it."""
import asyncio, json, os, sys, urllib.error, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wsclient
from test_cast import Server, http, ws, claim, check, failures, HTTP

def audio(method="GET", body=None):
    return http(method, "/api/cast/audio", body)

async def main():
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

asyncio.run(main())
print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
sys.exit(1 if failures else 0)
