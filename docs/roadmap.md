# Roadmap

What is still missing for a complete signage controller, in the order it is
meant to be done. Remote management and centralising several devices are out of
scope here on purpose, and so is switching the panel itself on and off: that
belongs to the hardware, not to this software.

Each entry says what it is, why it is wanted, and the decision it still needs.
An entry becomes a spec under `docs/superpowers/specs/` when work on it starts,
and leaves this file when it ships.

## Next

Nothing is queued; the next entry is picked from below.

## Later

### More widgets of our own

Layouts place widgets, and built-in widgets the controller renders itself have
shipped: a **clock**, a **banner** (including a marquee ticker), a **QR** and a
**countdown**, usable both inside a layout and as a standalone item. What remains
is content that needs an outside source: **RSS**, **weather**, a **room-booking
board** — each its own spec, and each has to answer how it behaves on a device
that is often offline (weather and RSS need the network; the room board needs a
calendar source).

### A free-form layout editor

The layout data model already describes any 24×24 grid, so a designer beyond the
templates would be UI only. Wanted only if the templates plus drag-and-resize
turn out to be too little.

### Transitions between items

A fade instead of a hard cut. With one Chromium page per display the old page is
gone before the new one paints, so a crossfade needs two pages (as `keep_loaded`
tabs already are) or a fade-in on the new one.
