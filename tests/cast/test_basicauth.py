"""Operator credentials set from the admin panel, and the CLI escape hatch."""
import base64, os, sys, urllib.error, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_cast import Server, http, check, failures, HTTP

def status_of(path, user=None, password=None):
    req = urllib.request.Request(f"http://127.0.0.1:{HTTP}{path}")
    if user:
        token = base64.b64encode(f"{user}:{password}".encode()).decode()
        req.add_header("Authorization", "Basic " + token)
    try:
        with urllib.request.urlopen(req, timeout=5) as res:
            return res.status
    except urllib.error.HTTPError as e:
        return e.code

def put(body):
    return http("PUT", "/api/settings", body)

def main():
    print("\n[22] credentials can be switched on from the admin panel")
    with Server():
        check("admin is open before anything is set", status_of("/admin.html") == 200)

        status, body = put({"auth_enabled": True, "auth_user": "ops"})
        check("enabling without ever setting a password is refused",
              status == 400 and "Passwort" in body.get("error", ""), (status, body))

        status, body = put({"auth_enabled": True, "auth_user": "ops", "auth_password": "short"})
        check("a too-short password is refused", status == 400, (status, body))

        status, body = put({"auth_enabled": True, "auth_user": "ops", "auth_password": "hunter2!!"})
        check("credentials accepted", status == 200 and body["auth_enabled"], (status, body))
        check("the password is never echoed back", "password" not in str(body).lower(), body)

        check("admin now demands credentials", status_of("/admin.html") == 401)
        check("the new credentials work", status_of("/admin.html", "ops", "hunter2!!") == 200)
        check("a wrong password is refused", status_of("/admin.html", "ops", "nope1234") == 401)
        check("a wrong user is refused", status_of("/admin.html", "other", "hunter2!!") == 401)
        check("the guest page stays open", status_of("/") == 200)

    print("\n[23] credentials survive a restart")
    with Server(fresh=False):
        check("still protected", status_of("/admin.html") == 401)
        check("still the same login", status_of("/admin.html", "ops", "hunter2!!") == 200)

        # changing the password must invalidate the cached header immediately
        req = urllib.request.Request(f"http://127.0.0.1:{HTTP}/api/settings", method="PUT",
                                     data=b'{"auth_password":"newpass123"}',
                                     headers={"Content-Type": "application/json",
                                              "Authorization": "Basic " + base64.b64encode(
                                                  b"ops:hunter2!!").decode()})
        with urllib.request.urlopen(req, timeout=5) as res:
            check("password change accepted", res.status == 200, res.status)
        check("the old password stops working", status_of("/admin.html", "ops", "hunter2!!") == 401)
        check("the new one works", status_of("/admin.html", "ops", "newpass123") == 200)

    print("\n[24] the command line is the way back in")
    # This is the point of CLI-wins: an operator who forgets the password they
    # set in the UI can always get back by passing credentials on the command line.
    with Server(fresh=False, basic_auth_user="rescue", basic_auth_password="letmein99"):
        check("the forgotten UI password no longer applies",
              status_of("/admin.html", "ops", "newpass123") == 401)
        check("the command-line credentials work",
              status_of("/admin.html", "rescue", "letmein99") == 200)

        req = urllib.request.Request(f"http://127.0.0.1:{HTTP}/api/settings")
        req.add_header("Authorization", "Basic " + base64.b64encode(b"rescue:letmein99").decode())
        with urllib.request.urlopen(req, timeout=5) as res:
            body = __import__("json").load(res)
        check("basic auth is reported as locked", body["locks"]["basic_auth"] is True, body["locks"])

        status, problem = http("PUT", "/api/settings", {"auth_enabled": False})
        check("and cannot be changed from the UI",
              status in (401, 409), (status, problem))

    print("\n[25] auth can be turned off again")
    with Server(fresh=False):
        req = urllib.request.Request(f"http://127.0.0.1:{HTTP}/api/settings", method="PUT",
                                     data=b'{"auth_enabled":false}',
                                     headers={"Content-Type": "application/json",
                                              "Authorization": "Basic " + base64.b64encode(
                                                  b"ops:newpass123").decode()})
        with urllib.request.urlopen(req, timeout=5) as res:
            check("disable accepted", res.status == 200, res.status)
        check("admin is open again", status_of("/admin.html") == 200)

main()
print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
sys.exit(1 if failures else 0)
