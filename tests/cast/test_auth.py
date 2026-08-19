"""Basic auth must not lock guests out of the cast page, nor let them into admin.

This is the trap the route reshuffle introduced: `/` used to be the operator
landing page and is now the public sender page.
"""
import os, sys, urllib.error, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_cast import Server, check, failures, HTTP, TLS

def status_of(path, user=None, password=None):
    req = urllib.request.Request(f"http://127.0.0.1:{HTTP}{path}")
    if user:
        import base64
        token = base64.b64encode(f"{user}:{password}".encode()).decode()
        req.add_header("Authorization", "Basic " + token)
    try:
        with urllib.request.urlopen(req, timeout=5) as res:
            return res.status
    except urllib.error.HTTPError as e:
        return e.code

def main():
    print("\n[17] basic auth: public cast paths vs. protected operator paths")
    with Server(basic_auth_user="admin", basic_auth_password="s3cret"):
        for path in ["/", "/index.html", "/cast.js", "/api/cast/info", "/api/cast/claim"]:
            code = status_of(path)
            check(f"guest reaches {path} without credentials", code != 401, code)

        for path in ["/admin.html", "/playlist.html", "/assets.html",
                     "/api/playlist", "/api/settings"]:
            code = status_of(path)
            check(f"{path} demands credentials", code == 401, code)

        for path in ["/admin.html", "/playlist.html", "/api/playlist"]:
            code = status_of(path, "admin", "s3cret")
            check(f"{path} opens with credentials", code == 200, code)

        check("wrong credentials are refused",
              status_of("/admin.html", "admin", "nope") == 401)

        # the old sender URL should still land somewhere useful
        check("/cast.html redirects to the new root",
              status_of("/cast.html") in (200, 301, 308), status_of("/cast.html"))

    print("\n[17b] plain HTTP stays on loopback; the network only sees TLS")
    with Server():
        import json as _json, socket, ssl, urllib.request
        lan = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        lan.connect(("10.254.254.254", 1))
        lan_ip = lan.getsockname()[0]
        lan.close()

        reachable = True
        try:
            with socket.create_connection((lan_ip, HTTP), timeout=3):
                pass
        except OSError:
            reachable = False
        check("plain HTTP is not exposed on the LAN address", not reachable, lan_ip)

        ctx = ssl._create_unverified_context()
        with urllib.request.urlopen(f"https://{lan_ip}:{TLS}/api/cast/info",
                                    timeout=5, context=ctx) as res:
            check("but HTTPS answers there", res.status == 200, res.status)

        # loopback HTTP still works -- that is what the display browser uses
        check("loopback HTTP still serves the display pages",
              status_of("/empty_playlist.html") == 200)

main()
print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
sys.exit(1 if failures else 0)
