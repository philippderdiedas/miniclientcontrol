# Roadmap

What is still missing for a complete signage controller, in the order it is
meant to be done. Remote management and centralising several devices are out of
scope here on purpose, and so is switching the panel itself on and off: that
belongs to the hardware, not to this software.

Each entry says what it is, why it is wanted, and the decision it still needs.
An entry becomes a spec under `docs/superpowers/specs/` when work on it starts,
and leaves this file when it ships.

## Next

### Users and roles

One set of basic-auth credentials covers everything today. Three roles instead:

- **Admin** — the system side: settings, webhooks, displays, casting, users.
- **Manager** — content: assets, playlists, assignments; approves what an editor
  proposes.
- **Editor** (Redakteur) — proposes changes to content, which take effect only
  once a manager approves them.

This is the largest item here. It needs a users table with password hashes (the
PBKDF2 already used for the operator password), a login with sessions instead of
basic auth, a role check per route alongside `is_display_path` and
`cast::is_cast_public_path`, and a model for **pending changes** — an edit that
is stored but not live, shown to a manager as a diff.

Still open: whether an editor's proposal is per field or per card, and whether
the CLI password stays as the admin's recovery path (it should — see
[deployment.md](deployment.md#runtime-settings-versus-flags)).

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
