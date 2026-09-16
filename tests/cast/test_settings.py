"""Runtime settings: validation, persistence, enforcement, and CLI precedence."""
import asyncio, os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wsclient
from test_cast import Server, http, ws, claim, check, failures, is_cast_display

def settings():
    return http("GET", "/api/settings")[1]

def put(body):
    return http("PUT", "/api/settings", body)

async def main():
    print("\n[14] cast settings are readable, writable and enforced")
    with Server():
        st = settings()
        check("defaults: enabled, open access, no code",
              st["cast_enabled"] and st["cast_auth"] == "none" and st["cast_code"] == "", st)
        check("nothing is locked without flags",
              not any(st["locks"].values()), st["locks"])
        check("the public endpoint never exposes the code",
              "code" not in http("GET", "/api/cast/info")[1], http("GET", "/api/cast/info")[1])

        status, body = put({"cast_auth": "code"})
        check("code mode without a code is rejected", status == 400 and "error" in body, (status, body))

        status, body = put({"cast_auth": "code", "cast_code": "AB"})
        check("a short code is rejected", status == 400, (status, body))

        status, body = put({"cast_auth": "code", "cast_code": "1234"})
        check("a numeric PIN is accepted", status == 200 and body["cast_code"] == "1234", (status, body))
        check("info now advertises code mode", http("GET", "/api/cast/info")[1]["auth"] == "code")

        status, body = claim("9999")
        check("the new code is enforced", status == 403, (status, body))

        sr, sw = await ws("sender", "1234")
        frame = await wsclient.recv_json(sr)
        check("the new code lets a sender in", frame["type"] == "welcome", frame)
        sw.close()

    print("\n[15] stored settings survive a restart")
    with Server(fresh=False):
        st = settings()
        check("code mode and PIN were persisted",
              st["cast_auth"] == "code" and st["cast_code"] == "1234", st)

    print("\n[15b] the command line wins over what is stored")
    with Server(fresh=False, cast_auth="none"):
        st = settings()
        check("the flag overrides the stored mode", st["cast_auth"] == "none", st)
        check("and the control is reported as locked", st["locks"]["cast_auth"] is True, st["locks"])
        status, body = put({"cast_auth": "pairing"})
        check("changing a pinned setting is refused with the flag named",
              status == 409 and "--cast-auth" in body.get("error", ""), (status, body))

    with Server(fresh=False, cast_code="ZZ99"):
        st = settings()
        check("a pinned code overrides the stored one", st["cast_code"] == "ZZ99", st)
        status, body = claim("1234")
        check("the stored code no longer works", status == 403, (status, body))
        status, body = claim("ZZ99")
        check("the pinned code does", status == 200, (status, body))

    with Server(fresh=False):
        st = settings()
        check("removing the flag restores the stored value",
              st["cast_auth"] == "code" and st["cast_code"] == "1234", st)
        put({"cast_auth": "none", "cast_code": ""})

    print("\n[16] turning casting off cuts a running cast")
    with Server(fresh=False):
        dr, dw = await ws("display"); await wsclient.recv_json(dr)
        sr, sw = await ws("sender"); await wsclient.recv_json(sr)
        await asyncio.sleep(0.4)
        check("cast running", is_cast_display(http("GET", "/api/override")[1]["url"]))

        status, _ = put({"cast_enabled": False})
        check("settings update accepted", status == 200, status)
        await asyncio.sleep(0.5)
        check("the running cast was ended", http("GET", "/api/override")[1]["active"] is False)
        check("info reports casting disabled", http("GET", "/api/cast/info")[1]["enabled"] is False)
        sw.close(); dw.close()

        status, body = claim()
        check("a new sender is turned away while disabled",
              status == 403 and "deaktiviert" in body.get("error", ""), (status, body))

        put({"cast_enabled": True})
        check("re-enabling works", http("GET", "/api/cast/info")[1]["enabled"] is True)

    print("\n[15d] the locale for dates and times")
    with Server():
        st = settings()
        # Not asserted against a fixed tag: the default follows LC_ALL/LC_TIME/LANG
        # of whoever runs the tests, and "no locale chosen" is a valid answer.
        check("the default follows the system, whatever it is",
              st["locale"] == (st["system_locale"] or ""), (st["locale"], st["system_locale"]))
        check("nothing is locked without the flag", st["locks"]["locale"] is False)

        status, body = put({"locale": "de-DE; color: red"})
        check("a tag that could carry a second value is refused", status == 400, (status, body))
        status, body = put({"locale": "sv-SE"})
        check("a plain tag is accepted", status == 200 and body["locale"] == "sv-SE", (status, body))
        check("and reaches the overlay runtime",
              http("GET", "/api/overlay")[1].get("locale") == "sv-SE",
              http("GET", "/api/overlay")[1].get("locale"))
        status, body = put({"locale": ""})
        check("empty is allowed, leaving it to the display browser",
              status == 200 and body["locale"] == "", (status, body))
        put({"locale": "sv-SE"})

    with Server(fresh=False):
        check("the stored tag survives a restart", settings()["locale"] == "sv-SE", settings()["locale"])

    with Server(fresh=False, locale="ja-JP"):
        st = settings()
        check("the flag overrides the stored tag", st["locale"] == "ja-JP", st["locale"])
        check("and locks the control", st["locks"]["locale"] is True, st["locks"])
        status, body = put({"locale": "it-IT"})
        check("changing it is refused, naming the flag",
              status == 409 and "--locale" in body.get("error", ""), (status, body))

    print("\n[16b] --disable-cast pins the switch off")
    with Server(fresh=False, disable_cast=True):
        st = settings()
        check("stored 'enabled' is forced off", st["cast_enabled"] is False, st)
        check("and the switch is locked", st["locks"]["cast_enabled"] is True, st["locks"])
        status, body = put({"cast_enabled": True})
        check("turning it back on is refused",
              status == 409 and "--disable-cast" in body.get("error", ""), (status, body))

    print("\n[16c] guest pages are their own switch")
    with Server():
        st = settings()
        check("off by default", st["guest_pages_enabled"] is False, st["guest_pages_enabled"])
        check("and not locked", st["locks"]["guest_pages"] is False, st["locks"])
        status, _ = put({"guest_pages_enabled": True})
        check("turning it on is accepted", status == 200, status)
        check("and reads back", settings()["guest_pages_enabled"] is True)
        # Independent of casting: turning one off must not touch the other.
        put({"cast_enabled": False})
        check("casting off leaves guest pages on",
              settings()["guest_pages_enabled"] is True, settings())
        put({"cast_enabled": True})

    with Server(fresh=False):
        check("it survived the restart", settings()["guest_pages_enabled"] is True)

    print("\n[16d] --guest-pages pins it")
    with Server(fresh=False, guest_pages="off"):
        st = settings()
        check("the flag forces it off", st["guest_pages_enabled"] is False, st)
        check("and locks the control", st["locks"]["guest_pages"] is True, st["locks"])
        status, body = put({"guest_pages_enabled": True})
        check("turning it on is refused, naming the flag",
              status == 409 and "--guest-pages" in body.get("error", ""), (status, body))
        # Saving the form unchanged must still work, or nothing else on the page
        # could be edited while the flag is set.
        status, _ = put({"guest_pages_enabled": False, "cast_code": "ABCD"})
        check("an unchanged value does not trip the lock", status == 200, status)

    print("\n[16e] the QR-code target")
    with Server():
        st = settings()
        check("default is the guest's own screen", st["cast_qr_target"] == "screen", st)
        check("nothing locks it -- there is no flag for it",
              "cast_qr_target" not in st["locks"], st["locks"])

        status, body = put({"cast_qr_target": "bogus"})
        check("an unknown target is rejected", status == 400 and "error" in body, (status, body))
        check("and the stored value is untouched", settings()["cast_qr_target"] == "screen")

        status, body = put({"cast_qr_target": "chooser"})
        check("chooser is accepted and reads back",
              status == 200 and body["cast_qr_target"] == "chooser", (status, body))

    with Server(fresh=False):
        check("it survived the restart", settings()["cast_qr_target"] == "chooser")
        put({"cast_qr_target": "screen"})

    print("\n[16f] the setting steers which URL a screen's own state carries")
    with Server(display="foyer:9931,werkstatt:9932"):
        put({"cast_qr_target": "screen"})
        status, st = http("GET", "/api/cast/state?screen=foyer")
        check("under 'screen', the state for one display names it",
              status == 200 and "?screen=foyer" in st["sender_url"], st)

        put({"cast_qr_target": "chooser"})
        status, st = http("GET", "/api/cast/state?screen=foyer")
        check("under 'chooser', the same display gets the bare chooser URL",
              status == 200 and "screen=" not in st["sender_url"], st)

asyncio.run(main())
print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
sys.exit(1 if failures else 0)
