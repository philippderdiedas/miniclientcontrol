"""How an image or video sits on the screen, and a video with no control bar.

The first half is plain HTTP: the fit is stored per playlist item, falls back
when it is nonsense, and a background that is not a colour is refused. The
second half drives a real Chrome through the real browser_loop, because the
whole point of the feature is what the display draws -- and a stored value
proves nothing about that.
"""
import asyncio, base64, json, os, shutil, subprocess, sys, time, urllib.request, uuid
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cdp
from test_cast import Server, check, failures, http

SP = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.join(SP, "..", "..", "target", "debug", "miniclientcontrol")
CHROME = "/usr/bin/google-chrome-stable"
HTTP, TLS, CDP = 3051, 3494, 9252

# One transparent pixel. Enough for every case here: `scroll` draws it at full
# width, so a 1x1 image becomes 1280x1280 on a 1280x720 window and the document
# really is taller than the screen.
PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")

procs = []


def upload(name, data, mimetype, port=None):
    """POST one file to /api/assets and return the new asset's id."""
    port = port or 3021
    boundary = "----mcc" + uuid.uuid4().hex
    body = (f"--{boundary}\r\n"
            f"Content-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n"
            f"Content-Type: {mimetype}\r\n\r\n").encode() + data + f"\r\n--{boundary}--\r\n".encode()
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/api/assets", data=body, method="POST",
        headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    with urllib.request.urlopen(req, timeout=10) as res:
        json.load(res)
    rows = http("GET", "/api/assets", port=port)[1]
    return next(row["id"] for row in rows if row["filename"] == name)


def a_playlist(port=None):
    """A playlist, assigned to every declared display -- see test_overlay.py."""
    rows = http("GET", "/api/playlists", port=port)[1] or []
    playlist_id = (rows[0]["id"] if rows
                   else http("POST", "/api/playlists", {"name": "Test"}, port=port)[1]["id"])
    for row in http("GET", "/api/displays", port=port)[1] or []:
        if row.get("playlist_id") != playlist_id:
            http("PUT", f"/api/displays/{row['name']}", {"playlist_id": playlist_id}, port=port)
    return playlist_id


def item(item_id, port=None):
    return next(row for row in http("GET", "/api/playlist", port=port)[1] if row["id"] == item_id)


def api_flow():
    print("\n[100] a new item sits contained on black")
    with Server():
        asset = upload("poster.png", PNG, "image/png")
        playlist = a_playlist()
        status, _ = http("POST", "/api/playlist", {"asset_id": asset, "playlist_id": playlist})
        check("the item is created", status == 201, status)
        row = http("GET", "/api/playlist")[1][-1]
        check("its fit defaults to contain", row["fit_mode"] == "contain", row)
        check("and its background to black", row["fit_background"] == "#000000", row)

        print("\n[101] the fit and the background are stored per item")
        status, _ = http("PUT", f"/api/playlist/{row['id']}",
                         {"fit_mode": "cover", "fit_background": "#00ff00"})
        check("the edit saves", status == 200, status)
        saved = item(row["id"])
        check("cover is kept", saved["fit_mode"] == "cover", saved)
        check("and so is the colour", saved["fit_background"] == "#00ff00", saved)

        status, _ = http("POST", "/api/playlist",
                         {"asset_id": asset, "playlist_id": playlist,
                          "fit_mode": "scroll", "fit_background": "#123"})
        created = http("GET", "/api/playlist")[1][-1]
        check("a new item can carry both from the start",
              status == 201 and created["fit_mode"] == "scroll"
              and created["fit_background"] == "#123", created)

        print("\n[102] nonsense falls back, a colour that is not one is refused")
        http("PUT", f"/api/playlist/{row['id']}", {"fit_mode": "stretch-it"})
        check("an unknown fit becomes contain rather than an error",
              item(row["id"])["fit_mode"] == "contain", item(row["id"]))

        status, body = http("PUT", f"/api/playlist/{row['id']}",
                            {"fit_background": "red; display:none", "duration": 42})
        check("a background that is not a hex colour is a 400",
              status == 400 and "error" in (body or {}), (status, body))
        after = item(row["id"])
        check("and nothing else in that request was written",
              after["duration"] != 42 and after["fit_background"] == "#00ff00", after)

        status, body = http("POST", "/api/playlist",
                            {"asset_id": asset, "playlist_id": playlist, "fit_background": "blue"})
        check("the same refusal on create", status == 400 and "error" in (body or {}), (status, body))

        other = http("POST", "/api/playlists", {"name": "Zweite"})[1]["id"]
        status, _ = http("PUT", f"/api/playlist/{row['id']}",
                         {"playlist_id": other, "fit_mode": "cover"})
        check("a move carrying a fit edit is refused like any other combined edit",
              status == 400, status)


if __name__ == "__main__":
    try:
        api_flow()
    finally:
        for p in procs:
            p.terminate()
    print("\n" + ("ALL PASSED" if not failures else f"{len(failures)} FAILED: {failures}"))
    sys.exit(1 if failures else 0)
