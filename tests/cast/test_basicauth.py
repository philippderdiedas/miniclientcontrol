"""The first account set from the admin panel, and the CLI escape hatch.

Credentials used to be one user and password in the settings; they are accounts
now. What this suite guards stays the same: an operator can protect a device
that starts open, the rules on a password hold, protection survives a restart, a
changed password stops the old one at once, and the command line always gets
back in.
"""
import base64, json, os, sys, urllib.error, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_cast import Server, http, check, failures, HTTP


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


_opener = urllib.request.build_opener(_NoRedirect)


def status_of(path, user=None, password=None):
    """The status without following a redirect; a page that sends an
    unauthenticated browser to the login page reads as "login"."""
    req = urllib.request.Request(f"http://127.0.0.1:{HTTP}{path}")
    if user:
        token = base64.b64encode(f"{user}:{password}".encode()).decode()
        req.add_header("Authorization", "Basic " + token)
    try:
        with _opener.open(req, timeout=5) as res:
            return res.status
    except urllib.error.HTTPError as e:
        if e.code in (302, 303) and (e.headers.get("Location") or "").startswith("/login.html"):
            return "login"
        return e.code


def as_user(method, path, user, password, body=None):
    data = json.dumps(body).encode() if body is not None else None
    headers = {"Authorization": "Basic " + base64.b64encode(f"{user}:{password}".encode()).decode()}
    if data:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(f"http://127.0.0.1:{HTTP}{path}", data=data, method=method,
                                 headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=5) as res:
            raw = res.read().decode()
            return res.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, raw


def main():
    print("\n[22] the first account can be created from the admin panel")
    with Server():
        check("admin is open before anything is set", status_of("/admin.html") == 200)

        status, body = http("POST", "/api/users", {"name": "ops", "password": "hunter2!!", "role": "editor"})
        check("the first account must be an admin",
              status == 400 and "Admin" in (body or {}).get("error", ""), (status, body))

        status, body = http("POST", "/api/users", {"name": "ops", "password": "short", "role": "admin"})
        check("a too-short password is refused", status == 400, (status, body))

        status, body = http("POST", "/api/users", {"name": "ops", "password": "hunter2!!", "role": "admin"})
        check("the account is created", status == 201, (status, body))
        check("the password is never echoed back", "hunter2" not in json.dumps(body), body)

        check("admin now demands credentials", status_of("/admin.html") in (401, "login"))
        check("the new credentials work", status_of("/admin.html", "ops", "hunter2!!") == 200)
        check("a wrong password is refused", status_of("/api/me", "ops", "nope1234") == 401)
        check("a wrong user is refused", status_of("/api/me", "other", "hunter2!!") == 401)
        check("the guest page stays open", status_of("/") == 200)

    print("\n[23] the account survives a restart")
    with Server(fresh=False):
        check("still protected", status_of("/api/playlist") == 401)
        check("still the same login", status_of("/admin.html", "ops", "hunter2!!") == 200)

        # A changed password must stop the cached header at once.
        status, _ = as_user("PUT", "/api/me/password", "ops", "hunter2!!",
                            {"current": "hunter2!!", "new": "newpass123"})
        check("password change accepted", status == 200, status)
        check("the old password stops working", status_of("/api/me", "ops", "hunter2!!") == 401)
        check("the new one works", status_of("/api/me", "ops", "newpass123") == 200)

    print("\n[24] the command line is the way back in")
    # An operator who forgets their password can always get back by passing
    # credentials on the command line -- a built-in admin, never stored.
    with Server(fresh=False, basic_auth_user="rescue", basic_auth_password="letmein99"):
        check("the command-line credentials work",
              status_of("/admin.html", "rescue", "letmein99") == 200)
        status, body = as_user("GET", "/api/settings", "rescue", "letmein99")
        check("basic auth is reported as locked", body["locks"]["basic_auth"] is True, body.get("locks"))
        users = as_user("GET", "/api/users", "rescue", "letmein99")[1]
        ops = next(u for u in users if u["name"] == "ops")
        status, _ = as_user("PUT", f"/api/users/{ops['id']}", "rescue", "letmein99",
                            {"password": "recovered1"})
        check("and can reset the forgotten password", status == 200, status)
        check("which then works", status_of("/api/me", "ops", "recovered1") == 200)
        check("the rescue credential is never an account", all(u["name"] != "rescue" for u in users), users)

    print("\n[25] the last admin cannot be removed")
    with Server(fresh=False):
        users = as_user("GET", "/api/users", "ops", "recovered1")[1]
        ops = next(u for u in users if u["name"] == "ops")
        status, body = as_user("DELETE", f"/api/users/{ops['id']}", "ops", "recovered1")
        check("deleting the only admin is refused", status == 400, (status, body))
        check("and the device stays protected", status_of("/api/playlist") == 401)


main()
print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
sys.exit(1 if failures else 0)
