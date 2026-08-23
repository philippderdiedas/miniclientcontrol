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
some OpenWRT builds and a fair number of consumer routers. It is the *guest's*
resolver doing it, so nothing on this device can detect or fix it. The startup
log prints the name and the address it should answer with, which is the first
thing to compare against what the guest's machine resolves. `--managed-cert off`
falls back to the bare address, which no resolver can object to.

**The URL still has `:3443` in it.**
The listener could not bind 443 and fell back, which is silent by design. Either
the privilege is missing (see
[deployment.md](deployment.md#giving-the-controller-port-443)) or something else
holds the port — on a two-display machine, the other instance.

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

**Where the windows are, with two displays** — `i3-msg -t get_tree` and look for
`window_properties.class` against the output name, or `i3-msg -t get_workspaces`
for the workspace-to-output mapping.

**What the cast is actually doing** — the display page keeps its peer connection in
a top-level binding, so a CDP `Runtime.evaluate` of `peer.pc.connectionState`,
`peer.pc.iceConnectionState` and `peer.pc.getStats()` works. Sampling
`framesDecoded`, `framesDropped`, `packetsLost` and `currentRoundTripTime` every
couple of seconds alongside `/proc/loadavg` is what distinguishes "fell behind"
from "connection failed", and the two look identical from the outside.

**Noise to ignore** — `chromiumoxide::handler: WS Invalid message: data did not
match any variant of untagged enum Message` is the library failing to decode a CDP
message it does not model. It has no functional consequence.
