# Roadmap

What is still missing for a complete signage controller, in the order it is
meant to be done. Remote management and centralising several devices are out of
scope here on purpose, and so is switching the panel itself on and off: that
belongs to the hardware, not to this software.

Each entry says what it is, why it is wanted, and the decision it still needs.
An entry becomes a spec under `docs/superpowers/specs/` when work on it starts,
and leaves this file when it ships.

## Next

### An item advances when its content ends

Besides a fixed time, an item may move on when its content is done: a video has
ended, a page is scrolled to the bottom, a PDF shows its last page. One mode
"end of content" whose meaning follows the content, not a list of trigger
types, and always with the duration as an upper bound, so a page that never
reports an end cannot hold the screen. In progress.

### Who may cast

Now that there are accounts, casting can ask *who*, not only *whether in the
room*. Per mode (cast a screen, show a page): off, anyone, or signed-in accounts
only — a page puts an arbitrary URL on the screen, so it is the likelier one to
restrict. The presence check (`cast_auth`: none, code, pairing) stays
independent: a signed-in account in the next room should still not take over the
foyer. Webhooks and the log can then name who cast.

Still open: any role, or a minimal casting-only role; how the guest page signs
in (the login page, then back). Technically the cast routes are exempt from auth
on purpose, so the middleware must *recognise* a session there without
*requiring* one — that is where a hole is easy to build. Pairs with OpenID
Connect below.

### Single sign-on with OpenID Connect

Signing in through the venue's identity provider instead of a local password, on
top of the accounts from users-and-roles: an SSO account is the same `users` row
with an issuer and subject instead of a password hash.

Still open: which providers to test against; roles from a groups claim or assigned
locally after the first sign-in; whether a first SSO sign-in creates an account or
needs an admin to invite it; behaviour offline (local accounts stay as fallback).
Technically: discovery, the authorization-code flow with PKCE and ID-token checks
on the hyper + `tokio-rustls` client `managed_cert.rs` already uses and `ring` for
the signatures — not `reqwest`, which does not cross-compile for armv7 here;
verify with `cross build`.

## Later

### A frozen screen is noticed

CDP reports healthy while the picture has not changed for hours — measured on a
Pi 3 whose GPU wedged, thirteen hours of one frame (see `CLAUDE.md`, *Webhooks*).
A periodic `Page.captureScreenshot` compared with the previous one, or a frame
counter in the page, would notice; the response is restarting that display's
Chromium and a webhook event. Care is needed not to call a static dashboard
frozen: an unchanged page is normal, an unchanged *clock in the overlay* is not.

Its screenshots are also the preview the admin page's status line should show —
deliberately left out of the asset preview, because what a screen shows is not
its asset's file (an override, a page, the overlay on top).

### Zones, layouts and widgets

A screen split into areas — main content plus a side column or a ticker. A
playlist item would become a layout: a set of zones, each with its own content.
Widgets (ticker, RSS, weather, countdown, room booking) come with it, because a
widget is what a zone shows when it is not a URL. Needs zones first; widgets
without zones are only the overlay again.

### Transitions between items

A fade instead of a hard cut. With one Chromium page per display the old page is
gone before the new one paints, so a crossfade needs two pages (as `keep_loaded`
tabs already are) or a fade-in on the new one.
