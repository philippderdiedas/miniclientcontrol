"""A guest putting a web page on the display instead of casting.

The point of the feature is that showing a page costs the device almost nothing
while a stream costs it a great deal, so the two switches are separate and both
directions are checked here.

Two of these look like formalities and are not: that connecting the socket alone
pins nothing (the server activates the display as soon as a sender arrives, so a
page mode that got this wrong would flash the cast page first), and that a
refused address leaves the guest holding their slot.
"""
import asyncio, json, os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wsclient
from test_cast import Server, http, check, failures, claim, ws

MENU = "http://example.test/menu"


def put_settings(body):
    """`cast_enabled` is a runtime setting, not a Server flag."""
    return http("PUT", "/api/settings", body)


def override():
    return http("GET", "/api/override")[1]


def state():
    return http("GET", "/api/cast/state")[1]


async def present(writer, url, scroll="none"):
    await wsclient.send_json(writer, {"type": "present", "url": url, "scroll": scroll})


async def open_page_socket():
    """Claim in page mode, connect, and swallow the welcome frame."""
    reader, writer = await ws("sender", mode="page")
    await wsclient.recv_json(reader)
    return reader, writer


def until(predicate, timeout=6.0):
    """Poll: the override is written by a task, not by the request we just made."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if predicate():
                return True
        except Exception:
            pass
        time.sleep(0.15)
    return False


async def main():
    print("\n[41] the setting gates it, and refuses at claim time")
    with Server(guest_pages="off"):
        status, body = claim(mode="page")
        check("a page claim is refused when the setting is off",
              status == 403 and "Webseite" in body.get("error", ""), (status, body))
        # Independent switches: refusing one must not refuse the other.
        check("casting is unaffected", claim()[0] == 200)

    print("\n[42] a guest pins a page")
    with Server(guest_pages="on"):
        reader, writer = await open_page_socket()
        # The server pins the display the moment a sender connects. For a page
        # that must not happen yet, or the display bounces through the cast page.
        check("the socket alone pins nothing", override().get("active") is not True,
              override())

        await present(writer, MENU)
        check("present pins the override to the page",
              until(lambda: override().get("url") == MENU), override())
        reply = await wsclient.recv_json(reader)
        check("and the guest is told what is on screen",
              reply.get("type") == "presenting" and reply.get("url") == MENU, reply)
        check("the operator sees the page", state()["showing"]["page"] == MENU,
              state()["showing"])
        writer.close()

    print("\n[43] the scroll choice comes through")
    with Server(guest_pages="on"):
        reader, writer = await open_page_socket()
        await present(writer, MENU, scroll="slow")
        check("slow scrolling becomes a continuous override",
              until(lambda: (override().get("scroll_config") or {}).get("type") == "Continuous"),
              override().get("scroll_config"))
        writer.close()

    print("\n[44] a refused address costs the guest nothing")
    with Server(guest_pages="on"):
        reader, writer = await open_page_socket()
        await present(writer, "file:///etc/passwd")
        reply = await wsclient.recv_json(reader)
        check("a file:// address is refused with a reason",
              reply.get("type") == "error" and reply.get("code") == "page", reply)
        check("and nothing was pinned", override().get("active") is not True, override())
        # The slot is still theirs: a typo must not mean claiming again.
        await present(writer, MENU)
        check("a corrected address is accepted on the same socket",
              until(lambda: override().get("url") == MENU), override())
        writer.close()

    print("\n[45] credentials reach the browser and nothing else")
    with Server(guest_pages="on"):
        reader, writer = await open_page_socket()
        await present(writer, "http://admin:hunter2@example.test/wiki")
        check("the page is up", until(lambda: override().get("url") is not None), override())
        check("the operator's view is redacted", "hunter2" not in json.dumps(state()),
              state()["showing"])
        check("the browser still gets them", "hunter2" in (override().get("url") or ""))
        writer.close()

    print("\n[46] the operator can end it")
    with Server(guest_pages="on"):
        reader, writer = await open_page_socket()
        await present(writer, MENU)
        until(lambda: override().get("url") == MENU)
        http("DELETE", "/api/cast/session")
        check("the screen goes back to the playlist",
              until(lambda: override().get("active") is not True), override())
        writer.close()

    print("\n[47] one guest at a time, either way round")
    with Server(guest_pages="on"):
        reader, writer = await open_page_socket()
        await present(writer, MENU)
        until(lambda: override().get("url") == MENU)
        check("a cast cannot start while a page is up", claim()[0] == 409, claim())
        writer.close()

    with Server(guest_pages="on"):
        creader, cwriter = await ws("sender")
        await wsclient.recv_json(creader)
        check("and a page cannot start while a cast is up",
              claim(mode="page")[0] == 409, claim(mode="page"))
        cwriter.close()

    print("\n[48] pages work with casting switched off")
    with Server(guest_pages="on"):
        put_settings({"cast_enabled": False})
        status, body = claim()
        check("a cast claim is refused",
              status == 403 and "deaktiviert" in body.get("error", ""), (status, body))
        reader, writer = await open_page_socket()
        await present(writer, MENU)
        check("a page still works", until(lambda: override().get("url") == MENU), override())
        writer.close()

    print("\n[49] only a sender may present")
    with Server(guest_pages="on"):
        reader, writer = await ws("display")
        await wsclient.recv_json(reader)
        await present(writer, MENU)
        await asyncio.sleep(1.0)
        check("a present from the display role is ignored",
              override().get("active") is not True, override())
        writer.close()

    print("\n[50] the overlay treats a guest page like a cast")
    with Server(guest_pages="on"):
        # No code makes this work: overlay_payload keys on is_active(), which
        # covers both. Tested because it is claimed rather than obvious.
        put_settings({"overlay": {
            "enabled": True, "text": "Haus-Notiz", "hide_during_cast": True,
        }})
        check("the overlay is up with nobody presenting",
              len(http("GET", "/api/overlay")[1]["layers"]) == 1,
              http("GET", "/api/overlay")[1]["layers"])

        reader, writer = await open_page_socket()
        await present(writer, MENU)
        until(lambda: override().get("url") == MENU)
        check("and stands down while a guest page is up",
              http("GET", "/api/overlay")[1]["layers"] == [],
              http("GET", "/api/overlay")[1]["layers"])
        writer.close()

    print("\n[51] a guest who walks away hands the screen back")
    with Server(guest_pages="on"):
        reader, writer = await open_page_socket()
        await present(writer, MENU)
        until(lambda: override().get("url") == MENU)
        writer.close()
        await asyncio.sleep(3)
        # PAGE_GRACE is 30s, deliberately longer than the cast's 5s: the expected
        # case is a phone whose tab was discarded, not a page reload.
        check("still up well past the cast's grace period",
              override().get("url") == MENU, override())
        check("and released after its own",
              until(lambda: override().get("active") is not True, timeout=45), override())


asyncio.run(main())
print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
sys.exit(1 if failures else 0)
