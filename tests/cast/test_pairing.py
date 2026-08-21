"""Pairing mode and the per-address lockout."""
import asyncio, json, os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wsclient
from test_cast import Server, http, ws, claim, check, failures, HTTP

async def main():
    print("\n[7] pairing mode: the code only exists on the display")
    with Server(cast_auth="pairing"):
        check("info reports auth pairing", http("GET", "/api/cast/info")[1]["auth"] == "pairing")

        # a sender with no pairing request is refused
        status, body = claim("ABCD")
        check("sender without a pairing request is refused",
              status == 403 and "Pairing" in body.get("error", ""), (status, body))

        dr, dw = await ws("display")
        await wsclient.recv_json(dr)

        status, body = http("POST", "/api/cast/pair")
        check("pair returns a TTL but never the code",
              status == 200 and "expires_in" in body and "code" not in body, body)

        pushed = await wsclient.recv_json(dr)
        check("display is pushed the code", pushed["type"] == "pairing" and len(pushed["code"]) == 4, pushed)
        code = pushed["code"]

        # The operator's view carries it too, and only theirs: somebody helping a
        # guest by phone is otherwise the one person who cannot see the code.
        state = http("GET", "/api/cast/state")[1]
        check("the operator state shows the live code and its remaining time",
              (state.get("pairing") or {}).get("code") == code
              and 0 < state["pairing"]["expires_in"] <= 30, state.get("pairing"))
        check("and the public info still does not",
              "pairing" not in http("GET", "/api/cast/info")[1],
              http("GET", "/api/cast/info")[1])

        await asyncio.sleep(0.4)
        check("pairing pins the display so the code is visible",
              http("GET", "/api/override")[1]["url"].endswith("cast_display.html"))

        status, body = claim("ZZZZ")
        check("wrong pairing code refused", status == 403, (status, body))

        sr, sw = await ws("sender", code)
        frame = await wsclient.recv_json(sr)
        check("correct pairing code accepted", frame["type"] == "welcome", frame)

        # the code is single-use: a second guest cannot reuse what they saw
        sw.close()
        await asyncio.sleep(6.0)
        status, body = claim(code)
        check("a consumed pairing code cannot be reused", status == 403, (status, body))
        dw.close()

    print("\n[8] repeated wrong codes lock the address out")
    with Server(cast_auth="code", cast_code="QT7X"):
        for i in range(5):
            claim("BAD" + str(i))
        status, body = claim("QT7X")
        check("the correct code is refused while locked out",
              status == 403 and "warten" in body.get("error", "").lower(), (status, body))

    print("\n[9] a pairing code nobody uses releases the display")
    with Server(cast_auth="pairing"):
        dr, dw = await ws("display"); await wsclient.recv_json(dr)
        http("POST", "/api/cast/pair")
        await asyncio.sleep(0.4)
        check("display pinned while the code is up",
              http("GET", "/api/override")[1]["active"] is True)
        await asyncio.sleep(31)
        check("display released after the code expired",
              http("GET", "/api/override")[1]["active"] is False)
        dw.close()

asyncio.run(main())
print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
sys.exit(1 if failures else 0)
