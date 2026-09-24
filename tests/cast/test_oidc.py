"""Single sign-on with OpenID Connect, against a fake provider.

The provider below is stdlib-only, so it signs ID tokens with HS256 (the client
secret). RS256/ES256 are covered in Rust against fixed fixtures (src/oidc/jwt.rs).
"""
import base64, hashlib, hmac, json, os, sys, threading, time, urllib.error, urllib.parse, urllib.request
from http.cookiejar import CookieJar
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_cast import Server, check, failures, http, HTTP, TLS
import ssl
from test_users import Browser, BASE, basic

IDP_PORT = 3071
ISSUER = f"http://127.0.0.1:{IDP_PORT}"
SECRET = "s3cret-for-tests"


def b64url(raw):
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


class Provider:
    """Discovery, an authorize endpoint that signs whoever `next_user` says,
    and a token endpoint. `tamper` lets a case break one claim."""
    def __init__(self):
        self.next_user = {"sub": "u1", "preferred_username": "anna", "groups": ["staff"]}
        self.tamper = {}
        self.codes = {}
        provider = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def reply(self, status, body, headers=()):
                data = json.dumps(body).encode() if not isinstance(body, bytes) else body
                self.send_response(status)
                for k, v in headers:
                    self.send_header(k, v)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                url = urllib.parse.urlparse(self.path)
                q = dict(urllib.parse.parse_qsl(url.query))
                if url.path == "/.well-known/openid-configuration":
                    self.reply(200, {"issuer": ISSUER, "authorization_endpoint": f"{ISSUER}/authorize",
                                     "token_endpoint": f"{ISSUER}/token", "jwks_uri": f"{ISSUER}/jwks",
                                     "id_token_signing_alg_values_supported": ["HS256"]})
                elif url.path == "/jwks":
                    self.reply(200, {"keys": []})
                elif url.path == "/authorize":
                    code = b64url(os.urandom(12))
                    provider.codes[code] = {"nonce": q.get("nonce"), "challenge": q.get("code_challenge"),
                                            "client_id": q.get("client_id")}
                    back = f"{q['redirect_uri']}?code={code}&state={urllib.parse.quote(q['state'])}"
                    self.reply(302, b"", [("Location", back)])
                else:
                    self.reply(404, {})

            def do_POST(self):
                form = dict(urllib.parse.parse_qsl(self.rfile.read(int(self.headers["Content-Length"])).decode()))
                grant = provider.codes.pop(form.get("code"), None)
                verifier = form.get("code_verifier", "")
                challenge = b64url(hashlib.sha256(verifier.encode()).digest())
                if not grant or grant["challenge"] != challenge:
                    self.reply(400, {"error": "invalid_grant"})
                    return
                claims = {"iss": ISSUER, "aud": grant["client_id"], "exp": int(time.time()) + 300,
                          "nonce": grant["nonce"], **provider.next_user, **provider.tamper}
                head = b64url(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
                body = b64url(json.dumps(claims).encode())
                sig = b64url(hmac.new(SECRET.encode(), f"{head}.{body}".encode(), hashlib.sha256).digest())
                self.reply(200, {"access_token": "x", "token_type": "Bearer", "id_token": f"{head}.{body}.{sig}"})

        ThreadingHTTPServer.allow_reuse_address = True
        self.server = ThreadingHTTPServer(("127.0.0.1", IDP_PORT), Handler)

    def __enter__(self):
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return self

    def __exit__(self, *a):
        self.server.shutdown()
        self.server.server_close()


TLSBASE = f"https://127.0.0.1:{TLS}"
# The provider calls back on the device's canonical HTTPS address; the harness
# makes that 127.0.0.1 with --public-url, and its certificate is self-signed.
INSECURE = ssl.create_default_context()
INSECURE.check_hostname = False
INSECURE.verify_mode = ssl.CERT_NONE


class SBrowser:
    """A browser on the canonical HTTPS address: cookie jar, redirects, and an
    Origin of that address -- what a signed-in page there sends."""
    def __init__(self):
        self.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(CookieJar()),
            urllib.request.HTTPSHandler(context=INSECURE))
    def call(self, method, path, body=None, origin=TLSBASE):
        data = json.dumps(body).encode() if body is not None else None
        headers = {"Content-Type": "application/json"} if data else {}
        if origin:
            headers["Origin"] = origin
        req = urllib.request.Request(TLSBASE + path, data=data, method=method, headers=headers)
        try:
            with self.opener.open(req, timeout=10) as res:
                raw = res.read().decode()
                return res.status, (json.loads(raw) if raw and raw[0] in "{[" else raw), res.geturl()
        except urllib.error.HTTPError as e:
            raw = e.read().decode()
            try:
                return e.code, json.loads(raw), None
            except ValueError:
                return e.code, raw, None


def admin_put(path, body):
    return basic("PUT", path, "root", "longenough", body)


def configure(**extra):
    body = {"issuer": ISSUER, "client_id": "mcc", "client_secret": SECRET, "label": "Fablab",
            "mapping": [{"group": "staff", "target": "editor"}, {"group": "boss", "target": "admin"},
                        {"group": "members", "target": "cast"}]}
    body.update(extra)
    return admin_put("/api/oidc/config", body)


def config_flow():
    print("\n[160] configuring the provider")
    with Server():
        http("POST", "/api/users", {"name": "root", "password": "longenough", "role": "admin"})
        status, info = http("GET", "/api/oidc/info")
        check("unconfigured, the login page is told so", status == 200 and info["configured"] is False, info)
        status, _ = configure()
        check("the admin configures it", status == 200, status)
        status, conf = basic("GET", "/api/oidc/config", "root", "longenough")
        check("the secret never comes back", "client_secret" not in conf and conf["secret_set"] is True, conf)
        check("the redirect URI is shown", conf["redirect_uri"].endswith("/api/oidc/callback"), conf)
        check("and flagged as unstable on a bare LAN address", conf["redirect_stable"] is False, conf)
        status, info = http("GET", "/api/oidc/info")
        check("the login page learns the label", info == {"configured": True, "label": "Fablab",
                                                          "local_passwords": True}, info)
        status, _ = configure(client_secret=None)
        status, conf = basic("GET", "/api/oidc/config", "root", "longenough")
        check("leaving the secret out keeps it", conf["secret_set"] is True, conf)
        status, _ = configure(local_passwords=False)
        check("passwords cannot be switched off with no SSO admin yet", status == 400, status)
        status, _ = configure(issuer="http://192.168.1.9/")
        check("a plain-http provider on the network is refused", status == 400, status)


def signin(browser, next_path="/admin.html"):
    """Walk the flow the way a browser does: start, provider, callback. Returns
    (status of the callback page, its text, the URL it sends the browser on to)."""
    opener = browser.opener
    req = urllib.request.Request(f"{BASE}/api/oidc/start?next={urllib.parse.quote(next_path)}")
    try:
        with opener.open(req, timeout=10) as res:  # follows start -> provider -> callback
            page = res.read().decode()
            status = res.status
    except urllib.error.HTTPError as e:
        page, status = e.read().decode(), e.code
    target = None
    marker = 'url='
    if marker in page:
        target = page.split(marker, 1)[1].split('"', 1)[0]
    return status, page, target


def flow_flow():
    print("\n[161] a mapped group signs in as an account with that role")
    with Server(public_url="127.0.0.1"), Provider() as idp:
        http("POST", "/api/users", {"name": "root", "password": "longenough", "role": "admin"})
        configure()
        b = SBrowser()
        status, page, target = signin(b)
        check("the callback answers with a page, not a redirect", status == 200, (status, page[:200]))
        check("which sends the browser on to next", target == "/admin.html", target)
        status, me, _ = b.call("GET", "/api/me")
        check("the session is the provider's user, as an editor",
              status == 200 and me["name"] == "anna" and me["role"] == "editor", me)
        users = basic("GET", "/api/users", "root", "longenough")[1]
        check("an account was created", any(u["name"] == "anna" for u in users), users)

        print("\n[162] the role follows the groups at every sign-in")
        idp.next_user = {"sub": "u1", "preferred_username": "anna", "groups": ["staff", "boss"]}
        b2 = SBrowser(); signin(b2)
        check("now an admin", b2.call("GET", "/api/me")[1]["role"] == "admin", None)
        idp.next_user = {"sub": "u1", "preferred_username": "anna", "groups": []}
        b3 = SBrowser(); status, page, _ = signin(b3)
        check("with no group left the sign-in is refused", "keine passende Gruppe" in page, page[:300])
        check("the account is disabled", next(u for u in basic("GET", "/api/users", "root", "longenough")[1]
                                             if u["name"] == "anna")["disabled"] is True, None)
        check("and its earlier session ended", b2.call("GET", "/api/me")[0] == 401, None)

        print("\n[163] a provider name never takes over a local account")
        idp.next_user = {"sub": "u9", "preferred_username": "root", "groups": ["staff"]}
        b4 = SBrowser(); signin(b4)
        me = b4.call("GET", "/api/me")[1]
        check("the SSO account is suffixed", me["name"] == "root-sso" and me["role"] == "editor", me)
        check("the local root is untouched", basic("GET", "/api/me", "root", "longenough")[1]["role"] == "admin", None)

        print("\n[164] a cast-only sign-in creates no account")
        idp.next_user = {"sub": "u5", "preferred_username": "mia", "groups": ["members"]}
        b5 = SBrowser(); status, page, target = signin(b5, "/")
        check("it succeeds and goes back to the guest page", status == 200 and target == "/", (status, target))
        check("no account", all(u["name"] != "mia" for u in basic("GET", "/api/users", "root", "longenough")[1]), None)
        status, _, url = b5.call("GET", "/admin.html")
        check("the admin page still asks to sign in", url and "/login.html" in url, url)
        check("and the API is a 401", b5.call("GET", "/api/playlist")[0] == 401, None)

        print("\n[165] everyone may cast, or nobody without a group")
        idp.next_user = {"sub": "u6", "preferred_username": "tom", "groups": []}
        b6 = SBrowser(); status, page, _ = signin(b6, "/")
        check("no group, not everyone: refused", "keine passende Gruppe" in page, page[:300])
        configure(everyone_casts=True)
        b7 = SBrowser(); status, page, target = signin(b7, "/")
        check("everyone may cast: a cast session", status == 200 and target == "/", (status, target))

        print("\n[166] tampered tokens and states are refused")
        for claim, value in (("aud", "someone-else"), ("iss", "http://evil.test"), ("exp", 1000), ("nonce", "x")):
            idp.tamper = {claim: value}
            bx = SBrowser(); status, page, _ = signin(bx)
            check(f"a wrong {claim} is refused", bx.call("GET", "/api/me")[0] == 401, (claim, page[:200]))
        idp.tamper = {}
        status, page, _ = SBrowser().call("GET", "/api/oidc/callback?code=nope&state=made-up", origin=None)
        check("an unknown state is refused", "abgelaufen" in str(page), page)

        print("\n[166b] a callback belongs to the browser that started it, once")
        class Stay(urllib.request.HTTPRedirectHandler):
            def redirect_request(self, *a, **k):
                return None
        started = SBrowser()
        jar = CookieJar()
        # build_opener replaces the default redirect handler with a subclass of it.
        started.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(jar), urllib.request.HTTPSHandler(context=INSECURE), Stay())
        try:
            started.opener.open(urllib.request.Request(f"{TLSBASE}/api/oidc/start?next=/admin.html"), timeout=10)
        except urllib.error.HTTPError as e:
            to_provider = e.headers["Location"]
        try:
            urllib.request.build_opener(Stay()).open(to_provider, timeout=10)
        except urllib.error.HTTPError as e:
            back = e.headers["Location"]
        stranger = SBrowser()
        _, page, _ = stranger.call("GET", back[len(TLSBASE):], origin=None)
        check("another browser cannot finish it", "anderen Browser" in str(page)
              and stranger.call("GET", "/api/me")[0] == 401, str(page)[:200])
        started.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(jar), urllib.request.HTTPSHandler(context=INSECURE))
        _, page, _ = started.call("GET", back[len(TLSBASE):], origin=None)
        check("and the state is spent, even for the right browser",
              "abgelaufen" in str(page) and started.call("GET", "/api/me")[0] == 401, str(page)[:200])

        print("\n[167] next survives, a foreign next is dropped")
        idp.next_user = {"sub": "u1", "preferred_username": "anna", "groups": ["staff"]}
        _, _, target = signin(SBrowser(), "//evil.test/x")
        check("a foreign next becomes the admin page", target == "/admin.html", target)


def cast_flow():
    print("\n[168] a cast session counts on a screen kept for signed-in people")
    from test_display import Alone
    with Alone(public_url="127.0.0.1"), Provider() as idp:
        http("POST", "/api/users", {"name": "root", "password": "longenough", "role": "admin"})
        configure()
        admin_put("/api/displays/werkstatt", {"cast_access": "account"})
        idp.next_user = {"sub": "u5", "preferred_username": "mia", "groups": ["members"]}
        b = SBrowser(); signin(b, "/")
        status, body, _ = b.call("POST", "/api/cast/claim", {"display": "werkstatt"})
        check("the member with a cast session may claim", status == 200, (status, body))
        _, info, _ = b.call("GET", "/api/cast/info", origin=None)
        check("the guest page names them", info.get("account") == "mia", info)
        status, _, _ = SBrowser().call("POST", "/api/cast/claim", {"display": "werkstatt"})
        check("a guest still may not", status == 401, status)


def local_flow():
    print("\n[169] local passwords off")
    with Server(public_url="127.0.0.1", basic_auth_user="rescue", basic_auth_password="rescuepass1"), Provider() as idp:
        # With a command-line credential the device is never open; it creates root.
        basic("POST", "/api/users", "rescue", "rescuepass1", {"name": "root", "password": "longenough", "role": "admin"})
        configure()
        idp.next_user = {"sub": "u2", "preferred_username": "chef", "groups": ["boss"]}
        signin(SBrowser())
        status, _ = configure(local_passwords=False)
        check("with an SSO admin, passwords can be switched off", status == 200, status)
        status, _, _ = SBrowser().call("POST", "/api/login", {"name": "root", "password": "longenough"})
        check("the login form refuses a local account", status == 403, status)
        check("so does Basic", basic("GET", "/api/me", "root", "longenough")[0] == 401, None)
        check("the command-line credential still works",
              basic("GET", "/api/me", "rescue", "rescuepass1")[0] == 200, None)
        info = http("GET", "/api/oidc/info")[1]
        check("the login page is told", info["local_passwords"] is False, info)


if __name__ == "__main__":
    config_flow()
    flow_flow()
    cast_flow()
    local_flow()
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
