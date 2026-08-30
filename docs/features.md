# What it does

## The playlist

The unit of work is a **playlist item**: either an uploaded asset or a URL, with
a duration, an optional date window, and an enable switch. The controller walks
the active items in `play_order`, navigating one Chromium page from one to the
next.

"Active" means enabled *and* inside its date window, which is how a notice can be
scheduled to appear on Monday and stop mattering on Friday without anyone
touching it again.

Editing the playlist takes effect immediately, and playback resumes at the item
after the one that was on screen rather than restarting from the top — on a device
whose playlist is touched during the day, restarting from item one means the later
items never play.

**Play now** jumps to a chosen item. **Keep loaded** holds an item in its own
background tab so a heavy dashboard is already rendered when its turn comes.

## Assets

Uploads land in `--assets-dir` and are served from `/uploads/`. Images and PDFs
are what the feature exists for; a PDF is rendered by a bundled pdf.js viewer
rather than handed to Chromium's own, so scrolling can be driven.

Each asset carries a default duration, which an item may override.

## Scrolling

A page taller than the screen can be scrolled while it is displayed, in one of
three modes:

- **None** — show the top and nothing else
- **Step** — jump or glide a fixed distance, pause, repeat
- **Continuous** — scroll smoothly, pause at the top and bottom

The runtime is injected into every page, including pages that are not ours. A
site with a strict content-security policy can block it, and then scrolling
silently does not happen — the injection is probed rather than assumed.

## Overlay

An overlay is drawn **on top of whatever is playing**: a line of text, an
uploaded image, a clock, a date, and a QR code. It exists so a display can carry
a permanent caption — a room name, a phone number, the address for screen sharing
— without that being baked into every asset.

There are two layers. A **global** overlay applies to everything, and a playlist
item may add **its own** on top of it. The item's layer is deliberately smaller in
scope (text, image, QR, position) so a display does not change character item by
item while the playlist runs.

An item may also **override the text colour**, which is the one piece of the look
it can reach. The case is a single bright page in an otherwise dark playlist:
white text that reads everywhere else disappears there, and the global setting
cannot fix one item without breaking the rest. The override recolours the whole
box it lands in — including the clock the global overlay draws, which is usually
the thing that became unreadable. An item that shares the global corner
recolours that shared box; one with a corner of its own recolours only its own.

A colour needs no text or QR beside it: a bright page often wants no badge of its
own, only a readable house clock. Everything else about the look — background,
opacity, sizes — stays global on purpose.

The QR code either encodes a fixed string or **the screen-share address**,
resolved at the moment it is drawn. That indirection matters: the address is not
stable — `--public-url` decides its shape, an occupied TLS port moves it, and a
DHCP lease changes it. A copied-out URL would go quietly wrong on a screen nobody
is checking.

Sizes are in `vmin`, so one setting reads the same on a 1080p panel and a
portrait 4K one.

The overlay can **stand down during a cast**, and that is the operator's switch,
not the guest's: the overlay is the venue's own statement, so a presenter cannot
clear the house message — but a venue that would rather not draw on someone's
slides says so once and is done. It is off by default.

Two things happen without anyone deciding them. A QR code whose source is the
screen-share address is dropped while a cast runs, whatever that switch says,
because the slot is taken and whoever scanned it would only be turned away; a
typed QR — a menu, a phone number — still means something and stays. And the
overlay steps aside entirely while the display is showing a **connection code**,
because the overlay draws above everything else on the page, so a badge in the
wrong corner would cover the one thing a guest needs to read.

The clock and date are formatted for a language tag that is settable in the admin
UI or with `--locale`, and which otherwise follows the machine: `LC_ALL`, then
`LC_TIME`, then `LANG`. `LC_TIME` before `LANG` is deliberate — a desktop set up
with English menus and German dates is a common arrangement, and it is the date
setting that applies here. `C` and `POSIX` count as "no locale chosen" and are not
passed on; the display browser's own default is then used, which follows from
`--browser-language`.

That is a different setting from `--browser-language`, which decides what
*websites* are asked to serve.

## Screen casting

By default the device gives guests a name like
`https://192-168-178-15.clientctrl.cc/` and serves a certificate a browser
already trusts, so there is no warning page to click through and no port to type.
It falls back to the bare address with a self-signed certificate when it cannot
arrange that — see [casting.md](casting.md#a-real-certificate-for-a-private-address).

A guest may also simply hand the kiosk a **web address** to open, when the
operator has allowed it — often what somebody actually wanted, and far cheaper
for the device than a video stream. It is a switch of its own, so a display too
weak for casting can still be given a page. See
[casting.md](casting.md#a-guest-showing-a-page).

A guest opens the device's address, optionally enters a code, and shares a screen
or a camera. The display switches to it, and returns to the playlist when the
guest stops, closes the laptop, or walks away. See [casting.md](casting.md).

While a cast is running, the guest can also control the **room's audio** —
per-stream and per-output volume, mute, and output device — because the person
presenting is the person who needs it. That permission starts and ends with the
cast, and is bound to the address actually casting rather than to the LAN at
large: turning the speakers up is a physical act in a shared room.

The operator has the same panel on the admin page, through a door of their own,
and theirs works whether or not anybody is casting. Both end in the same code, so
the guest's knobs and the operator's cannot drift apart. A device with no sound
server shows no panel at all rather than a set of controls that do nothing.

## Operator surface

`/admin.html` shows what is on screen, what is casting, and the runtime settings:
whether casting is allowed, how a guest authenticates, the overlay, the language
for dates and times, and the credentials for the operator UI itself. It also
carries the room-audio panel and, while one is alive, the **pairing code the
display is currently showing** with its remaining seconds — otherwise the person
helping a guest over the phone is the only one who cannot see it.

Settings live in the database, so they survive restarts. Anything passed on the
command line pins that setting and the UI shows it as locked — which is also the
way back in if the operator password is ever forgotten.
