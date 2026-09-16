# Screen casting

A guest shares a screen or camera to the display over WebRTC. The controller
relays the handshake and owns the session; the media goes peer to peer and never
passes through it.

This replaced a separate picklecast process that had to be glued to the
controller with a webhook and an `/api/override` proxy. With both sides in one
process, neither is needed.

## The flow

```
guest                          controller                     display browser
  |                                 |                                |
  |-- POST /api/cast/claim -------->|                                |
  |<------------- ticket -----------|                                |
  |                                 |                                |
  |-- ws /api/cast/ws?ticket ------>|                                |
  |                                 |-- override := cast page ------>|
  |                                 |<------- ws role=display -------|
  |<---------- peer joined ---------|                                |
  |                                 |                                |
  |======== SDP / ICE, relayed verbatim, server does not parse ======|
  |                                                                  |
  |<=================== media, peer to peer ========================>|
```

## Why the code is checked before anything is shared

`POST /api/cast/claim` validates the code and **reserves the session**, returning
an opaque ticket. The WebSocket carries only that ticket.

Doing it the other way round — the obvious way — means a guest picks a window in
their browser's screen picker and *then* hears the code was wrong. It also lets
two guests sit in the picker at once with one of them guaranteed to lose.

A reservation does not pin the display: the playlist keeps running until somebody
actually streams. It expires on its own, is released on `pagehide`, and the
address that holds it may re-claim — a reload or a second click must not lock a
guest out of their own reservation. `/api/cast/info` reports `busy` relative to
the asking address for the same reason.

## Why admission happens after the upgrade

A WebSocket rejected at the HTTP layer gives the page neither status nor body, so
a wrong ticket would surface as nothing but "connection failed". The socket is
accepted and then refused with a typed error frame the page can show.

`role=display` is the exception: it is loopback-only and answered with a plain
status code, because the only client that ever asks is our own Chromium.

## Session rules

- **Exactly one sender and one display.** A second guest is refused at claim time,
  with a reason.
- **Ping every 15 s, drop after 45 s of silence.** A laptop whose lid closes stops
  answering without ever sending a TCP FIN, so the socket looks healthy until
  something probes it. This is what keeps a dead cast from freezing the signage.
- **Watchdogs:** a grace period after the sender's socket drops, so a page reload
  does not bounce the display back to the playlist; a deadline for the display to
  connect back; a TTL on a pairing code nobody uses.

Every one of these ends the same way: the override is released and the playlist
resumes where it left off.

## Which screen a cast lands on

A controller can drive several screens (see
[deployment.md](deployment.md#declaring-the-screens-a-deployment-drives)), but
casting is **one session for the whole controller, on one screen** — not one
session per screen. `--cast-display <name>` says which declared display a cast
pins; with no flag it is the first one declared. A name that is not declared is
refused at startup, rather than surfacing hours later as a guest scanning a QR
code and the picture appearing on the wrong panel.

Everything a cast touches follows that one display: the override that pins
`cast_display.html`, the restore on teardown, and the `override.set` webhook,
which names that screen.

What does **not** follow it yet, and is worth knowing before a venue declares a
second screen:

- **The guest URL and the QR code are global.** There is one sender address, one
  `/api/cast/qr.svg`, and neither says which screen it reaches.
- **The idle screen invites a cast on every display.** `empty_playlist.html` is
  the same page everywhere and shows the cast address, the QR code and — in code
  mode — the standing code whenever casting is enabled. A second panel standing
  idle therefore advertises a cast that will appear somewhere else.
- **`hide_during_cast` and the cast-sourced QR drop are global too**, because
  they follow `CastSession::is_active()`, which is one session's state and not
  one screen's.

Per-display casting — a session per screen, a QR that names its own screen — is
its own piece of work. `cast.rs` carries the session state machine, and one
session is what a venue with one guest at a time actually needs; the honest
position until then is that a second screen is a playlist screen.

## Authentication

`--cast-auth` picks how a guest proves themselves:

| Mode | How |
|---|---|
| `none` | anyone who can reach the page may cast — a trusted LAN |
| `code` | a fixed four-character code, known out of band |
| `pairing` | a fresh code appears on the display for 30 s, single use |

Basic auth is deliberately not an option here: it would mean handing the operator
password to every guest. Codes are compared in constant time and an address is
locked out after repeated failures.

There is **no standing pairing code**. One is minted when a guest asks for it,
lives about thirty seconds and is single use, so there is nothing permanent to
show; the `code` mode's fixed code is the standing one, and the admin UI edits it.
While a pairing code *is* alive, the operator's view of the session carries it
with its remaining seconds, so somebody helping a guest by phone can read out what
the display is showing. The public endpoint never carries it.

`--cast-auth=code` with no code configured logs an error and refuses every
sender, but does **not** stop the process. Casting must never keep the signage
from booting, and the operator can fix it in the UI without a restart.

## A real certificate, for a private address

By default a guest meets a self-signed certificate and clicks through a warning.
That is one more thing to explain to somebody who only wants to show a slide, and
on a phone the warning is worse than on a laptop.

With `--public-url none` (the default) and `--managed-cert auto` (also the
default) the device instead advertises itself as
`192-168-178-15.clientctrl.cc`. That zone's public DNS answers with the address
spelled out in the name, so the name points straight back into the LAN — and a
public wildcard certificate for `*.clientctrl.cc` covers it. The controller
fetches that certificate from `api.clientcontrol.cc`, caches it beside the
self-signed one, and renews it in the background. Guests get no warning at all.

Note the two names: the certificate is served by `clientcontrol.cc`, and the zone
it certifies is `clientctrl.cc`. Not a typo.

IPv4 comes first and only when it is [RFC 1918][rfc1918] private — `10/8`,
`172.16/12`, `192.168/16`, and deliberately not CGNAT or link-local. A device on
a public address is not the case this is for. Failing that, a globally routable
IPv6 is preferred over a ULA, and both are encoded the same way with `:` becoming
`-`, so `fd00::1` is `fd00--1.clientctrl.cc`. Link-local is never used: it needs a
zone index to be reachable at all, and the zone does not answer for it.

IPv4 before IPv6 is for the QR code. A dashed IPv4 label is a fraction of the
length of an IPv6 one, and the code is scanned from across a room.

[rfc1918]: https://datatracker.ietf.org/doc/html/rfc1918

Three properties worth knowing:

- **The name follows the address.** It is recomputed rather than stored, so a
  DHCP move changes the name — and needs no new certificate, because the wildcard
  already covers whatever it turns into. The old self-signed path had to
  regenerate on every address change.
- **The private key is public.** Every device needs it, so the API serves it
  without authentication, which means anybody can impersonate a
  `*.clientctrl.cc` name. This is the same arrangement `traefik.me` and
  `local.gd` use. The trade is deliberate: the names only ever point into
  somebody's LAN, and the alternative is a warning page for every guest.
- **The device checks its own name at startup** and logs whether the local
  resolver answers with the address it should. That catches the common shape of
  the rebinding problem below — device and guest behind the same router — but it
  measures *this* machine's resolver, not the guest's, so it only ever warns.
- **It can fail, and failure is quiet.** No network, no private address, or an
  API that will not answer, and the device falls back to the self-signed
  certificate and the bare address. `--managed-cert off` makes that the
  permanent choice, which is the setting for a device that must not talk to
  anything outside the LAN.

When the cache has expired and the API cannot be reached, the **expired**
certificate is served rather than falling back. Falling back would change the
advertised *name*, and every QR code already printed or scanned would stop
working; an expired certificate costs the same warning page the fallback would
have shown anyway.

## A guest showing a page

Sharing a screen is more machinery than some moments need. A guest who wants the
kiosk to show a menu, a schedule or a link somebody just mentioned can hand it
the address instead, and the kiosk loads the page itself — no video codec, no
laptop pinned to the room, and it looks better than a re-encoded picture of the
same page would.

It is off until an operator turns it on, and it is a switch of its own rather
than a corner of casting. Rendering a page costs the device almost nothing while
WebRTC costs it a great deal, so a display too weak to receive a cast can still
be given a page — and a venue that wants the reverse can have that too.

Everything else is the cast's: the same code, the slot claimed before anything
happens, one guest at a time, and the operator's stop button. The guest holds a
socket for as long as the page is up, and letting go hands the screen back after
a grace period — thirty seconds here rather than the cast's five, because a phone
whose tab was set aside is the expected case and not a fault. The keepalive
itself survives backgrounding: it is a protocol-level ping the browser answers
without waking any JavaScript, so what the longer grace covers is a tab the
system actually discarded.

Only `http` and `https`. Addresses on the local network are allowed
deliberately, because a venue may want its own dashboard on the screen.
Credentials in an address are accepted for that same reason, but they are
stripped everywhere the address is logged or displayed — so a guest's password
does not end up in the journal or on the operator's screen, and
`http://google.com@evil.test` is shown as the `evil.test` it is rather than as
the Google it pretends to be.

A page a guest puts up counts as a cast wherever that matters: the overlay
respects `hide_during_cast`, and a cast-sourced QR code disappears, because the
slot is taken and whoever scanned it would be turned away.

**Note what a guest can see this way.** With operator authentication switched
off, a guest can point the kiosk at its own admin page and read the cast code off
the screen. They cannot operate it — a kiosk has no keyboard — but it is legible.
Turning authentication on is the answer. Carving out one address would half
retract the decision to allow local addresses, and would leave every other
internal page reachable anyway.

## Port 443, when it can be had

With no `--cast-tls-port`, the listener tries 443 first and drops to 3443 and
upwards if it cannot have it. On 443 the port disappears from the URL, which is
the whole point: `https://192-168-178-15.clientctrl.cc/` is shorter to read out,
shorter to type, and fewer modules in the QR code.

Binding a port below 1024 needs a privilege this process does not have by
default, so failing to get it is ordinary and silent. Granting it is a deployment
decision — see [deployment.md](deployment.md#giving-the-controller-port-443). On a
machine driving two displays only one of the two can have 443; the other takes
3443 and its URL keeps the port.

## HTTPS is not optional

`getDisplayMedia` and `RTCPeerConnection` only exist in a secure context. The
display is fine — it reaches the controller over loopback, which counts as secure
— but the guest is a laptop opening `http://10.x.x.x`, which does not. Hence the
separate HTTPS listener.

The certificate is self-signed and regenerated when the machine's addresses
change: after a DHCP move a stale certificate fails *name* validation, which is a
scarier warning than an unknown issuer. The file holds the private key and is
written `0600`.

`rustls` is pinned to the `ring` backend rather than the default `aws-lc-rs`,
which needs a C toolchain the armv7 cross image does not have.

## The address guests are given

`--public-url` decides it: the LAN address, `mdns` for `<hostname>.local`, a bare
host, or a full base URL for a device behind a proxy that owns the port. Whatever
comes out is added to the certificate, or guests get a name mismatch on top of the
unknown-issuer warning.

Avahi announces only `<hostname>.local`, so any *other* `.local` name is published
by supervising `avahi-publish`. That is what lets one machine driving two screens
be `kiosk2-links.local` and `kiosk2-rechts.local` instead of one ambiguous name.

A machine that publishes with Avahi but resolves with systemd-resolved may not be
able to look up its own published names. Guests on the LAN still can, and that is
the case that matters.

## Quality: who decides what

Two knobs, and they belong to different people.

**Which way quality gives** is the guest's call, because it follows from the
content: a spreadsheet is unreadable once softened, a video unwatchable once it
stutters. The guest page offers *Schärfe* and *Flüssigkeit*, which set
`degradationPreference` (plus a matching content hint and framerate cap). This is
not a bespoke design — it is the same primitive every video platform exposes.

**The frame-size ceiling** is the receiver's. The display reports what it can show
over the `limits` channel, from its panel size and its GPU texture limit. A frame
wider than the texture limit decodes perfectly and then composites as nothing at
all — a black rectangle with healthy statistics behind it.

Neither of those says anything about whether the receiving **CPU** can decode that
in real time, and WebRTC carries no signal for "my decoder is drowning". So
`--cast-max-edge` lets the operator cap it per device. The cap is applied in the
one place the limit passes through the server, so the relay to the sender and
`/api/cast/info` cannot disagree about it.

What the video platforms have and a peer-to-peer link does not is an SFU with
simulcast, where the receiver picks a layer from several the sender emits. There
is no standard receiver-driven resolution signal in plain WebRTC, which is why the
`limits` message exists at all.

The guest page shows the ceiling, what is actually going out, and — from
`qualityLimitationReason` — whether the limit is their CPU, the network, or
neither. "Neither" is the interesting answer: quality is then bounded by the
display or by their own choice.

## Venue audio

While a cast runs, the guest can change the room's volume: every playing stream
individually, plus the output device, its level and its mute. A signage box may
also be running AirPlay or a music daemon, and someone about to present needs to
turn those down without hunting for whoever started them.

The same panel serves the guest and the operator, from one implementation, over
**two endpoints that differ only in who is let in**:

| Endpoint | Who | Guarded by |
|---|---|---|
| `/api/cast/audio` | the guest | the address of the sender actually connected |
| `/api/audio` | the operator | the operator credentials |

The guest's is exempt from operator credentials — somebody sharing a screen has to
reach it without them — so it is bound to the connected sender's address and
nothing else. The permission starts and ends with the cast. Turning the speakers
down is a physical act in a shared room, and "anyone who can reach the page" is
too wide for it.

Widening that check to admit the operator is the tempting shortcut and is wrong:
the route is exempt from authentication, so "also allow somebody else" would let
any guest on the LAN turn the room up at three in the morning. The operator gets a
second door instead, which needs no cast to be running. **Loopback is not a way
around it** — the loopback exemption covers the display browser's paths only, so
the admin page asks for credentials even on the device itself.

Both endpoints end in the same function, so the guest's knobs and the operator's
cannot drift apart.

The cast's own stream is identified through the process subtree of the browser the
controller launched. Names cannot decide it: with two displays both would just say
"Chromium".

Switching the output device drags the cast's stream along with it, because
`set-default-sink` only affects *new* streams — without the move, the audio keeps
coming out of the old device and the control looks broken.

Control goes through `pactl`, which is a client of the PulseAudio *protocol*;
PipeWire implements it too, so one code path covers both. The PipeWire-native
tools would be strictly narrower. A machine with no sound server at all reports
the panel as unavailable rather than failing.
