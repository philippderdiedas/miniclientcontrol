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

- **Exactly one sender and one display, per screen.** A second guest on the same
  screen is refused at claim time, with a reason; a second guest on a different
  screen is not refused at all. See
  [Which screen a cast lands on](#which-screen-a-cast-lands-on).
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
[deployment.md](deployment.md#declaring-the-screens-a-deployment-drives)), and
**every declared screen has its own cast session**. Two guests can cast to two
screens at once and never learn about each other: the sender, the reservation,
the pairing code, the override and every timer belong to one screen's session,
and two guests must not share a deadline.

That is not the cheap version. Keeping one session and letting the guest choose
where it lands would have been a much smaller change, and it was rejected on what
a venue actually looks like. The screens are in different rooms. A guest in the
Foyer told *"someone is already casting"* because of something happening in the
Werkstatt has been refused for a reason they cannot see, by somebody who is not in
the room.

One thing deliberately stays controller-wide: the **per-address counter behind the
code lockout**. Per screen it would multiply by the number of screens and hand an
attacker N tries at a four-character code instead of one.

`is_active()` is per screen with everything else, which is what makes
`hide_during_cast` and the cast-sourced QR drop per screen for free — a cast in
the Werkstatt no longer blanks the Foyer's overlay. Guest pages follow the same
way: one screen can hold a guest's web page while another streams video.

### The guest says which screen, by scanning it

Each panel's QR encodes its own name — `https://<device>/?screen=foyer` — so
scanning the panel in front of you *is* the choice, and it costs no taps. The
guest page names the screen it is bound to, *„Sie senden an: Foyer"*, before
anything is shared, so a wrong scan is visible up front rather than discovered
afterwards by looking at a panel that did not change.

Three entry states, one document, so there is one piece of guest UI to keep
correct:

- **Bound** — a `?screen=` naming a declared screen. Straight to the share
  buttons.
- **Chooser** — no `?screen=` at all: the list of screens, each marked `frei` or
  `belegt`, busy ones not selectable. This is what a typed address gets, and what
  a chooser QR gets.
- **Gone** — a `?screen=` naming a screen this deployment no longer declares, from
  a QR printed for a panel since removed. It falls back to the chooser with
  *„Diesen Bildschirm gibt es nicht mehr."*. The guest is holding a phone, not
  reading a status code.

With exactly one screen declared there is no chooser at all. An omitted screen
resolves while one display is declared and is refused once several are —
`display::resolve`, the same rule the playback routes follow — so a single-screen
venue never meets a list with one entry in it, and a guest who types the bare
address on a two-screen controller does.

A bound guest can still **switch**, from a list of labels beside that header. This
is a deliberate loosening: a guest can take a screen they cannot see. It was
chosen anyway, because the alternative failure — someone scans the wrong panel, or
wants the screen in the next room, and has no way forward — is the more common one
in a fab lab, where the people are known and the screens are not a scarce resource
being fought over.

**Switching mid-session is not offered.** Once a sender is streaming, or a guest
page is up, the switcher is disabled with a note: stop, then pick another screen.
A live switch means tearing down one `RTCPeerConnection` and negotiating another
while the first screen's override unwinds — two sessions in flight for one guest,
to serve a case the guest was shown before they shared anything.

### Which QR a screen draws is one setting for the venue

**QR-Code auf dem Schirm führt zu → diesem Schirm / Auswahl aller Schirme**, in
the cast section of `admin.html`. One decision for the deployment, because that is
what it is: the overlay's cast QR and the address a screen prints while idle both
read it, so the two cannot disagree. With a single display declared it changes
nothing and the page says so rather than offering a dead control.

Not put on each display's card in `displays.html`: two screens could then
advertise differently, which a guest sees and no operator page summarises. Not
folded into the overlay's `qr_source` either — the idle page draws its own
invitation without consulting the overlay configuration, so the rule would have to
be known in two places and would drift.

`/api/cast/qr.svg` (`cast::url::cast_qr`) takes the same `?screen=` every other
scoped cast route does and reads the setting itself, so the picture drawn on the
idle and standby screens agrees with the address printed beside it and with the
overlay's own QR — one setting, read in three places, never a fourth.

### What the chooser can be told, and by whom

`/api/cast/info` carries `screens: [{name, label, busy, max_edge}]`, and it is
exempt from authentication regardless of address — it has to be, the guest is by
definition not loopback. So **any guest on the LAN can read the screen labels and
watch when each is in use.** That is the price of the chooser and it is paid
knowingly: the alternative is a guest picking a screen that cannot work and being
refused after choosing, or a busy screen vanishing from the list entirely, which
makes a venue with someone casting look like a venue with fewer screens. Labels
are operator-chosen, so a deployment that minds can name its screens `A` and `B`.

The list is withheld when **neither** casting nor guest pages is reachable: a
switched-off feature should not enumerate the venue. The two switches are
independent, so a venue running guest pages with casting off still gets the list —
a page-mode claim binds against it the same way.

`busy` is relative to the asking address, exactly like the claim endpoint: a guest
already holding a screen's reservation must not be told it is busy. `max_edge`
rides along because a sender constrains its capture *before* it has a socket —
that is the whole reason a display's announced limit outlives its session — and
with one session per screen there is no single limit left to report.

### The screen travels as a parameter, never as a path segment

...on anything a guest reaches. `cast::is_cast_public_path` and `is_display_path`
are literal string matches, and
[architecture.md](architecture.md#three-audiences-for-http) treats keeping them
that way as load-bearing; scoping the cast routes under
`/api/displays/{name}/cast/…` would turn both predicates into pattern matching. So:

| Route | Audience | Names the screen by |
|---|---|---|
| `GET /api/cast/info` | public, any address | it answers for all of them |
| `POST /api/cast/claim` | public | `display` in the body |
| `DELETE /api/cast/claim` | public | `display` in the body |
| `POST /api/cast/pair` | public | `display` in the body |
| `GET /api/cast/ws` | public | the ticket; the display peer passes `?screen=` |
| `GET`/`POST /api/cast/audio` | the connected sender | `?screen=` |
| `GET /api/cast/state` | loopback + operator | `?screen=` |
| `DELETE /api/displays/{name}/cast/session` | operator only | path segment |

The operator's stop is the one path-scoped cast route, and it may be shaped that
way precisely because it is in neither exemption list.

**A sender never names its screen on the socket.** Its ticket already does: the
socket finds whichever session holds a live reservation for that exact ticket, and
a ticket held by no session is refused before any session is touched. Trusting a
query string instead would let a sender claim a screen its reservation was never
issued for.

### What the operator sees

`/admin.html` reports a cast line per screen — who is casting, since when, what is
showing — and a *Beenden* button that ends that screen's session and leaves the
others running. The switches above it stay global: `cast_enabled`, the
authentication mode and guest pages are the venue's, not one panel's, and turning
`cast_enabled` off ends **every** session rather than one.

`displays.html` does not change. It answers which playlist a screen plays; casting
is a different axis, and a second home for it there is one more page an operator
has to hold in their head.

### Not built

- **Any queue or takeover for a busy screen.** A busy screen is busy; the guest
  picks another or waits.
- **Per-screen cast switches.** A venue that wants casting on one screen only is a
  real request, but it is not this one.
- **Per-screen audio sinks.** One device, one sink — see
  [Venue audio](#venue-audio).

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

### Who may cast

The code answers *whether a guest is in the room*; each screen also says *who*
may use it, separately for casting and for showing a page: **jeder**, **nur
Konto** or **aus**, on the displays page. Any account counts, whatever its role.
A screen kept for accounts shows a guest "Anmelden zum Casten", which signs in
on the same address and comes back; a signed-in member still types the code —
an account in the next room should not take over the foyer. The venue's own
switches stay above this: with casting off in the settings, no screen can turn
it back on. Switching a mode off on one screen ends that screen's session of
that mode and nobody else's. Who cast shows on the displays page and in the
`cast.started` and `guest_page.shown` webhooks.

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
happens, one guest at a time on a screen, the guest's choice of which screen, and
the operator's stop button. The guest holds a socket for as long as the page is
up, and letting go hands the screen back after
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

A page a guest puts up counts as a cast wherever that matters: on that screen the
overlay respects `hide_during_cast`, and a cast-sourced QR code disappears,
because the slot is taken and whoever scanned it would be turned away. Only on
that screen — a page in the Foyer says nothing about the Werkstatt.

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
decision — see [deployment.md](deployment.md#giving-the-controller-port-443). One
controller needs 443 once however many screens it drives; it is two *controllers*
on one machine that compete for it, and the one that loses takes 3443 and keeps
the port in its URL.

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

### One room, one owner

The venue has one speaker pair, and with a cast session per screen there can be
two guests holding two screens. Two of them unmuting at once is two videos talking
over each other, with nothing on either phone explaining why. So the audio stays a
single resource with an owner: the first cast that touches it claims it, and the
second caster's control answers *„Ton läuft gerade auf ‚Foyer'"* rather than
silently doing nothing.

**A guest cannot take the audio from another guest.** That is a stranger silencing
someone mid-presentation. **The operator is not bound by it**: `/api/audio` needs
no cast running at all and never goes through the ownership check, so whoever runs
the venue can always turn the room down. It does not move the ownership either —
the screen that holds it keeps it until its session ends.

The owner is released on that cast's teardown, and validated against the owning
screen's `is_active()` on every read — so a claim left behind by a screen that was
undeclared from the configuration, or by a guest whose socket dropped before it had
put anything on the display, frees itself instead of leaving the room mute until a
restart. It is *not* a cure for a crashed browser: a process that dies never
touches the session, so the session still reads active.

Not chosen: an operator setting pinning the audio to one screen. It is
deterministic, and it means a cast on any other screen can never have sound even
when nothing else is running.

One rough edge: `/api/audio` is unscoped and singles out the **first declared**
screen's browser as "the cast's own stream". Volume, mute and the output device
all apply to the room either way, but the stream-dragging above is the operator's
blind spot — switching the output device while the cast is on a later screen does
not carry that cast's audio across with it.
