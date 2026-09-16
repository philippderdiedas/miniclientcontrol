"""An override the operator sets during a cast is newer, and must survive it."""
import asyncio, os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wsclient
from test_cast import Server, http, ws, check, failures, is_cast_display

async def main():
    print("\n[12] operator override set during a cast is not clobbered")
    with Server():
        http("POST", "/api/override", {"url": "http://example.invalid/before"})
        dr, dw = await ws("display"); await wsclient.recv_json(dr)
        sr, sw = await ws("sender"); await wsclient.recv_json(sr)
        await asyncio.sleep(0.4)
        check("cast holds the display", is_cast_display(http("GET", "/api/override")[1]["url"]))

        # operator overrules the cast mid-session
        http("POST", "/api/override", {"url": "http://example.invalid/operator"})
        http("DELETE", "/api/displays/default/cast/session")
        await asyncio.sleep(0.5)
        current = http("GET", "/api/override")[1]
        check("the operator's newer choice wins over the pre-cast one",
              current["url"] == "http://example.invalid/operator", current)
        sw.close(); dw.close()

    print("\n[13] a cast that starts with no override leaves none behind")
    with Server():
        dr, dw = await ws("display"); await wsclient.recv_json(dr)
        sr, sw = await ws("sender"); await wsclient.recv_json(sr)
        await asyncio.sleep(0.4)
        http("DELETE", "/api/displays/default/cast/session")
        await asyncio.sleep(0.4)
        check("override fully cleared", http("GET", "/api/override")[1]["active"] is False)
        sw.close(); dw.close()

asyncio.run(main())
print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
sys.exit(1 if failures else 0)
