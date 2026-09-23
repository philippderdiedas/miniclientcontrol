"""Accounts, roles and proposals, over plain HTTP.

Open until the first account exists; then a session for a browser and HTTP
Basic for a script, each with its account's role; and an editor's content
writes kept as a bundle until a manager applies it.
"""
import base64, json, os, sys, urllib.error, urllib.request
# Not `import http.cookiejar`: `http` is test_cast's request helper below, and
# it would shadow the module (the trap test_display.py describes).
from http.cookiejar import CookieJar
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_cast import Server, check, failures, http, HTTP

BASE = f"http://127.0.0.1:{HTTP}"


class Browser:
    """A cookie jar and an Origin header -- what a signed-in page sends."""
    def __init__(self):
        self.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(CookieJar()),
            urllib.request.HTTPRedirectHandler())
    def call(self, method, path, body=None, origin=BASE, follow=True):
        data = json.dumps(body).encode() if body is not None else None
        headers = {"Content-Type": "application/json"} if data else {}
        if origin:
            headers["Origin"] = origin
        req = urllib.request.Request(BASE + path, data=data, method=method, headers=headers)
        try:
            with self.opener.open(req, timeout=5) as res:
                raw = res.read().decode()
                return res.status, (json.loads(raw) if raw and raw[0] in "{[" else raw), res.geturl()
        except urllib.error.HTTPError as e:
            raw = e.read().decode()
            try:
                return e.code, json.loads(raw), None
            except ValueError:
                return e.code, raw, None


def basic(method, path, user, password, body=None):
    token = base64.b64encode(f"{user}:{password}".encode()).decode()
    data = json.dumps(body).encode() if body is not None else None
    headers = {"Authorization": f"Basic {token}"}
    if data:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(BASE + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=5) as res:
            raw = res.read().decode()
            return res.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, None


def accounts_flow():
    print("\n[120] open until the first account exists")
    with Server():
        status, me = http("GET", "/api/me")
        check("with no account the operator API is open", status == 200 and me.get("open") is True, me)
        status, body = http("POST", "/api/users", {"name": "ed", "password": "longenough", "role": "editor"})
        check("the first account must be an admin", status == 400, (status, body))
        status, _ = http("POST", "/api/users", {"name": "root", "password": "longenough", "role": "admin"})
        check("an admin account is created", status in (200, 201), status)
        status, _ = http("GET", "/api/playlist")
        check("after that, no credentials is a 401", status == 401, status)
        b = Browser()
        status, body, url = b.call("GET", "/admin.html")
        check("an HTML page redirects to the login page",
              status == 200 and url and "/login.html" in url, (status, url))

        print("\n[121] signing in, the session, signing out")
        status, body, _ = b.call("POST", "/api/login", {"name": "root", "password": "wrong-one"})
        check("a wrong password is refused", status == 401, status)
        status, body, _ = b.call("POST", "/api/login", {"name": "root", "password": "longenough"})
        check("the right one signs in", status == 200 and body.get("role") == "admin", body)
        status, me, _ = b.call("GET", "/api/me")
        check("the session carries the account", me.get("name") == "root" and me.get("open") is False, me)
        status, _, _ = b.call("PUT", "/api/settings", {"locale": "de-DE"}, origin="http://evil.test")
        check("a cookie write from another origin is refused", status == 403, status)
        status, _, _ = b.call("PUT", "/api/settings", {"locale": "de-DE"})
        check("from its own origin it goes through", status == 200, status)
        b.call("POST", "/api/logout")
        status, _, _ = b.call("GET", "/api/me")
        check("after signing out the session is gone", status == 401, status)

        print("\n[122] a script signs in with HTTP Basic")
        status, me = basic("GET", "/api/me", "root", "longenough")
        check("Basic auth works against the account", status == 200 and me["role"] == "admin", (status, me))
        status, _ = basic("GET", "/api/me", "root", "nope-nope")
        check("with a wrong password it does not", status == 401, status)

        print("\n[123] each role reaches what it may")
        for name, role in (("mgr", "manager"), ("ed", "editor")):
            status, _ = basic("POST", "/api/users", "root", "longenough",
                              {"name": name, "password": "longenough", "role": role})
            check(f"the admin creates a {role}", status in (200, 201), status)
        check("a manager may not change settings",
              basic("PUT", "/api/settings", "mgr", "longenough", {"locale": "de-DE"})[0] == 403, None)
        check("a manager may pin an override",
              basic("DELETE", "/api/override", "mgr", "longenough")[0] in (200, 204), None)
        check("an editor may not pin an override",
              basic("DELETE", "/api/override", "ed", "longenough")[0] == 403, None)
        check("an editor may read the playlist",
              basic("GET", "/api/playlist", "ed", "longenough")[0] == 200, None)
        check("an editor may not read webhook targets",
              basic("GET", "/api/webhooks", "ed", "longenough")[0] == 403, None)

        print("\n[124] disabling an account, and the last admin")
        users = basic("GET", "/api/users", "root", "longenough")[1]
        ed_id = next(u["id"] for u in users if u["name"] == "ed")
        root_id = next(u["id"] for u in users if u["name"] == "root")
        basic("PUT", f"/api/users/{ed_id}", "root", "longenough", {"disabled": True})
        check("a disabled account cannot sign in",
              basic("GET", "/api/me", "ed", "longenough")[0] == 401, None)
        check("the last admin cannot be deleted",
              basic("DELETE", f"/api/users/{root_id}", "root", "longenough")[0] == 400, None)

        print("\n[127] an editor's writes wait in a draft")
        basic("PUT", f"/api/users/{ed_id}", "root", "longenough", {"disabled": False})
        pl = basic("POST", "/api/playlists", "mgr", "longenough", {"name": "Foyer"})[1]["id"]
        basic("POST", "/api/playlist", "mgr", "longenough",
              {"url": "https://a.test/", "advance": {"on": "time", "seconds": 10}, "playlist_id": pl})
        item = basic("GET", f"/api/playlist?playlist_id={pl}", "mgr", "longenough")[1][0]["id"]
        status, body = basic("PUT", f"/api/playlist/{item}", "ed", "longenough", {"advance": {"on": "time", "seconds": 42}})
        check("an editor's edit is accepted as a proposal", status == 202 and body.get("proposed"), (status, body))
        live = basic("GET", f"/api/playlist?playlist_id={pl}", "mgr", "longenough")[1][0]
        check("and is not live", live["advance"]["seconds"] == 10, live)
        status, body = basic("POST", "/api/playlists", "ed", "longenough", {"name": "Neu"})
        new_pl = body.get("placeholder")
        check("a create gets a placeholder", status == 202 and new_pl and new_pl.startswith("new:"), body)
        status, body = basic("POST", "/api/playlist", "ed", "longenough",
                             {"url": "https://b.test/", "advance": {"on": "time", "seconds": 5}, "playlist_id": new_pl})
        check("which a later request may use", status == 202, (status, body))
        draft = basic("GET", "/api/changesets/draft", "ed", "longenough")[1]
        check("the draft holds all three, in order", len(draft["requests"]) == 3, draft)
        check("nobody else sees a playlist named Neu yet",
              all(p["name"] != "Neu" for p in basic("GET", "/api/playlists", "mgr", "longenough")[1]), None)
        status, _ = basic("POST", "/api/changesets/draft/submit", "ed", "longenough")
        check("the draft is submitted", status == 200, status)

        print("\n[128] a manager applies the bundle, and all of it is live at once")
        bundle = next(c for c in basic("GET", "/api/changesets?state=submitted", "mgr", "longenough")[1])
        check("the reviewer sees before and after", all("before" in r for r in bundle["requests"]), bundle)
        status, body = basic("POST", f"/api/changesets/{bundle['id']}/approve", "mgr", "longenough")
        check("it applies", status == 200 and body.get("state") == "applied", (status, body))
        live = basic("GET", f"/api/playlist?playlist_id={pl}", "mgr", "longenough")[1][0]
        check("the edit is live", live["advance"]["seconds"] == 42, live)
        made = next((p for p in basic("GET", "/api/playlists", "mgr", "longenough")[1] if p["name"] == "Neu"), None)
        check("the new playlist exists", made is not None, None)
        items = basic("GET", f"/api/playlist?playlist_id={made['id']}", "mgr", "longenough")[1]
        check("and holds the item that named it by placeholder",
              len(items) == 1 and items[0]["url"] == "https://b.test/", items)

        print("\n[129] a stale bundle is not applied")
        basic("PUT", f"/api/playlist/{item}", "ed", "longenough", {"advance": {"on": "time", "seconds": 7}})
        basic("POST", "/api/changesets/draft/submit", "ed", "longenough")
        basic("PUT", f"/api/playlist/{item}", "mgr", "longenough", {"advance": {"on": "time", "seconds": 99}})
        stale = next(c for c in basic("GET", "/api/changesets?state=submitted", "mgr", "longenough")[1])
        status, body = basic("POST", f"/api/changesets/{stale['id']}/approve", "mgr", "longenough")
        check("approving a bundle whose object changed is a 409", status == 409, (status, body))
        live = basic("GET", f"/api/playlist?playlist_id={pl}", "mgr", "longenough")[1][0]
        check("and the manager's change stands", live["advance"]["seconds"] == 99, live)
        status, _ = basic("POST", f"/api/changesets/{stale['id']}/reject", "mgr", "longenough", {"note": "veraltet"})
        check("it can still be rejected", status == 200, status)

        print("\n[130] an editor's upload stays hidden until it is applied, and goes with a reject")
        def upload_as(user, name, data=b"\x89PNG\r\n\x1a\n", mimetype="image/png"):
            import uuid
            boundary = "----mcc" + uuid.uuid4().hex
            body = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n"
                    f"Content-Type: {mimetype}\r\n\r\n").encode() + data + f"\r\n--{boundary}--\r\n".encode()
            token = base64.b64encode(f"{user}:longenough".encode()).decode()
            req = urllib.request.Request(BASE + "/api/assets", data=body, method="POST", headers={
                "Content-Type": f"multipart/form-data; boundary={boundary}", "Authorization": f"Basic {token}"})
            with urllib.request.urlopen(req, timeout=10) as res:
                return res.status
        check("the editor's upload is accepted", upload_as("ed", "vorschlag.png") == 200, None)
        mine = [a for a in basic("GET", "/api/assets", "ed", "longenough")[1] if a["filename"] == "vorschlag.png"]
        theirs = [a for a in basic("GET", "/api/assets", "mgr", "longenough")[1] if a["filename"] == "vorschlag.png"]
        check("its author sees it", len(mine) == 1, mine)
        check("nobody else does", theirs == [], theirs)
        stored = os.path.join(os.path.dirname(os.path.abspath(__file__)), "assets", mine[0]["local_path"])
        check("the file is on disk", os.path.exists(stored), stored)
        basic("POST", "/api/changesets/draft/submit", "ed", "longenough")
        pending = next(c for c in basic("GET", "/api/changesets?state=submitted", "mgr", "longenough")[1])
        check("the bundle shows the upload", pending["requests"][0]["method"] == "UPLOAD", pending)
        shown = pending.get("refs", {}).get("assets", {}).get(str(mine[0]["id"]), {})
        check("and hands the reviewer what a preview needs, hidden from the asset list or not",
              shown.get("filename") == "vorschlag.png" and shown.get("local_path") == mine[0]["local_path"]
              and shown.get("mimetype") == "image/png", pending.get("refs"))
        basic("POST", f"/api/changesets/{pending['id']}/reject", "mgr", "longenough", {"note": "nein"})
        check("rejecting deletes the file", not os.path.exists(stored), stored)
        check("and the asset",
              all(a["filename"] != "vorschlag.png" for a in basic("GET", "/api/assets", "ed", "longenough")[1]), None)

        print("\n[131] a replay that fails stops the bundle and says how far it got")
        basic("PUT", f"/api/playlist/{item}", "ed", "longenough", {"advance": {"on": "time", "seconds": 11}})
        # A second object -- two changes to one object merge into one request --
        # and valid JSON the handler refuses only when it runs: an empty name.
        basic("PUT", f"/api/playlists/{pl}", "ed", "longenough", {"name": ""})
        basic("POST", "/api/changesets/draft/submit", "ed", "longenough")
        failing = next(c for c in basic("GET", "/api/changesets?state=submitted", "mgr", "longenough")[1])
        status, body = basic("POST", f"/api/changesets/{failing['id']}/approve", "mgr", "longenough")
        check("the bundle stops as failed", status == 409 and body.get("state") == "failed", (status, body))
        after = next(c for c in basic("GET", "/api/changesets?state=failed", "mgr", "longenough")[1])
        applied = [r["applied"] for r in after["requests"]]
        check("the first request is marked applied, the second not", applied == [True, False], after)
        live = basic("GET", f"/api/playlist?playlist_id={pl}", "mgr", "longenough")[1][0]
        check("and what was applied is live", live["advance"]["seconds"] == 11, live)

        print("\n[132] an editor sees what became of their proposals, with the manager's note")
        status, mine = basic("GET", "/api/changesets/mine", "ed", "longenough")
        check("an editor may list their own bundles", status == 200, (status, mine))
        states = {b["state"] for b in mine["bundles"]}
        check("decided bundles are there, the draft is not",
              {"applied", "rejected", "failed"} <= states and "draft" not in states, states)
        rejected = next(b for b in mine["bundles"] if b["state"] == "rejected" and b["note"] == "veraltet")
        check("a rejection carries the manager's note", rejected["note"] == "veraltet", rejected)
        check("decisions not yet looked at are counted", mine["unseen"] >= 3, mine["unseen"])
        basic("POST", "/api/changesets/mine/seen", "ed", "longenough")
        check("once looked at, nothing is unseen",
              basic("GET", "/api/changesets/mine", "ed", "longenough")[1]["unseen"] == 0, None)

        basic("PUT", f"/api/playlist/{item}", "ed", "longenough", {"advance": {"on": "time", "seconds": 12}})
        basic("POST", "/api/changesets/draft/submit", "ed", "longenough")
        latest = next(c for c in basic("GET", "/api/changesets?state=submitted", "mgr", "longenough")[1])
        status, _ = basic("POST", f"/api/changesets/{latest['id']}/approve", "mgr", "longenough",
                          {"note": "Danke, übernommen"})
        check("an approval may carry a note too", status == 200, status)
        mine = basic("GET", "/api/changesets/mine", "ed", "longenough")[1]
        approved = next(b for b in mine["bundles"] if b["id"] == latest["id"])
        check("and the editor reads it", approved["state"] == "applied"
              and approved["note"] == "Danke, übernommen", approved)
        check("as a new decision", mine["unseen"] == 1, mine["unseen"])
        others = basic("GET", "/api/changesets/mine", "mgr", "longenough")[1]["bundles"]
        check("a bundle is only its author's to list here", others == [], others)

        print("\n[132a] a bundle names what it touches, not just ids")
        basic("POST", "/api/playlist", "ed", "longenough",
              {"url": "https://named.test/", "advance": {"on": "time", "seconds": 5}, "playlist_id": pl})
        draft = basic("GET", "/api/changesets/draft", "ed", "longenough")[1]
        refs = draft.get("refs", {})
        names = {p["id"]: p["name"] for p in basic("GET", "/api/playlists", "mgr", "longenough")[1]}
        check("the playlist a new item goes into is named",
              refs.get("playlists", {}).get(str(pl)) == names[pl], refs)
        basic("PUT", f"/api/playlist/{item}", "ed", "longenough", {"advance": {"on": "time", "seconds": 7}})
        refs = basic("GET", "/api/changesets/draft", "ed", "longenough")[1].get("refs", {})
        check("an edited item comes with what it shows",
              refs.get("items", {}).get(str(item), {}).get("playlist_id") == pl, refs)
        basic("DELETE", "/api/changesets/draft", "ed", "longenough")

        print("\n[133] two changes to one object become one line in the draft")
        basic("DELETE", "/api/changesets/draft", "ed", "longenough")
        basic("PUT", f"/api/playlist/{item}", "ed", "longenough", {"advance": {"on": "time", "seconds": 21}})
        basic("PUT", f"/api/playlist/{item}", "ed", "longenough", {"fit_mode": "cover"})
        reqs = basic("GET", "/api/changesets/draft", "ed", "longenough")[1]["requests"]
        check("one request for the item", len(reqs) == 1, reqs)
        check("carrying both changes", reqs[0]["body"] == {"advance": {"on": "time", "seconds": 21}, "fit_mode": "cover"}, reqs)
        basic("DELETE", f"/api/playlist/{item}", "ed", "longenough")
        reqs = basic("GET", "/api/changesets/draft", "ed", "longenough")[1]["requests"]
        check("deleting it afterwards replaces the edit",
              len(reqs) == 1 and reqs[0]["method"] == "DELETE", reqs)
        placeholder = basic("POST", "/api/playlists", "ed", "longenough", {"name": "Entwurf"})[1]["placeholder"]
        basic("PUT", f"/api/playlists/{placeholder}", "ed", "longenough", {"name": "Umbenannt"})
        reqs = basic("GET", "/api/changesets/draft", "ed", "longenough")[1]["requests"]
        create = [r for r in reqs if r["placeholder"] == placeholder]
        check("an edit to a proposed playlist lands in its create",
              len(reqs) == 2 and create and create[0]["body"]["name"] == "Umbenannt", reqs)
        basic("DELETE", "/api/changesets/draft", "ed", "longenough")

        print("\n[134] editing an object of one's own open bundle brings the bundle back")
        basic("PUT", f"/api/playlist/{item}", "ed", "longenough", {"advance": {"on": "time", "seconds": 31}})
        basic("POST", "/api/changesets/draft/submit", "ed", "longenough")
        open_ids = [c["id"] for c in basic("GET", "/api/changesets?state=submitted", "mgr", "longenough")[1]]
        status, body = basic("PUT", f"/api/playlist/{item}", "ed", "longenough", {"advance": {"on": "time", "seconds": 32}})
        check("the answer says the bundle came back", status == 202 and body.get("reopened") in open_ids, body)
        check("it is off the approvals list",
              basic("GET", "/api/changesets?state=submitted", "mgr", "longenough")[1] == [], None)
        reqs = basic("GET", "/api/changesets/draft", "ed", "longenough")[1]["requests"]
        check("and in the draft as one merged change",
              len(reqs) == 1 and reqs[0]["body"] == {"advance": {"on": "time", "seconds": 32}}, reqs)

        print("\n[135] withdrawing, and hiding what is decided")
        basic("POST", "/api/changesets/draft/submit", "ed", "longenough")
        sub = basic("GET", "/api/changesets?state=submitted", "mgr", "longenough")[1][0]["id"]
        status, _ = basic("POST", f"/api/changesets/{sub}/withdraw", "ed", "longenough")
        check("an editor withdraws their open bundle", status == 200, status)
        check("which is back in the draft",
              len(basic("GET", "/api/changesets/draft", "ed", "longenough")[1]["requests"]) == 1, None)
        check("another account cannot withdraw it",
              basic("POST", f"/api/changesets/{sub}/withdraw", "mgr", "longenough")[0] in (403, 404, 409), None)
        basic("DELETE", "/api/changesets/draft", "ed", "longenough")
        mine = basic("GET", "/api/changesets/mine", "ed", "longenough")[1]["bundles"]
        decided = [b for b in mine if b["state"] in ("applied", "rejected", "failed", "stale")]
        status, _ = basic("POST", f"/api/changesets/{decided[0]['id']}/hide", "ed", "longenough")
        check("a decided bundle can be hidden", status == 200, status)
        left = basic("GET", "/api/changesets/mine", "ed", "longenough")[1]["bundles"]
        check("and is gone from the list", all(b["id"] != decided[0]["id"] for b in left), left)
        basic("POST", "/api/changesets/mine/hide-decided", "ed", "longenough")
        left = basic("GET", "/api/changesets/mine", "ed", "longenough")[1]["bundles"]
        check("hiding all decided leaves none", all(b["state"] in ("submitted",) for b in left), left)
        check("while a manager still has the history",
              len(basic("GET", "/api/changesets?state=applied", "mgr", "longenough")[1]) >= 1, None)

        print("\n[125] login attempts are limited per address")
        results = [Browser().call("POST", "/api/login", {"name": "root", "password": "x" * 9})[0]
                   for _ in range(12)]
        check("after ten failures the address gets 429", results[-1] == 429, results)

    print("\n[126] the command line credential is the way back in")
    with Server(fresh=False, basic_auth_user="rescue", basic_auth_password="rescue-me-now"):
        status, me = basic("GET", "/api/me", "rescue", "rescue-me-now")
        check("the CLI credential is an admin", status == 200 and me["role"] == "admin", (status, me))
        check("and is never stored as an account",
              all(u["name"] != "rescue" for u in basic("GET", "/api/users", "rescue", "rescue-me-now")[1]),
              None)


if __name__ == "__main__":
    accounts_flow()
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
