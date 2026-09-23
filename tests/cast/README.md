# Cast end-to-end tests

Plain-stdlib Python, no dependencies. They start real `miniclientcontrol`
processes against a debug build and drive them over HTTP and WebSocket.

```bash
cargo build
cd tests/cast
python3 test_cast.py        # signaling relay, override coupling, code mode
python3 test_pairing.py     # pairing mode, lockout, code expiry (~40s)
python3 test_conflict.py    # override precedence during a cast
python3 test_settings.py    # runtime settings, persistence, CLI precedence
python3 test_auth.py        # public cast paths vs. protected admin, loopback-only HTTP
python3 test_reserve.py     # the claim step: reserve before sharing
python3 test_basicauth.py   # credentials from the admin panel, CLI as the way back in
python3 test_port.py        # TLS port clash: fatal when explicit, next free otherwise
python3 test_public.py      # --public-url modes and the QR endpoint
python3 test_guestpage.py   # a guest showing a web page instead of casting (~45s)
python3 test_audio.py       # venue audio, and who is allowed to touch it
python3 test_limits.py      # the frame size the display announces to the sender
python3 test_managed.py     # the managed certificate (needs the network, else skips)
python3 test_browser.py     # real WebRTC between two Chrome instances
python3 test_overlay.py     # the settings, and the badge really on the page
python3 test_media.py       # image fit, video, a video's length, asset previews (needs Chrome)
python3 test_webhook.py     # outbound webhooks, end to end (~50s, needs Chrome)
python3 test_display.py     # several screens at once (needs a Chrome per screen)
python3 test_castscreens.py # two screens casting at once, independently (needs a Chrome per screen)
```

Five of them need a real Chrome — `test_browser.py`, `test_overlay.py`,
`test_media.py`, `test_webhook.py` and `test_display.py` — for their own reasons, given below.
`test_castscreens.py` reuses `test_display.py`'s harness rather than starting
its own, so it is a sixth: passing a case number that needs a browser (`86` or
`90`; every other case is plain HTTP/WebSocket and starts no Chrome at all)
launches Chrome on the very same two ports. **`test_display.py`,
`test_webhook.py` and `test_castscreens.py` all use CDP port 9242, so no two of
these three may run at the same time.**

`test_media.py` runs its own display Chrome on CDP port 9252, a second one for the
operator pages on 9253, and its own controller on 3051. The API cases
(`[100]`-`[102]`, `[110]`) need no browser. `[111]` records a three-second WebM in
the page, so no fixture file and no ffmpeg are involved.

`test_guestpage.py` is slow on purpose: the last case waits out the guest page's
thirty-second grace period, which is the whole liveness contract of that feature.

`test_webhook.py` needs `google-chrome-stable` too, for a less obvious reason:
three of the ten events are fired by the control loop in `browser.rs`, and that
loop does not run at all without a CDP connection. Without a browser the suite
would silently cover only the events that come from an HTTP handler, and its
most important case -- a receiver that never answers must not stall the playlist
-- would have no playlist to stall. Pass a case number (`python3 test_webhook.py
59`) to run just one.

`test_browser.py` needs `google-chrome-stable` and uses `--use-fake-device-for-media-stream`,
so it shares a synthetic camera rather than a screen — a headless Chrome has no
desktop to pick from. Everything after the `getMedia` call is the same code path.

`test_display.py` (cases `[70]`-`[79]`) declares two displays, `foyer` and
`werkstatt`, and starts one headless Chrome per screen on its own profile
directory — two Chromiums sharing a profile corrupt it, which is why the
controller derives one profile per display in the first place. Its cases are about
the properties a unit test cannot see: that two loops really are independent, and
that a caller who does not say which screen they mean is told rather than guessed
at. The cases that want *no* control loop (the routing, the refusals and the
playlist guard are answered by an HTTP handler) declare their screens on CDP ports
9 and 10, where nothing ever listens, rather than leaving a display implicit — an
undeclared display falls back to 9222 and would attach to a stray Chrome there and
write item ids into cases that are asserting an absence of them. A playlist item
pointed at `127.0.0.1:9` is likewise never loaded: Chromium refuses port 9 outright
with `ERR_UNSAFE_PORT`, so the navigation never leaves the browser.

`[79b]` waits for the next full minute on purpose — it is the boundary timer
under test — so it takes up to a minute.

`test_castscreens.py` (cases `[80]`-`[90]`) is `test_display.py`'s harness
again, proving the same "own" for casting: its own session, claim,
reservation, pairing code and room-audio contest per screen, not merely
differently named ones. Most of its cases are HTTP/WebSocket state that does
not depend on a browser at all and use the dead CDP ports, same as above; only
`[86]` (a cast on one screen leaves another's playlist and overlay untouched)
and `[90]` (each idle screen navigates to its own `?screen=` page, not
another's) need a real loop and a real Chrome, so passing any other case
number starts neither.

`wsclient.py` is the minimal stdlib WebSocket client from the picklecast test
suite (MIT, Evan Widloski); `cdp.py` layers just enough DevTools Protocol on top
to navigate a page and evaluate expressions.

These tests bind fixed ports (3021/3031/3041 HTTP, 3464/3474/3484 TLS,
9222/9223/9232/9242/9243 CDP) and write scratch files next to themselves.
