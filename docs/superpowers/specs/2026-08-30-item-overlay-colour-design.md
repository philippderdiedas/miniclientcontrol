# Item overlay colour override

**Status:** implemented
**Date:** 2026-08-30

## What and why

The overlay's text colour is a global setting. One colour for the whole
playlist, and on signage that is almost always right: the display should not
change character item by item.

Almost. A playlist that is uniformly dark with one bright item breaks it. The
white clock the global overlay draws is unreadable for the forty seconds that
item is up, and there is nothing an operator can do about it short of changing
the colour for everything and breaking the other twelve items instead.

So an item may override the overlay's text colour, for as long as it is showing.

## Decisions taken

Two questions shaped this, and their answers are load-bearing:

1. **The override recolours the whole box, not just the item's own layer.** The
   problem is the *global* layer's white clock sitting on a bright page. An
   override that only reached the item's own text would leave the thing that
   prompted it untouched.

   This is a deliberate exception to "layers naming the same corner share one
   box and the first of them decides how it looks". The rule becomes: the first
   layer decides, unless an item in that box overrides the colour. Nothing else
   about the box style is negotiable — see *Not in scope*.

2. **Colour alone is enough; no content required.** A bright page often wants no
   badge of its own — it only wants the house clock to stay readable. Requiring
   text or a QR to carry a colour would mean typing a dummy character, and a
   space is not a design.

## The rule, in one sentence

An item recolours the box it sits in.

Sharing the global overlay's corner recolours that shared box, clock and date
included. Naming its own corner recolours only its own box, and the global one
stays as it was.

An item with no content of its own has no box of its own, so its `position` says
nothing. Such an item recolours the *global* box regardless of the corner it
names — the alternative is a stored setting that silently does nothing.

## Approach

The resolution happens server-side, in `settings::overlay_payload`, and the
display runtime does not change at all.

`overlay_payload` is already the only place that builds the runtime's
configuration, and `overlay.js` already styles a box from its first layer. If
the payload hands over a first layer that already carries the effective colour,
every consumer is correct for free — including the admin preview, which renders
the display's own runtime against the identical payload. Teaching the runtime a
second notion of where a colour comes from would put style resolution in two
places, and two places drift.

### Data model

One field on `ItemOverlay`:

```rust
/// Empty means "the global overlay's colour", which is the usual case.
pub color: String,
```

It lives inside the `overlay_config` JSON blob, so **there is no migration**.
`ItemOverlay` is `#[serde(default)]`; rows written before this field decode to
an empty string, which is exactly "inherit".

Empty-means-inherit follows `position`, where empty already means "wherever the
global overlay is".

### Drawing versus recolouring

`draws()` today answers one question for two purposes. It splits:

| Predicate | Answers | True when |
|---|---|---|
| `draws()` | does this item contribute a layer? | `enabled` and it has text, an image or a QR |
| `recolours()` | does this item override the colour? | `enabled` and `color` is set |

Keeping them apart is what stops a colour-only item from pushing an empty layer
and drawing an empty box.

### Payload resolution

1. Build the global layer as today.
2. If the item recolours, work out which box it lands in — its own `position`
   when it draws and names one, the global overlay's otherwise.
3. Stamp the effective colour onto every layer already in that box, and onto the
   item's own layer if it has one.

A colour-only override where the global overlay is switched off or empty is a
no-op: there is no box to recolour. Harmless, and not worth an error on a screen
nobody is standing in front of.

### Validation

`is_hex_colour` already exists and is what the global colour is checked with. A
value that is not a colour falls back to inherit rather than being rejected —
the same call the codebase already makes for an unknown corner and for a global
colour that will not parse. A stored colour that cannot render is worse than a
stored colour that is ignored, because only one of them is visible.

### Storage

The existing rule is that an item overlay is stored as SQL `null` unless it
would actually draw something, so read paths never have to tell "switched off"
from "empty". That becomes: unless it would draw something **or** recolour.

### UI

`playlist.html` gets a checkbox — *Eigene Textfarbe* — and a colour picker. The
checkbox is what expresses "inherit": a colour input has no empty state, so
without it there is no way back to the global colour once one is chosen. This is
the same shape as the admin page's QR choice `keiner`, which is a UI value with
no server-side counterpart.

Unchecked writes an empty string.

The card is the edit form and the page polls every two seconds, so the new
control joins the existing `dirty` tracking like every other field on it.

## Not in scope

**`color_alpha`.** The item sets the hue; the global opacity still applies.
Signage text that is deliberately faint is rare, and the playlist card is
already the densest form in the application.

**Background, size, margin, the box style generally.** The complaint was about
readable text on one bright page. An item that could restyle the box completely
is a different feature — it would let the display change character item by item,
which is the thing the global-only design was protecting.

## Testing

In `tests/cast/test_overlay.py`, against the real browser:

- an item's colour recolours the shared box, clock and date included
- an item in its own corner recolours only its own box; the global one stays
- a colour with no content is stored, draws no box of its own, and recolours the
  global box
- a colour that is not a colour falls back to inherit rather than rendering
  nothing
- clearing it puts the global colour back

The assertions read the *computed* colour out of the shadow root, not the
payload: the payload being right and the screen being wrong is the failure mode
worth catching.
