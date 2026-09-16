# A guest casts to a screen they choose

**Status:** designed
**Date:** 2026-09-16

## What and why

[Multiple displays from one controller](2026-09-12-multi-display-design.md) gave
every declared screen its own browser, its own control loop and its own playlist.
Casting was the one thing it left whole: there is exactly one `CastSession` in
`AppState`, and `--cast-display` decides which panel it lands on. That was the
honest smaller thing to build, and it is documented as such in
[docs/casting.md](../../casting.md) — but it leaves a venue in an odd place. A
guest standing in the Werkstatt scans the QR on the Werkstatt panel and their
screen appears in the Foyer, because the deployment picked the Foyer months ago.
A second guest is refused for reasons they cannot see, in a room they are not in.

So: one cast session per declared screen, and the guest says which screen they
mean — by scanning the panel in front of them, or by picking from a list.

The four decisions that shaped this, with what they cost:

## Decision 1: one session per screen, simultaneously

`CastSession` stops being a singleton in `AppState` and becomes a field of
`Display`. Two guests can cast to two screens at once and never learn about each
other.

The alternative — keep the singleton and let the guest choose which screen it
lands on — is a much smaller change, and it was rejected on what a venue actually
looks like. The screens are in different rooms. A guest in the Foyer being told
*"someone is already casting"* because of something happening in the Werkstatt is
a refusal with no visible cause, and the person who could explain it is not in the
room.

What moves with the session: `sender`, `display`, `pairing`, `reservation`,
`previous_override`, `holding_override`, `showing`, `cast_announced`, `epoch`, and
every timer — `RESERVATION_TTL`, `PAIRING_TTL`, `SENDER_GRACE`, `PAGE_GRACE`.
These all describe one guest's session, and two guests must not share a deadline.

Three things deliberately do **not** move:

- **`attempts`**, the per-IP brute-force counter, stays controller-wide. Per
  screen it multiplies by the number of screens and hands an attacker N tries at
  a four-digit code instead of one.
- **`display_limits`** moves to `Display` but *outside* the session. It is a
  property of the panel's GPU, not of a cast, and the existing comment already
  says it must outlive the session so the next sender can constrain capture
  before its first frame rather than showing a black rectangle.
- **The room audio**, which gets its own decision below.

`is_active()` becomes per display, which makes `hide_during_cast` and the
cast-sourced QR drop per screen for free: a cast in the Werkstatt no longer
blanks the Foyer's overlay. Guest pages follow the same way — `mode: page` is per
session, so one screen can hold a guest's web page while another streams video.

Teardown keeps both existing guards, now per screen: the still-ours check against
`session.showing`, and `run_override_loop`'s unchanged-override skip. The second
one matters more here than it did, because two loops now poke independently.

## Decision 2: the QR carries the screen, and the guest can still switch

Each panel draws a QR encoding its own name (`https://<device>/?screen=foyer`).
Scanning the panel in front of you *is* the choice — zero taps for the normal
case — and the guest page names the screen it is bound to before anything is
shared, so a wrong scan is visible rather than discovered afterwards by looking at
a panel that did not change.

The page also offers a switcher: a list of screen labels, each marked `frei` or
`belegt`. This is a deliberate loosening. **A guest can take a screen they cannot
see.** It was chosen anyway, because the alternative failure — someone scans the
wrong panel, or wants the screen in the next room, and has no way forward — is the
more common one in a fab lab, where the people are known and the screens are not
a scarce resource being fought over.

Which QR a screen draws is an operator setting, **QR-Code auf dem Schirm führt
zu → diesem Schirm / Auswahl**, in the cast section of `admin.html`. One decision
for the venue, because that is what it is: both the overlay QR and the idle
screen's invitation read it, so the two cannot disagree. With a single declared
display it is inert, and the UI says so rather than offering a dead control.

Not chosen: putting it on each display's card in `displays.html`. Two screens
could then advertise differently, which a guest sees and no operator page
summarises. Also not chosen: folding it into the overlay's `qr_source` enum — the
idle page draws its own invitation without consulting the overlay configuration,
so the rule would have to be known in two places and would drift.

**Switching mid-cast is not offered.** Once a sender is streaming, the switcher is
disabled with a note: stop, then pick another screen. A live switch means tearing
down one `RTCPeerConnection` and negotiating another while the first screen's
override unwinds — two sessions in flight for one guest, to serve a case the guest
has already been shown before sharing.

## Decision 3: the chooser tells a guest what is free

`/api/cast/info` gains `screens: [{name, label, busy}]`, and the chooser renders
it with busy entries not selectable.

That endpoint is exempt from authentication regardless of address — it has to be,
the guest is by definition not loopback — so **any guest on the LAN can read the
screen labels and watch when each is in use.** That is the price of the chooser,
and it is paid knowingly: the alternative is a guest picking a screen that cannot
work and being refused after choosing, or a busy screen vanishing from the list
entirely, which makes a venue with someone casting look like a venue with fewer
screens.

Labels are operator-chosen, so a deployment that minds can name its screens `A`
and `B`. When casting is disabled, `info` reports `enabled: false` and **no**
screen list: a switched-off feature should not enumerate the venue.

## Decision 4: room audio is one resource with an owner

The venue has one speaker pair. Two casts unmuting at once is two videos talking
over each other, with nothing on either guest's phone explaining why.

So audio stays a single resource: `AppState` holds `audio_owner: Option<String>`,
claimed by the first cast that turns sound on and released on that cast's
teardown. The second caster's audio control says who has it — *„Ton läuft gerade
auf ‚Foyer'"* — rather than silently doing nothing.

**A guest cannot take audio from another guest.** That is a stranger silencing
someone mid-presentation. **The operator can**, through `/api/audio`, which is
already the authenticated twin of the guest's `/api/cast/audio` and already ends
in the same `apply_audio`.

The owner is validated against that screen's `is_active()` on every read, so a
cast that dies without a clean teardown frees the audio by itself instead of
leaving the room mute until a restart.

Not chosen: an operator setting pinning audio to one screen. It is deterministic,
and it means a cast on any other screen can never have sound even when nothing
else is running.

## Decision 5: the screen is a parameter on public routes, a path segment on operator routes

`is_cast_public_path` and `is_display_path` are both literal `matches!` lists of
exact strings, and [CLAUDE.md](../../../CLAUDE.md) treats keeping them that way
as load-bearing: widening `is_display_path` exposes display-only paths to the
whole LAN, narrowing the cast list locks guests out. Scoping cast routes under
`/api/displays/{name}/cast/…` would turn both predicates from string equality
into pattern matching.

So the rule is: **the screen travels as a parameter on anything in an exemption
list, and as a path segment only on authenticated routes.**

| Route | Audience | Names the screen by |
|---|---|---|
| `GET /api/cast/info` | public, any address | returns `screens: [{name, label, busy}]` |
| `POST /api/cast/claim` | public | `display` in the body |
| `POST /api/cast/pair` | public | `display` in the body |
| `GET /api/cast/ws` | public | the ticket; the display peer passes `?screen=` |
| `GET /api/cast/qr.svg` | public | `?screen=`, omitted for the chooser QR |
| `GET /api/cast/audio` | caster only | the ticket |
| `GET /api/cast/state` | loopback + operator | `?screen=` — stays literal for the idle page |
| `DELETE /api/displays/{name}/cast/session` | operator only | path segment |

`cast_display.html` and `empty_playlist.html` are both navigated by `browser.rs`,
which knows which display it is driving, so each is sent a URL carrying its own
screen. Nothing detects anything.

Ticket validation gains one check: a ticket minted for `foyer` presented on a
socket claiming `werkstatt` is refused. Today a ticket only has to be current;
with N sessions it must also match.

**`--cast-display` is removed**, and with it the unscoped `DELETE
/api/cast/session`. A guest who arrives without naming a screen meets the
chooser; there is no implicit screen anywhere.

This spec is therefore, in part, undoing work from two days ago: the multi-display
branch added `--cast-display` and `AppState::cast_display()`, and reviewing that
piece is what forced every cast site to resolve its display through one function.
That seam is exactly what this change needs, so the work was not wasted — but the
flag was added and removed inside one release, and this document says so rather
than leaving a reader to wonder.

## Guest experience

**Scanned a panel:** the page opens bound to that screen, headed *„Sie senden an:
Foyer"*, share button one tap away. The switcher sits in that header as a list of
labels with `frei`/`belegt` beside each, polled from `/api/cast/info`.

**No `?screen=`** — typed the URL, or the QR is switched to the chooser: the same
page opens on the chooser instead of a screen. One document, two entry states, so
there is one piece of guest UI to keep correct.

**A screen in the URL that no longer exists** — a QR printed for a panel since
removed from `--display` — falls to the chooser with *„Diesen Bildschirm gibt es
nicht mehr."*. The guest is holding a phone, not reading a status code.

**Pairing mode:** the guest chooses a screen, asks for a code, and reads it off
the panel in front of them — which is the proof-of-presence pairing exists for,
and is less ambiguous than today because there is no question which screen will
light up. Codes are unique across screens while alive, so a code typed against the
wrong screen is wrong rather than accidentally valid.

## Operator experience

`admin.html`'s per-screen rows gain a cast line: who is casting, since when, what
is showing, and a *Beenden* button per screen. The cast section keeps the global
switches (`cast_enabled`, auth mode, guest pages) and gains the QR setting from
decision 2.

`displays.html` does not change. It answers which playlist a screen plays;
casting is a different axis, and a second home for it there is one more page an
operator has to hold in their head.

## Failure modes

- **Two guests claim one screen at once** — the reservation is per screen, so the
  second gets `409` naming it. `attempts` stays controller-wide.
- **A ticket crosses screens** — refused. This check cannot fail today, which is
  why it does not exist.
- **The audio owner's cast dies uncleanly** — the owner is validated against
  `is_active()` on read and frees itself.
- **`cast_enabled` switched off** — ends *every* session, not one.
- **A display browser dies mid-cast** — `watch_display_arrival` is already per
  session; it stops being the only one.
- **Casting disabled** — `info` reports `enabled: false` and no screen list.

## Testing

A new `tests/cast/test_castscreens.py`, two declared screens and two Chromes,
reusing `test_display.py`'s harness (declared ports, one profile directory per
screen, `atexit` cleanup, `LONG` on any item whose id is asserted, a positive
barrier before every negative assertion).

The cases that carry the design: two senders casting simultaneously with each
override landing on its own screen; the cross-screen ticket refusal; `info`'s
busy flags flipping as sessions come and go; audio first-come, hand-back on
teardown, and the operator's override of it; pairing codes per screen and unique
while alive. And the negative that matters most — a cast on one screen leaving
the other screen's overlay, playlist and current item untouched.

## Work breakdown

Roughly fourteen tasks: the session moves into `Display` → per-screen timers and
watchdogs → the ticket binds a screen → `claim`/`pair` take a display → `info`
gains the list and `state` the parameter → operator-scoped session delete, and
`--cast-display` removed → audio ownership → `is_active` per display → the QR
setting and per-display QR resolution → the two display pages carry their screen
→ the guest page (bind, chooser, switcher) → `admin.html` → the integration suite
→ documentation.

## Out of scope

- **Any queue or takeover for a busy screen.** A busy screen is busy; the guest
  picks another or waits. A queue is a feature with its own design.
- **Per-screen cast switches.** `cast_enabled`, the auth mode and
  `guest_pages_enabled` stay global. A venue that wants casting on one screen
  only is a real request, but it is not this one.
- **Moving a live cast between screens.** See decision 2.
- **Per-screen audio sinks.** One device, one sink.
