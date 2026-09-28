"""The MCP endpoint and the API tokens behind it.

`/mcp` is a thin JSON-RPC front on the operator API: every tool call is
replayed through the router as the caller, so what a token may do is exactly
what its account may do -- an editor's token proposes, and nothing about the
MCP path widens that. A token never manages its own account's credentials,
not directly and not by going round through a tool call.
"""
import base64, json, os, sys, urllib.error, urllib.request
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_cast import Server, check, failures, HTTP
from test_users import Browser, BASE

# A 1x1 PNG.
PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")


def raw(method, path, body=None, token=None, headers=None):
    data = json.dumps(body).encode() if body is not None else None
    h = dict(headers or {})
    if data is not None:
        h.setdefault("Content-Type", "application/json")
    if token:
        h["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(BASE + path, data=data, method=method, headers=h)
    try:
        with urllib.request.urlopen(req, timeout=10) as res:
            text = res.read().decode()
            return res.status, (json.loads(text) if text else None)
    except urllib.error.HTTPError as e:
        text = e.read().decode()
        try:
            return e.code, json.loads(text)
        except ValueError:
            return e.code, text


_ids = iter(range(1, 10_000))


def rpc(method, params=None, token=None, headers=None):
    status, body = raw("POST", "/mcp", {"jsonrpc": "2.0", "id": next(_ids), "method": method,
                                        "params": params or {}}, token, headers)
    return status, body


def tool(name, arguments, token=None):
    """-> (isError, text, content) of one tool call."""
    status, body = rpc("tools/call", {"name": name, "arguments": arguments}, token)
    assert status == 200, (status, body)
    result = body["result"]
    texts = [c["text"] for c in result["content"] if c["type"] == "text"]
    return result["isError"], "\n".join(texts), result["content"]


def api(method, path, body=None, token=None):
    return tool("api_request", {"method": method, "path": path, **({"body": body} if body is not None else {})}, token)


def main():
    with Server():
        print("\n[194] the transport: initialize, tools, notifications, no stream, Origin")
        status, body = rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                          "clientInfo": {"name": "t", "version": "0"}})
        check("initialize answers", status == 200 and "result" in body, (status, body))
        result = body["result"]
        check("the asked version is echoed", result["protocolVersion"] == "2025-06-18", result)
        check("tools are offered", "tools" in result["capabilities"], result)
        _, body = rpc("initialize", {"protocolVersion": "1999-01-01"})
        check("an unknown version gets ours", body["result"]["protocolVersion"] == "2025-06-18", body)
        status, _ = raw("POST", "/mcp", {"jsonrpc": "2.0", "method": "notifications/initialized"})
        check("a notification is accepted with no body", status == 202, status)
        _, body = rpc("tools/list")
        names = sorted(t["name"] for t in body["result"]["tools"])
        check("the four tools", names == ["api_reference", "api_request", "screenshot", "upload_asset"], names)
        _, body = rpc("nope/nope")
        check("an unknown method is a JSON-RPC error", body.get("error", {}).get("code") == -32601, body)
        status, _ = raw("GET", "/mcp")
        check("no server stream: GET is 405", status == 405, status)
        status, _ = rpc("ping", headers={"Origin": "https://evil.example"})
        check("a foreign Origin is refused", status == 403, status)
        status, _ = rpc("ping", headers={"Origin": BASE})
        check("this host's Origin passes", status == 200, status)
        error, text, _ = tool("api_reference", {})
        check("the reference is the README's API section",
              not error and text.startswith("## API Overview") and "/api/displays/{name}/schedule" in text,
              text[:80])

        print("\n[195] open mode: api_request runs, the guest and sign-in paths do not")
        error, text, _ = api("GET", "/api/me")
        check("GET /api/me through the tool", not error and '"open": true' in text, text)
        for path in ["/api/cast/claim", "/api/login", "/admin.html", "/api/../admin.html"]:
            error, text, _ = api("POST" if "claim" in path or "login" in path else "GET", path)
            check(f"{path} is refused by the tool", error, text)

        # Accounts: an admin, an editor, a manager.
        check("admin created", raw("POST", "/api/users", {"name": "root", "password": "longenough", "role": "admin"})[0] == 201)
        admin = Browser()
        admin.call("POST", "/api/login", {"name": "root", "password": "longenough"})
        for name, role in [("ed", "editor"), ("boss", "manager")]:
            status, _, _ = admin.call("POST", "/api/users", {"name": name, "password": "longenough", "role": role})
            check(f"{role} created", status == 201, status)

        print("\n[196] tokens: minted by a signed-in account, shown once, used as Bearer")
        ed = Browser()
        ed.call("POST", "/api/login", {"name": "ed", "password": "longenough"})
        status, created, _ = ed.call("POST", "/api/me/tokens", {"name": "claude", "days": 30})
        check("the editor mints a token", status == 201 and created["token"].startswith("mcc_"), (status, created))
        ed_token = created["token"]
        status, listed, _ = ed.call("GET", "/api/me/tokens")
        check("the list shows it without the secret",
              status == 200 and len(listed) == 1 and ed_token not in json.dumps(listed), listed)
        status, me = raw("GET", "/api/me", token=ed_token)
        check("Bearer resolves to the editor", status == 200 and me["name"] == "ed" and me["role"] == "editor", me)
        status, _ = raw("GET", "/api/me", token="mcc_" + "0" * 64)
        check("an unknown token is 401", status == 401, status)
        status, _ = raw("POST", "/api/me/tokens", {"name": "more"}, token=ed_token)
        check("a token cannot mint a token", status == 403, status)
        status, _ = raw("PUT", "/api/me/password", {"current": "longenough", "new": "whatever12"}, token=ed_token)
        check("a token cannot change the password", status == 403, status)
        status, _ = raw("POST", "/mcp", {"jsonrpc": "2.0", "id": 1, "method": "ping"})
        check("/mcp needs a credential once accounts exist", status == 401, status)

        print("\n[197] an editor's token through MCP proposes, and is refused what an editor is")
        _, body = rpc("initialize", {"protocolVersion": "2025-06-18"}, ed_token)
        check("the instructions name the account and role",
              "editor" in body["result"]["instructions"] and "'ed'" in body["result"]["instructions"], body)
        error, text, _ = api("POST", "/api/playlists", {"name": "Von Claude"}, ed_token)
        check("a content write becomes a proposal (202)", not error and text.startswith("HTTP 202") and "proposed" in text, text)
        error, text, _ = api("GET", "/api/playlists", token=ed_token)
        check("... and is not applied", not error and "Von Claude" not in text, text)
        error, text, _ = api("PUT", "/api/settings", {"locale": "en"}, ed_token)
        check("an admin write is refused", error and text.startswith("HTTP 403"), text)
        error, text, _ = api("GET", "/api/users", token=ed_token)
        check("an admin read is refused", error and text.startswith("HTTP 403"), text)
        error, text, _ = api("POST", "/api/me/tokens", {"name": "sneaky"}, ed_token)
        check("a token cannot mint one by going round through a tool", error and text.startswith("HTTP 403"), text)
        error, text, _ = api("PUT", "/api/me/password", {"current": "longenough", "new": "whatever12"}, ed_token)
        check("nor change the password that way", error and text.startswith("HTTP 403"), text)
        error, text, _ = api("POST", "/api/changesets/draft/submit", {}, ed_token)
        check("the editor submits the draft", not error, text)

        print("\n[198] a manager's token writes directly and uploads")
        boss = Browser()
        boss.call("POST", "/api/login", {"name": "boss", "password": "longenough"})
        _, created, _ = boss.call("POST", "/api/me/tokens", {"name": "claude"})
        boss_token = created["token"]
        error, text, _ = api("POST", "/api/playlists", {"name": "Direkt"}, boss_token)
        check("the write applies", not error and text.startswith("HTTP 2"), text)
        _, text, _ = api("GET", "/api/playlists", token=boss_token)
        check("... and is there", "Direkt" in text, text)
        error, text, _ = tool("upload_asset", {"filename": "dot.png",
                                               "content_base64": base64.b64encode(PNG).decode()}, boss_token)
        check("upload_asset stores the file", not error, text)
        _, text, _ = api("GET", "/api/assets", token=boss_token)
        check("... and it is listed", "dot.png" in text, text)
        error, text, _ = tool("upload_asset", {"filename": "x.png", "content_base64": "%%%"}, boss_token)
        check("bad base64 is a tool error", error, text)
        error, text, _ = tool("screenshot", {"display": "../users"}, boss_token)
        check("screenshot refuses a name that is not one", error, text)
        error, text, _ = tool("screenshot", {"display": "default"}, boss_token)
        check("screenshot without a browser is an error, not a crash", error and "503" in text, text)

        print("\n[199] revoking, disabling and expiring end a token at once")
        status, listed, _ = ed.call("GET", "/api/me/tokens")
        status, _, _ = boss.call("DELETE", f"/api/me/tokens/{listed[0]['id']}")
        check("another account cannot revoke it", status == 404, status)
        status, _, _ = ed.call("DELETE", f"/api/me/tokens/{listed[0]['id']}")
        check("its owner can", status == 200, status)
        check("the revoked token is 401", raw("GET", "/api/me", token=ed_token)[0] == 401)
        status, users, _ = admin.call("GET", "/api/users")
        boss_id = next(u["id"] for u in users if u["name"] == "boss")
        admin.call("PUT", f"/api/users/{boss_id}", {"disabled": True})
        check("a disabled account's token is 401", raw("GET", "/api/me", token=boss_token)[0] == 401)
        admin.call("PUT", f"/api/users/{boss_id}", {"disabled": False})
        check("and works again when it is enabled", raw("GET", "/api/me", token=boss_token)[0] == 200)


main()
print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
sys.exit(1 if failures else 0)
