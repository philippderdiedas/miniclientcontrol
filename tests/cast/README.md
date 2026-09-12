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
python3 test_browser.py     # real WebRTC between two Chrome instances
python3 test_webhook.py     # outbound webhooks, end to end (~50s, needs Chrome)
```

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

`wsclient.py` is the minimal stdlib WebSocket client from the picklecast test
suite (MIT, Evan Widloski); `cdp.py` layers just enough DevTools Protocol on top
to navigate a page and evaluate expressions.

These tests bind fixed ports (3021/3031 HTTP, 3464/3474 TLS, 9222/9223/9242 CDP) and
write scratch files next to themselves.
