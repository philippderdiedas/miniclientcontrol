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

`--cast-auth=code` with no code configured logs an error and refuses every
sender, but does **not** stop the process. Casting must never keep the signage
from booting, and the operator can fix it in the UI without a restart.

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

The endpoints are bound to the **address of the sender that is actually
connected**, so the permission starts and ends with the cast. Turning the speakers
down is a physical act in a shared room, and "anyone who can reach the page" is
too wide for it.

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
