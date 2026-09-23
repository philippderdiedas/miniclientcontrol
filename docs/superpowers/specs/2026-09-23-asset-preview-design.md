# Asset preview on hover

**Status:** designed
**Date:** 2026-09-23

## What and why

The operator pages name an asset by id and file name — `Asset #12 – 4f1c…_plakat.png`
on a playlist card, `#12 – plakat.png` in every asset picker. Telling two posters,
two clips or two handouts apart means opening each one in a new tab.

## Decisions taken

1. **Rendered in the page from the file itself.** An image is the image, a video
   its first frame through a `<video>` element, a PDF its first page through the
   vendored pdf.js. Nothing is generated or stored on the device — no thumbnail
   files on an SD card, no route, no upload step. Server-side thumbnails would
   cover images only without ffmpeg or a PDF renderer; browser-made thumbnails
   uploaded beside the asset would add files, a storage rule and a route for lists
   of a handful of assets.
2. **Where:** the asset table (`assets.html`: id and file name), the playlist card
   head (`playlist.html`, asset items), and a small thumbnail **beside every asset
   picker** — card, add form, override, the item overlay's image, and the global
   overlay's image in `admin.html`. A native `<select>` cannot show anything while
   its options are open, so beside a picker the thumbnail shows the *chosen*
   option and follows every change.
3. **The status line in `admin.html` waits** for the frozen-screen detection: its
   periodic screenshot is the right preview of what a screen *shows*, which an
   asset's file is not (an override, a page, the overlay on top).

## `web/preview.js`

One script, included by `assets.html`, `playlist.html` and `admin.html`, exposing
`globalThis.Preview`:

- `Preview.hover(element, asset)` — on `mouseenter` or `focus`, a floating box
  (at most 320×240) appears below the element (above it near the bottom of the
  window) and disappears on `mouseleave` / `blur`. The element gets `tabindex=0`,
  so keyboard focus and a tap on a touch screen show it too. Returns the element.
  An asset of any other kind is left untouched.
- `Preview.follow(select, lookup)` — a 64×40 thumbnail for whatever `select`
  has chosen; `lookup(value)` returns the asset or nothing. Updates on `change`;
  returns `{ element, refresh }`, where `refresh()` is for a caller that set the
  value in code (which fires no `change`) or rebuilt the options. The caller places
  `element`. It renders only once it is visible (`IntersectionObserver`), so a
  long playlist does not render a PDF per card up front.

Rendering by kind, shared by both:

- image: `<img loading="lazy">`
- video: `<video preload="metadata" muted>` with `#t=0.1`, so the frame shown is
  the first picture rather than black
- PDF: pdf.js is loaded on the first PDF preview only (it is ~300 KB), its worker
  from `/pdf.worker.min.js` — never a CDN, the device is often offline. Page 1 is
  rendered once per asset per page load, at 480 px on its long side, and cached as
  a data URL, so the hover box and the thumbnails never render it twice.
- anything else: nothing.

Built with `createElement`, the path through `encodeURIComponent`, only
`/uploads/…` from the controller itself — the same rules `media_viewer.html`
follows.

## Testing

In `tests/cast/test_media.py`'s browser half, in the operator pages' own Chrome
(CDP 9253):

- `assets.html`: hovering an image's file name shows an `<img>` on its
  `/uploads/…` path; a video's shows a `<video>`; a PDF's shows its first page —
  checked by drawing it to a canvas and finding the blue of the test PDF's
  rectangle, not by its size alone.
- `playlist.html`: hovering an asset card's head shows the preview; choosing a PDF
  in the add form makes the thumbnail beside it show that PDF's first page.

## Not in scope

- Previews while a `<select>` is open (the browser offers no hook).
- The status line in `admin.html` (decision 3).
- Thumbnail files, a thumbnail route, server-side rendering.
