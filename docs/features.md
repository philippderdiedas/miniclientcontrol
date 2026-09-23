# What it does

## Playlists

A **playlist** is a named list an operator creates, and every playlist item
belongs to one. A screen is assigned a playlist, so two screens can show
different things — the foyer showing opening hours while the workshop shows
machine status — and two screens assigned the *same* playlist mirror it, which
falls out for free rather than being a feature of its own.

A playlist is a first-class object rather than a column on an item, and the case
that decided it is the unglamorous one: a screen is taken away for good and the
remaining screen should take over what it was showing. As an object that is a
dropdown; as a column it would be a data migration. It follows that **a playlist
is never deleted with a display** — it outlives the screen, unassigned, until
somebody picks it up again.

Deleting a playlist that still holds items is refused, and the refusal says how
many are in the way. The alternative is items left belonging to nothing, which no
screen would ever play and nothing would report.

A device upgraded from a version before playlists existed finds its items in a
playlist called `Standard`, assigned to its one screen, and plays exactly what it
played the day before.

## Items

The unit of work is a **playlist item**: either an uploaded asset or a URL, with
a duration, an optional date window, and an enable switch. The controller walks
the active items of its display's playlist in `play_order`, navigating one
Chromium page from one to the next.

"Active" means enabled *and* inside its date window, which is how a notice can be
scheduled to appear on Monday and stop mattering on Friday without anyone
touching it again.

Editing the playlist takes effect immediately, and playback resumes at the item
after the one that was on screen rather than restarting from the top — on a device
whose playlist is touched during the day, restarting from item one means the later
items never play.

**Play now** jumps to a chosen item. **Keep loaded** holds an item in its own
background tab so a heavy dashboard is already rendered when its turn comes.

An item can be **moved to another playlist**, which appends it to the end of the
target and renumbers both lists. It is its own button rather than part of saving
the card: the move decides the item's new position, so a move carrying other
edits is refused — and an item that has just left the list it was being edited in
should say so in one place rather than half-save in two.

## Several screens

One controller can drive more than one screen. Each declared display gets its own
Chromium, its own control loop and its own assigned playlist, while the asset
library, the settings, the overlay configuration, the credentials, the audio and
the webhook targets stay shared — duplicating *configuration* was never the
complaint. What made two controllers painful was uploading the same PDF twice,
remembering which screen was on which port, and having two admin pages to choose
between before any edit.

**The screens are declared, not discovered**: `--display foyer --display
werkstatt`, once per installation. Placing the windows stays the window manager's
job, which is where this project already puts it — the controller only gives each
browser a window class of its own to match on. See
[deployment.md](deployment.md#declaring-the-screens-a-deployment-drives).

On the operator's side a display is a card on `/displays.html`: a label they can
change, and a dropdown for the playlist it plays. A screen with no playlist shows
the idle page rather than erroring, and a screen that was removed from the command
line keeps its row so that its playlist can be handed to another one.

Playback is per screen and so is the override, so `/playlist.html` has a **screen
picker** beside its playlist picker, and everything in its status bar — what is
running, the override, **Play now** — is about the selected screen while the list
below is about the selected playlist. The two are independent, because an
operator routinely edits a playlist no screen is currently showing, and the bar
says so when they point at different things. The picker is remembered per browser
and disappears when there is only one screen to choose. `/admin.html` answers the
same question for all of them at once: one line per declared screen, naming what
it plays and whether an override is on it.

That the API grew `/api/displays/{name}/…` paths for both follows from the same
split, and the pages use them always — a page with one code path for one screen
and another for several is a page that only works on whichever the author had.
The old unscoped paths still work while one screen is declared, and answer `409`
naming the declared screens once several are — an existing script gets told it
has become ambiguous rather than having a coin flipped for it.

Casting is per screen as well: every declared screen has its own session, a guest
says which one they mean by scanning the panel in front of them, and two guests
can cast to two screens at once. See
[casting.md](casting.md#which-screen-a-cast-lands-on).

## Assets

Uploads land in `--assets-dir` and are served from `/uploads/`. Images, videos
and PDFs are what the feature exists for. A PDF is rendered by a bundled pdf.js
viewer rather than handed to Chromium's own, so scrolling can be driven; an image
or a video is drawn by a small page of ours for the same kind of reason —
Chromium's own image and video documents have a layout nothing can change and a
video control bar nothing turns off.

Each playlist item that plays an image or a video says **how it sits on the
screen**: *Einpassen* (whole picture, with bars), *Füllen* (fills, crops),
*Strecken* (fills, distorts), *Original* (1:1) or *Scrollen* (full width, for a
tall image the scroll modes then move). The colour of the bars is the item's too.
A video loops until its item's duration is up.

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

The badge does not disappear between items. The controller hands the next page
its overlay before navigating to it, so the badge is there from the page's first
frame — and stays through a page that reloads itself. A page with a strict
content-security policy can refuse that, and on such a page the badge comes back
a moment after the page does, as it always has.

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
is checking. With several screens declared, one operator setting decides whether
that address names the panel the badge is on or opens the chooser — one decision
for the venue, so two panels cannot advertise differently.

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

With more than one screen, the guest also says **which** one. Scanning the QR on
the panel in front of them is that answer and costs no taps; a typed address gets
a list of the screens instead, each marked free or busy, and a bound guest can
still switch to another screen before sharing anything. Two screens can be cast to
at once, and the operator stops either one from the admin page without touching
the other.

While a cast is running, the guest can also control the **room's audio** —
per-stream and per-output volume, mute, and output device — because the person
presenting is the person who needs it. That permission starts and ends with the
cast, and is bound to the address actually casting rather than to the LAN at
large: turning the speakers up is a physical act in a shared room. There is one
speaker pair however many screens there are, so the first cast to touch it holds
it, and a second screen's guest is told which screen has the sound rather than
finding their control does nothing.

The operator has the same panel on the admin page, through a door of their own,
and theirs works whether or not anybody is casting. Both end in the same code, so
the guest's knobs and the operator's cannot drift apart. A device with no sound
server shows no panel at all rather than a set of controls that do nothing.

## Operator surface

`/admin.html` shows what is on the screens — one line per declared display, with
its playlist and any override — what is casting, and the runtime settings:
whether casting is allowed, how a guest authenticates, the overlay, the language
for dates and times, and the credentials for the operator UI itself. It also
carries the room-audio panel and, while one is alive, the **pairing code the
display is currently showing** with its remaining seconds — otherwise the person
helping a guest over the phone is the only one who cannot see it.

`/displays.html`, linked from there, is the one page about screens: one card per
declared display with its label and the playlist it plays. `/playlist.html` opens
with a playlist picker, and everything below it edits the playlist that is
selected; the selection is kept in the URL, so a link to one playlist is a link
somebody can send. Its screen picker is the other half of that, and is kept in
the browser rather than in the URL — which screen you are standing in front of is
not part of what a link about a playlist means.

Settings live in the database, so they survive restarts. Anything passed on the
command line pins that setting and the UI shows it as locked — which is also the
way back in if the operator password is ever forgotten.

## Webhooks

The controller knows things nobody else does: a cast started, the display browser
died and came back, the playlist has been empty since Tuesday. All of it used to
live in a log on a device in a corridor. Webhooks are the way out: the controller
**POSTs to URLs the operator configures** when something happens, so a Discord
channel gets a line when a guest puts a page on the foyer screen and a monitoring
system gets a ping when the display drops.

Targets are a list of rows on `/webhooks.html`, not a setting — each has a name,
a URL, a method, custom headers, the events it wants, and an optional body
template. They are never configurable from the command line: a URL alone would
fit a flag, but per-target headers, event selection and a template do not.

Ten events, in four families:

| Event | Carries |
|---|---|
| `playback.item_changed` | `item_id`, `kind` (`asset`/`url`), `title`, `url`, `duration` |
| `playback.playlist_empty` | — |
| `override.set` | `url`, `source` (`operator`/`cast`/`guest_page`) |
| `override.cleared` | `source` |
| `cast.started` | `sender_ip`, `mode` |
| `cast.ended` | `reason`, `duration_secs` |
| `guest_page.shown` | `url`, `sender_ip` |
| `guest_page.ended` | `reason`, `duration_secs` |
| `display.disconnected` | `error` |
| `display.connected` | `reconnect` |

Every one of them arrives in the same envelope — `event`, `timestamp`, `device`
(the machine's hostname), `display` (the screen the event is about), and a `data`
object with the fields above. A target with no template gets exactly that, as
JSON, which is what most receivers want. `display` sits in the envelope rather
than in one event's `data` because every one of the ten is about a particular
screen, and it is additive: a target configured before several screens existed
keeps working and simply receives one more key.

**With a template, the envelope is the context.** The body is a
[minijinja](https://docs.rs/minijinja) template rendered against it, so a Discord
target and a Telegram target can each be given the shape they insist on without
the controller knowing anything about either. Header *values* are templates too,
which is enough for a receiver that routes on the event name in a header. The
admin page offers the selected events' fields as clickable chips, and a chip
always inserts `{{ data.x | tojson }}` — the `| tojson` is not decoration, it is
what keeps a URL containing a quote, or a plain `true`, from producing invalid
JSON.

**Delivery is one attempt and no retry.** These events are ephemeral status, not
a ledger: a `cast.started` redelivered four minutes later, after the cast ended,
is worse than never sent — and a durable queue would put an SD-card write on the
path of every event, on a device where the card is the component that dies. The
result of the last attempt per target is shown on the page, in memory only.
**Testen** renders a target against a sample event and delivers it for real,
because a dry run proves the template compiles and nothing about whether Discord
accepts it.

Nothing a target does can reach the display. A receiver that hangs, answers `500`
or redirects elsewhere costs the playlist nothing, and a redirect is refused
rather than followed — see [deployment.md](deployment.md#a-webhook-target-may-hold-somebody-elses-secret).
Internal receivers on self-signed certificates are this project's normal world,
so there is a per-target switch for skipping certificate verification; it is
per-row and visible rather than global, because a target holding an API token
must not skip it by accident.

`playback.item_changed` is the chatty one: a ten-second playlist fires it 360
times an hour. It is opt-in like everything else, and the page says so beside the
checkbox rather than leaving the operator to find out from their receiver's bill.

**`display.disconnected` reports the CDP connection, not that anything is being
painted.** It fires when the controller loses its DevTools session and has to
reconnect, which is a real and common failure — but it is not a frozen-screen
detector, and using it as one is relying on exactly the wrong signal. Measured on
a Raspberry Pi 3 whose V3D GPU wedged: the kernel reset it once a second, the
compositor sat blocked in `vc4_wait_for_seqno`, and the screen showed the same
frame for thirteen hours while CDP answered every request normally and both page
targets were present. No disconnect was detected, because nothing had
disconnected.
