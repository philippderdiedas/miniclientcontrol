"""The display tells the sender how large a frame it can actually put on screen.

The reason this exists: the kiosk on a Raspberry Pi 3 has a Broadcom VC4 GPU with
`MAX_TEXTURE_SIZE` 2048. A 2880-wide screen share decoded fine there and still
showed a black rectangle, because a frame wider than the texture limit never
reaches the compositor. The display is the only side that knows its own hardware,
so it announces the limit and the sender constrains its capture to it.

The server only relays the number -- it does not decide the policy.
"""
import asyncio, os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wsclient
from test_cast import Server, http, ws, check, failures


async def until(reader, want, tries=12):
    """Next frame of type `want`, skipping the chatter in between."""
    for _ in range(tries):
        try:
            frame = await asyncio.wait_for(wsclient.recv_json(reader), 3)
        except asyncio.TimeoutError:
            return None
        if frame is None:
            return None
        if frame.get("type") == want:
            return frame
    return None


async def main():
    print("\n[30] the display announces the largest frame it can show")
    with Server():
        dr, dw = await ws("display")
        await until(dr, "welcome")
        await wsclient.send_json(dw, {"type": "limits", "max_edge": 2048})
        await asyncio.sleep(0.4)

        status, info = http("GET", "/api/cast/info")
        check("the public info carries the limit, so a sender can constrain its capture",
              (info.get("display_limits") or {}).get("max_edge") == 2048, info)

        print("\n[31] a sender joining later is told in its welcome")
        sr, sw = await ws("sender")
        welcome = await until(sr, "welcome")
        check("welcome carries the display's limit",
              (welcome.get("display_limits") or {}).get("max_edge") == 2048, welcome)

        print("\n[32] a limit that changes mid-session is pushed to the sender")
        await wsclient.send_json(dw, {"type": "limits", "max_edge": 1600})
        pushed = await until(sr, "display_limits")
        check("the sender gets a display_limits frame",
              pushed and pushed.get("max_edge") == 1600, pushed)

        print("\n[33] nonsense is not relayed")
        await wsclient.send_json(dw, {"type": "limits", "max_edge": 12})
        await asyncio.sleep(0.4)
        status, info = http("GET", "/api/cast/info")
        check("an absurdly small limit is ignored rather than shrinking the cast to nothing",
              (info.get("display_limits") or {}).get("max_edge") == 1600, info)

        await wsclient.send_json(sw, {"type": "limits", "max_edge": 99999})
        await asyncio.sleep(0.4)
        status, info = http("GET", "/api/cast/info")
        check("a sender cannot announce limits on the display's behalf",
              (info.get("display_limits") or {}).get("max_edge") == 1600, info)

        sw.close()
        dw.close()
        await asyncio.sleep(0.3)

    print("\n[34] the last known limit survives the session")
    with Server(fresh=True):
        status, info = http("GET", "/api/cast/info")
        check("a controller that has not seen a display reports no limit",
              info.get("display_limits") is None, info)

    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)


asyncio.run(main())
