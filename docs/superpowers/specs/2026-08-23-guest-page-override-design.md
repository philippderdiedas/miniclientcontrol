# Guest page override

**Status:** approved, not yet implemented
**Date:** 2026-08-23

## What and why

A guest standing in front of the display can already share their screen. Often
that is more machinery than the moment needs: they want the kiosk to *open a
page* — a menu, a schedule, a link somebody just mentioned — and screen sharing
is a poor way to do it. It pins their laptop to the room, spends the Pi's CPU on
a video codec, and looks worse than the page would if the kiosk simply loaded it.

So a guest may hand the kiosk a URL instead. The operator decides whether that is
allowed, with a setting of its own.

This is the operator's playlist override, offered to a guest — but reached
through the guest's door rather than the operator's, and bounded by the same
session rules casting already enforces.

## Decisions taken

Four questions shaped this, and their answers are load-bearing:

1. **Lifetime: the guest's page must stay open.** The override lives as long as
   the guest holds a socket, exactly like a cast. Not a fixed timer.
2. **URL scope: `http` and `https` only, LAN targets allowed.** No blocking of
   private or loopback destinations — a venue may legitimately want an internal
   dashboard on screen.
3. **The switch is independent of casting.** A venue can allow guest pages while
   screen sharing is off, and the reverse. Rendering a page costs a Raspberry Pi
   almost nothing while WebRTC costs it a great deal, so this is a real
   configuration and not a hypothetical one.
4. **The guest may choose scrolling**, from two options rather than the
   operator's full set. `"none"` is `ScrollMode::None`; `"slow"` is
   `ScrollMode::Continuous` with the same defaults the operator's UI offers —
   `speed: 2.0`, `top_delay: 2000`, `return_delay: 2000`. The guest never sees
   those numbers, and step mode is not offered.

## Approach

`CastSession` gains a notion of *what is being shown*, rather than a second
session type living beside it.

The session already owns everything this feature needs: the single slot, the
claim and ticket, the socket with its ping and grace period, the saved previous
override, and the watchdogs. Two sessions competing for one override slot would
mean duplicating all of that plus arbitration in both directions — the shape that
drifts apart. One screen means one slot, so one session.

The house already says *a cast is an override*. This generalises that sentence to
*what a guest shows is an override*.

```rust
enum Showing {
    Nothing,
    Cast,
    Page { url: Url, scroll: ScrollMode },
}
```

`activate_display` and `deactivate_display` take the `OverrideItem` to install
instead of hardcoding `cast_display_url(port)`. Their names already promised
this; the change straightens them rather than extending them.

### What falls out for free

- **`is_active()` needs no change** and correctly covers both. A cast-sourced QR
  code is already dropped while a session is active, and it should be dropped
  during a guest page too: the slot is taken, so whoever scanned it would be
  turned away.
- **`hide_during_cast` applies to both.** Its meaning becomes "while a guest has
  the screen". Same argument as a presenter's slides.
- **`DELETE /api/cast/session` already ends it.** The operator's stop button
  needs no new endpoint.
- **One guest at a time** and **the code is checked before anything happens**
  come from the claim step, unchanged.

### The one real difference

A cast waits for the *display* to connect back over its own socket, guarded by
`watch_display_arrival`. A guest page has no display peer at all — `browser_loop`
simply navigates there. **That watchdog must not fire for a page**, or it tears
the page down after its deadline for a peer that was never coming.

## Flow

1. Guest picks **Webseite zeigen**, types a URL, chooses scrolling.
2. `POST /api/cast/claim` — unchanged. The code is validated and the slot
   reserved before anything else, for the same reason it is for a cast.
3. The socket opens with the ticket, then carries one new frame:
   `{"type":"present","url":"…","scroll":"none"|"slow"}`. A **second `present`
   frame on the same socket replaces the URL** rather than being refused: the
   guest mistyping an address should not cost them the slot and a fresh claim.
4. The server validates the URL and pins the override to it.
5. `browser_loop` navigates. No display socket is involved.
6. It ends when the socket drops (after a grace period), when the guest presses
   stop, when the operator ends the session, or on ping timeout.

**The grace period is longer than the cast's.** A cast uses five seconds, enough
to survive a page reload. Here the expected normal case is a phone whose tab is
backgrounded, so the page comes down too eagerly at that setting. Thirty seconds.

## URL handling

Accepted: parses as a URL, scheme is `http` or `https`, has a host, within a
length cap of 2048.

**Embedded credentials are accepted but never echoed.** Rejecting
`https://user:pass@host/` would be theatre — anyone who can write that can write
`?token=…`, which cannot be filtered — and it would break the internal-dashboard
case that decision 2 deliberately allows.

The real problems with them are display and logging, so that is where they are
solved. The browser receives the URL in full; the log line and
`/api/cast/state` carry it **parsed and with the userinfo removed**. That also
disposes of `http://google.com@evil.test`, which reads like Google in a card
that prints the raw string and becomes an unambiguous `http://evil.test/` once
reserialised.

Parsing uses the `url` crate, already in the tree via `chromiumoxide`.

## Settings

New `guest_pages_enabled`, default **off**, edited in the admin UI beside the
casting options, with `--guest-pages <on|off>` to pin it from a unit file. The
flag follows the documented rule that a deployment must be able to nail a runtime
setting down, and a switch that decides whether strangers may put content on a
venue's screen is worth that.

`cast_enabled` and `guest_pages_enabled` are independent. Consequence worth
stating because it is easy to get wrong: **the claim and socket paths must stop
depending on `cast_enabled` alone** and admit a guest when either capability is
on. `--disable-cast` still takes both, because without the HTTPS listener there
is no guest page at all.

## Surfaces

| Surface | Change |
|---|---|
| `GET /api/cast/info` | gains `page_enabled` (public) |
| `GET /api/cast/state` | gains `showing`: `null`, `"cast"`, or `{ "page": "<redacted url>" }` |
| `/api/cast/ws` | gains the `present` frame, sender role only |
| `web/index.html` | a third button, revealing a URL field and two scroll choices |
| `web/admin.html` | the new setting; the status card shows the page a guest put up |

`GET /api/settings` and `PUT /api/settings` carry the new field, and `Locks`
reports whether the flag pinned it.

## Errors

A rejected URL comes back as a typed error frame on the socket and is shown
inline on the guest page, the same way a refused claim is. The feature being off,
or the slot being taken, is refused at claim time with a reason — before the
guest has typed anything they would lose.

Nothing here may take the signage down: a bad frame ends the attempt, not the
process.

## Testing

Rust unit tests for URL validation as a table — accepted schemes, rejected
schemes, missing host, over-length, and that redaction strips userinfo while the
browser-bound string keeps it.

`tests/cast/test_guestpage.py`:

- the setting off refuses the claim, with a reason
- on: claim plus `present` pins the override to the URL
- the scroll choice arrives in the override
- the guest's socket dropping returns the display to the playlist after the grace
- the operator's stop ends it
- a cast and a page cannot hold the slot at once, in both orders
- `cast_enabled` off with guest pages on still works end to end
- a `present` frame from a display-role socket is refused

## Known consequences

**With basic auth off, a guest can put `/admin.html` on the screen** and read the
cast code off it. They cannot operate it — a kiosk has no keyboard — but it is
legible. The remedy already exists, which is to turn authentication on, so this
goes in the documentation rather than into a special case for our own address.
Carving out one origin would half-retract decision 2 while still leaving every
other internal page reachable.

**A backgrounded phone tab ends the page.** Inherent to decision 1 and mitigated,
not removed, by the longer grace period.

**`cast.rs` grows**, and it is already the second-largest file at 1244 lines. The
change generalises `activate_display`/`deactivate_display` rather than adding a
parallel path, so the growth is bounded; if it starts to sprawl, the QR helpers
are the natural thing to lift out, but that is not part of this work.

## Documentation to update

`docs/casting.md` (a section on guest pages), `docs/features.md`, `README.md`
(API and flags), and `CLAUDE.md` — the invariants being that a guest page is an
override like a cast, that `is_active()` covers both, that
`watch_display_arrival` must not fire for a page, and that the claim path keys
off either capability rather than `cast_enabled`.
