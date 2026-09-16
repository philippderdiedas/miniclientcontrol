# Troubleshooting

## Reading the cast log

The session lines tell most of the story. The difference between these two is the
one worth learning:

```
Cast: Sender asked to stop            the guest ended it -- a clean stop frame
Cast: Sender at <ip> disconnected     the socket dropped -- no stop was sent
```

A disconnect without a stop means the guest's page tore the connection down
itself, which it does when the peer connection fails. That is a network or
capacity problem, not a click.

```
Cast: display pinned to the cast page      a cast took the screen
Cast: display can show frames up to Npx    the display announced its ceiling
Cast: display offers Npx, capped to Mpx    --cast-max-edge overrode it
Cast: sender did not return, ending session the grace period expired
Cast: display released, playlist resumes   the screen went back to the playlist
Cast: pairing code expired unused          nobody used the code in time
```

## Symptoms

**The display shows nothing / stays on `about:blank`.**
Is there a browser? `--no-launch-browser` or a missing Chromium leaves the loop
reconnecting forever. Check that the CDP port is listening, and that the unit has
`DISPLAY` in its environment — a controller that starts the browser needs it, one
that only spoke CDP did not.

**A declared screen sits on the idle page while the other one plays.**
It has no playlist assigned, which is a state and not a fault: a display with no
`playlist_id` shows the idle page deliberately. The automatic hand-over of the
oldest playlist happens **only when exactly one screen is declared** — it exists
to carry a single-screen device across the upgrade, and handing the foyer a list
made for the workshop would be wrong content rather than an obvious gap. So a
device that used to play something and has just been given two `--display` flags
comes up with both screens idle, by design. Assign them on `/displays.html`, or
check `GET /api/displays` for a `playlist_id` of `null`. The startup log says so
too, once a playlist exists: *"N display(s) have no playlist yet"*.

**The controller exits immediately after `--display` was added.**
That is the intended reaction to a command line it cannot honour, and the message
names the reason: `--chromium-class`, `--chromium-user-data-dir` and a non-default
`--cdp-url` are derived from the display's name and position and are refused
rather than silently ignored, as is a `--class` smuggled through
`--chromium-arg`, a duplicate name or port, a name outside `[A-Za-z0-9_-]`, and a
non-numeric port after the colon. See
[deployment.md](deployment.md#flags-that-are-refused-rather-than-ignored).

**A playlist URL shows Chromium's privacy warning.**
It should not: invalid certificates are accepted deliberately. If it happens
anyway, the connection to CDP is the thing to look at, not the certificate.

**The screen goes blank when Basic Auth is switched on.**
Something the display browser fetches lost its exemption. The display list is
loopback-scoped; check that the path in question is in it.

**Guests cannot reach the page at all.**
Plain HTTP is loopback-only by design. Guests use the HTTPS port. If a guest
typed the host without a scheme, their browser tried plain HTTP on the TLS port,
which fails at the handshake before any of our code runs — the page redirects
from HTTP to HTTPS, but only on the HTTP port.

**A guest is told someone else is casting, and nobody is.**
A reservation is held for the claim window. It expires on its own, is released
when the tab closes, and the address that holds it may re-claim. If the message
persists for a different address, check `/api/cast/state` for `reserved`.

**Casting works, then stops after a minute or two.**
Look at whether the resolution stayed constant while the framerate fell. That is
the receiver falling behind, and the answer is `--cast-max-edge`, not a bug. See
[raspberry-pi.md](raspberry-pi.md) for what the numbers look like.

**The audio panel does not appear.**
On the *guest* page it only exists while a cast is running, and only for the
address that is casting. The operator's copy on the admin page needs no cast. If
neither shows up, the device has no `pactl` or no sound server, the backend
reports itself unavailable, and the panel stays hidden on purpose.

**Guests get "server not found" for a `.clientctrl.cc` name.**
That name is public DNS pointing at a private address, which is exactly the
pattern DNS-rebinding protection blocks — dnsmasq's `stop-dns-rebind`, Pi-hole,
some OpenWRT builds and a fair number of consumer routers.

Start with the line the controller logs at startup:

```
10-206-211-199.clientctrl.cc resolves to 10.206.211.199 here      good
10-206-211-199.clientctrl.cc does not resolve on this device      the resolver refuses it
```

The warning is a strong hint when the device and the guest sit behind the same
router, which is the usual arrangement — the same resolver is filtering for both.
It is not proof either way, because it measures *this* machine's resolver: a
device with its own upstream can pass while guests fail, and a guest whose phone
uses DNS-over-HTTPS bypasses the router and works even when the warning fired.

If it is really the guest's resolver, nothing on the device can fix it. Point
that resolver's rebinding exception at `clientctrl.cc`, or use `--managed-cert
off` and hand out the bare address, which no resolver objects to.

**The URL still has `:3443` in it.**
The listener could not bind 443 and fell back, which is silent by design. Either
the privilege is missing (see
[deployment.md](deployment.md#giving-the-controller-port-443)) or something else
holds the port — on a two-display machine, the other instance.

**A guest's page never appears.**
Guest pages are off by default. They are refused at claim time, so the guest
should have been told why rather than seeing nothing happen. If the address was
accepted and the screen did not change, it was probably a file rather than a
page: downloads are refused browser-wide, so nothing is written and nothing is
shown.

**The screen went back to the playlist while the guest was still standing there.**
Their page stopped holding its socket. On a phone that usually means the tab was
discarded rather than merely backgrounded — backgrounding alone does not do it,
because the keepalive is answered by the browser and not by the page's script.

**`avahi-publish` errors every few minutes.**
`avahi-utils` is not installed and `--public-url` names a `.local` host that is not
this machine's hostname. Either install it, or set the hostname to that name, or
use `--public-url mdns`.

**The controller refuses to start, complaining about a port.**
An explicitly configured `--cast-tls-port` that is taken is fatal, and the usual
cause is an older instance of this binary still holding it. Drop the flag to take
the next free port instead, or find the process.

## Diagnostics that pay for themselves

**What is on screen right now**

```bash
curl -s http://127.0.0.1:9222/json | python3 -c \
  'import json,sys; print([t["url"] for t in json.load(sys.stdin) if t["type"]=="page"])'
```

**Where the windows are, with several displays** — each browser carries the class
`miniclientcontrol-<name>`, which is `WM_CLASS` under X11 and the `app_id` under
Wayland. Under i3, `i3-msg -t get_tree` and look for `window_properties.class`
against the output name, or `i3-msg -t get_workspaces` for the
workspace-to-output mapping. Under sway, `swaymsg -t get_tree` and look for
`app_id`, or `swaymsg -t get_outputs` for the connector names the `assign` lines
refer to.

**What the cast is actually doing** — the display page keeps its peer connection in
a top-level binding, so a CDP `Runtime.evaluate` of `peer.pc.connectionState`,
`peer.pc.iceConnectionState` and `peer.pc.getStats()` works. Sampling
`framesDecoded`, `framesDropped`, `packetsLost` and `currentRoundTripTime` every
couple of seconds alongside `/proc/loadavg` is what distinguishes "fell behind"
from "connection failed", and the two look identical from the outside.

**Noise to ignore** — `chromiumoxide::handler: WS Invalid message: data did not
match any variant of untagged enum Message` is the library failing to decode a CDP
message it does not model. It has no functional consequence.
