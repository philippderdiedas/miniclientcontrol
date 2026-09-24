"""Who may cast: per screen and mode -- anyone, accounts only, or nobody.

Every case is about `werkstatt`, the second declared screen: a case on the
first would pass identically if the screen were never resolved (CLAUDE.md).
"""
import asyncio, base64, json, os, sys, urllib.error, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wsclient
from test_cast import check, failures, http, HTTP
from test_display import Alone
from test_users import Browser, BASE

SCREEN = "werkstatt"


def access_of(name):
    return next(d for d in http("GET", "/api/displays")[1] if d["name"] == name)


def api_flow():
    print("\n[150] every screen starts open to anyone")
    with Alone():
        check("cast is open to anyone", access_of(SCREEN)["cast_access"] == "anyone", access_of(SCREEN))
        check("and so are pages", access_of(SCREEN)["page_access"] == "anyone", access_of(SCREEN))
        status, _ = http("PUT", f"/api/displays/{SCREEN}", {"cast_access": "account", "page_access": "off"})
        check("the operator narrows one screen", status == 200, status)
        check("it is stored", access_of(SCREEN)["cast_access"] == "account"
              and access_of(SCREEN)["page_access"] == "off", access_of(SCREEN))
        check("the other screen is untouched", access_of("foyer")["cast_access"] == "anyone", access_of("foyer"))
        status, _ = http("PUT", f"/api/displays/{SCREEN}", {"cast_access": "members"})
        check("an unknown value is refused", status == 422, status)


def claim_as(browser, mode, origin=BASE):
    status, body, _ = browser.call("POST", "/api/cast/claim", {"display": SCREEN, "mode": mode}, origin=origin)
    return status, body


def release(browser):
    browser.call("DELETE", "/api/cast/claim", {"display": SCREEN})


def basic_claim(mode):
    token = base64.b64encode(b"member:longenough").decode()
    req = urllib.request.Request(f"{BASE}/api/cast/claim", method="POST",
                                 data=json.dumps({"display": SCREEN, "mode": mode}).encode(),
                                 headers={"Content-Type": "application/json", "Authorization": f"Basic {token}"})
    try:
        with urllib.request.urlopen(req, timeout=5) as res:
            return res.status, json.loads(res.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def with_accounts():
    """An admin (first account) and a member, and a signed-in browser for the member."""
    http("POST", "/api/users", {"name": "root", "password": "longenough", "role": "admin"})
    token = base64.b64encode(b"root:longenough").decode()
    req = urllib.request.Request(f"{BASE}/api/users", method="POST",
                                 data=json.dumps({"name": "member", "password": "longenough", "role": "editor"}).encode(),
                                 headers={"Content-Type": "application/json", "Authorization": f"Basic {token}"})
    urllib.request.urlopen(req, timeout=5).read()
    member = Browser()
    member.call("POST", "/api/login", {"name": "member", "password": "longenough"})
    return member, token


def put_access(token, **values):
    req = urllib.request.Request(f"{BASE}/api/displays/{SCREEN}", method="PUT", data=json.dumps(values).encode(),
                                 headers={"Content-Type": "application/json", "Authorization": f"Basic {token}"})
    urllib.request.urlopen(req, timeout=5).read()


def claim_flow():
    print("\n[151] the matrix: mode x access x signed in")
    with Alone(guest_pages="on"):
        member, admin = with_accounts()
        guest = Browser()
        for mode in ("cast", "page"):
            key = f"{mode}_access"
            put_access(admin, **{key: "anyone"})
            status, _ = claim_as(guest, mode)
            check(f"{mode}, anyone: a guest may", status == 200, status)
            release(guest)
            put_access(admin, **{key: "account"})
            status, body = claim_as(guest, mode)
            check(f"{mode}, account: a guest is asked to sign in",
                  status == 401 and body.get("login") == "/login.html?next=/", (status, body))
            status, _ = claim_as(member, mode)
            check(f"{mode}, account: a member may", status == 200, status)
            release(member)
            put_access(admin, **{key: "off"})
            status, body = claim_as(member, mode)
            check(f"{mode}, off: not even a member", status == 403 and "error" in body, (status, body))
            put_access(admin, **{key: "anyone"})

        print("\n[152] a cookie from another site, or a Basic header, is a guest")
        put_access(admin, cast_access="account")
        status, _ = claim_as(member, "cast", origin="http://evil.test")
        check("the member's cookie with a foreign Origin counts for nothing", status == 401, status)
        status, _ = basic_claim("cast")
        check("HTTP Basic on a claim counts for nothing either", status == 401, status)
        put_access(admin, cast_access="anyone")

    print("\n[153] an account still needs the code")
    with Alone(cast_auth="code", cast_code="ABCD"):
        member, admin = with_accounts()
        put_access(admin, cast_access="account")
        status, body = claim_as(member, "cast")
        check("signed in, but no code: refused by the code check", status == 403, (status, body))
        status, body, _ = member.call("POST", "/api/cast/claim",
                                      {"display": SCREEN, "mode": "cast", "code": "ABCD"})
        check("signed in with the code: allowed", status == 200, (status, body))

    print("\n[154] the socket applies the page access to a reused cast ticket")
    with Alone(guest_pages="on"):
        member, admin = with_accounts()
        put_access(admin, page_access="account")
        guest = Browser()
        status, body = claim_as(guest, "cast")  # what the code check does
        check("the guest holds a cast ticket", status == 200, (status, body))

        async def present():
            r, w = await wsclient.connect("127.0.0.1", HTTP,
                                          f"/api/cast/ws?role=sender&ticket={body['ticket']}", secure=False)
            await wsclient.recv_json(r)  # welcome
            await wsclient.send_json(w, {"type": "present", "url": "https://example.org/"})
            frame = await asyncio.wait_for(wsclient.recv_json(r), 5)
            w.close()
            return frame
        frame = asyncio.run(present())
        check("a page on that ticket is refused on the socket",
              frame and frame.get("type") == "error" and frame.get("code") == "page", frame)


def end_flow():
    print("\n[155] switching a screen off ends its session of that mode, and only there")
    with Alone():
        async def run():
            fr, fw = await wsclient.connect("127.0.0.1", HTTP, "/api/cast/ws?role=sender&ticket="
                                            + http("POST", "/api/cast/claim", {"display": "foyer"})[1]["ticket"],
                                            secure=False)
            wr, ww = await wsclient.connect("127.0.0.1", HTTP, "/api/cast/ws?role=sender&ticket="
                                            + http("POST", "/api/cast/claim", {"display": SCREEN})[1]["ticket"],
                                            secure=False)
            await wsclient.recv_json(fr)
            await wsclient.recv_json(wr)
            http("PUT", f"/api/displays/{SCREEN}", {"page_access": "off"})
            await asyncio.sleep(0.5)
            check("switching pages off leaves a cast running",
                  http("GET", f"/api/cast/state?screen={SCREEN}")[1].get("sender") == "127.0.0.1",
                  http("GET", f"/api/cast/state?screen={SCREEN}")[1])
            http("PUT", f"/api/displays/{SCREEN}", {"cast_access": "off"})
            closed = None
            try:
                while True:
                    frame = await asyncio.wait_for(wsclient.recv_json(wr), 5)
                    if frame is None:
                        closed = True
                        break
            except asyncio.TimeoutError:
                closed = False
            check("switching casting off closes that screen's sender", closed is True, closed)
            check("and the other screen's cast is untouched",
                  http("GET", "/api/cast/state?screen=foyer")[1].get("sender") == "127.0.0.1",
                  http("GET", "/api/cast/state?screen=foyer")[1])
            fw.close()
        asyncio.run(run())


def who_flow():
    print("\n[156] the session knows who cast")
    with Alone():
        member, admin = with_accounts()
        status, body = claim_as(member, "cast")
        async def run():
            r, w = await wsclient.connect("127.0.0.1", HTTP, f"/api/cast/ws?role=sender&ticket={body['ticket']}",
                                          secure=False)
            await wsclient.recv_json(r)
            await asyncio.sleep(0.3)
            token = admin
            req = urllib.request.Request(f"{BASE}/api/displays", headers={"Authorization": f"Basic {token}"})
            rows = json.loads(urllib.request.urlopen(req, timeout=5).read())
            row = next(d for d in rows if d["name"] == SCREEN)
            check("the operator list names the member", row.get("cast_user") == "member", row)
            w.close()
        asyncio.run(run())


def info_flow():
    print("\n[157] the guest page learns what it may do, and who it is")
    with Alone(guest_pages="on"):
        member, admin = with_accounts()
        put_access(admin, cast_access="account", page_access="off")
        info = http("GET", "/api/cast/info")[1]
        row = next((s for s in info.get("screens", []) if s["name"] == SCREEN), None)
        check("the screen is listed with its access",
              row and row["cast_access"] == "account" and row["page_access"] == "off", row)
        check("a guest is nobody", info.get("account") is None, info.get("account"))
        _, info, _ = member.call("GET", "/api/cast/info", origin=None)
        check("the member is named, even without an Origin on a GET", info.get("account") == "member", info)
        put_access(admin, cast_access="off")
        info = http("GET", "/api/cast/info")[1]
        check("off for both modes, the screen is not listed",
              all(s["name"] != SCREEN for s in info.get("screens", [])), info.get("screens"))


if __name__ == "__main__":
    api_flow()
    claim_flow()
    end_flow()
    who_flow()
    info_flow()
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
