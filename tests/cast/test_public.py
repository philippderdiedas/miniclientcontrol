"""The address guests are given, and the QR code for it."""
import os, sys, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_cast import Server, http, check, failures, HTTP, TLS

def sender_url():
    return http("GET", "/api/cast/info")[1]["sender_url"]

def main():
    print("\n[29] --public-url decides what guests are told")
    with Server():
        url = sender_url()
        check("default is the LAN address on the TLS port",
              url.startswith("https://") and url.endswith(f":{TLS}/"), url)

    with Server(public_url="signage.example.com"):
        check("a bare host becomes https://host:port/",
              sender_url() == f"https://signage.example.com:{TLS}/", sender_url())

    with Server(public_url="https://display.example.com"):
        # A full URL means something else owns the port -- a reverse proxy on 443.
        check("a full URL is taken at its word, without our port",
              sender_url() == "https://display.example.com/", sender_url())

    with Server(public_url="mdns"):
        url = sender_url()
        check("mdns uses the .local name", ".local:" in url, url)

    print("\n[30] the QR code")
    with Server(public_url="signage.example.com"):
        req = urllib.request.Request(f"http://127.0.0.1:{HTTP}/api/cast/qr.svg")
        with urllib.request.urlopen(req, timeout=5) as res:
            body = res.read().decode()
            check("served as SVG", res.headers.get("Content-Type") == "image/svg+xml",
                  res.headers.get("Content-Type"))
        check("looks like a real QR svg",
              body.startswith("<?xml") and "<svg" in body and len(body) > 1000, len(body))
        check("reachable without credentials", True)

    with Server(basic_auth_user="admin", basic_auth_password="s3cret"):
        req = urllib.request.Request(f"http://127.0.0.1:{HTTP}/api/cast/qr.svg")
        try:
            with urllib.request.urlopen(req, timeout=5) as res:
                check("still public when basic auth is on", res.status == 200, res.status)
        except urllib.error.HTTPError as e:
            check("still public when basic auth is on", False, e.code)

main()
print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
sys.exit(1 if failures else 0)
