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
a rule for when it moves on, an optional date window, and an enable switch. The controller walks
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

## Layouts

An item can split the screen instead of filling it: a **layout** is a set of
**widgets** on a 24×24 grid, each showing a URL or an asset — two dashboards side
by side, a main page with a bar below, a grid of pictures. The editor on the
playlist page starts from templates (L-shape, main + bar, 50/50, 2×2 …) and lets
each widget be dragged and resized; the canvas is drawn in the real aspect ratio
of the screen the playlist runs on. Each widget scrolls with its own mode.

A dashboard that normally refuses to be embedded (checkmk, Grafana) still shows
in a widget: the controller drops the framing headers and carries the login
cookie across, so the widget stays signed in. This only happens for widgets the
operator put into a layout, never for a page shown full-screen or a guest's cast,
and the login is confined to that layout so no other page can use it. A dashboard
must be reachable over HTTPS to keep its login in a widget. Each widget from
another site is a browser process of its own — a wall of dashboards is light on a
mini-PC and heavy on a Raspberry Pi. Any item, layout or not, can be **duplicated**
from its card. A layout has a **Hintergrund**-Farbe für die Fläche hinter und
zwischen den Widgets (Standard schwarz).

## Built-in widgets

Some content needs neither a URL nor an uploaded file — the controller can draw it
itself. There are four **built-ins**:

- **Uhr** — the time, large and auto-sized: 12/24-hour, seconds on or off, an
  optional date line, and a time zone (blank uses the device's).
- **Banner / Nachricht** — styled text. At **Textgröße** *Automatisch* it is a
  headline that fills the space (for "GESCHLOSSEN", a room name); at a fixed vmin
  size it is body text that wraps (links-, zentriert- oder rechtsbündig), for a
  longer notice. **Überlauf** wählt zwischen Abschneiden und *Lauftext* (ein
  einzeiliger Ticker, der zu langen Text durchscrollt).
- **QR-Code** — for a text/URL you give, or the guest address for screen sharing,
  with an optional caption.
- **Countdown** — counts down to a date and time, as words ("3 T 4 Std") or a
  digital `DD:HH:MM:SS`, optionally with seconds and milliseconds; then shows a
  text you choose.

Bei Uhr, Banner und Countdown ist die **Textgröße** wählbar — *Automatisch*
füllt die Kachel, sonst ein fester Wert in vmin (dieselbe Einheit wie beim
Overlay).

Each has a background colour, a text colour and a **Schriftart** (Sans, Serif oder
Monospace — offline vorhanden). A built-in can be a **widget in a layout**
(pick "Built-in" as the widget's source) or a **whole item on its own** (pick
"Built-in" when adding an item) — a full-screen clock is just a built-in item.

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

### Dayparting

What a screen plays can follow the clock. Each screen has a **default
playlist** and any number of **time windows** — weekdays, from, to, playlist —
in priority order: the topmost window that matches now wins, and the default
covers the rest. "Mo–Fr 08:00–18:00 Büro, sonst Nacht" is one window and a
default. A window may cross midnight (Fr 22:00–06:00 runs into Saturday
morning), and a window ending at 00:00 runs to the end of the day.

At a window boundary the screen switches straight away, mid-item, exactly as it
does when an operator reassigns it by hand. Overlapping windows are allowed —
"lunch beats the working day" is a reasonable thing to want — and the displays
page says which row hides which.

## Assets

Uploads land in `--assets-dir` and are served from `/uploads/`. Images, videos
and PDFs are what the feature exists for. A PDF is rendered by a bundled pdf.js
viewer rather than handed to Chromium's own, so scrolling can be driven; an image
or a video is drawn by a small page of ours for the same kind of reason —
Chromium's own image and video documents have a layout nothing can change and a
video control bar nothing turns off.

Each playlist item that plays an image, a video or a PDF says **how it sits on
the screen**: *Einpassen* (whole picture, with bars), *Füllen* (fills, crops),
*Strecken* (fills, distorts), *Original* (1:1), *Breite füllen* (full width, the
height follows — what a tall image or a PDF scrolls through) or *Höhe füllen*
(full height, centred). The colour of the bars is the item's too. A video loops
until its item moves on.

A PDF defaults to *Breite füllen*, which is how PDFs have always been shown: one
long page after another. Any other choice makes every page one screen, edge to
edge, so the **Step** scroll mode with its default step (one screen height) pages
through the document like a slide show.

### When an item moves on

Each item says **Weiter nach**: either *Zeit* — a number of seconds — or
*Durchläufen* — how many times its content runs through. What one run is follows
from the content: a video played to its end; a page, an image or a PDF with a
scroll mode reached the bottom (a PDF stepping page by page, its last page) and
held there for its bottom delay. After the last run the content stays where it
is — at the bottom, on the last frame — until the next item replaces it. Runs
need something that ends, so they are offered only for a video or with a scroll
mode.

A page that fits the screen is at its end at once; it counts a run per top and
bottom delay, and never faster than three seconds, so a count cannot flash past.
If the content stops moving — a video that stalls, a page whose script never
arrived — the controller moves on after `--advance-stall-timeout` (two minutes by
default) and says so in the log.

A video's real length is measured when it is uploaded and shown on the assets
page; picking the video for a playlist item with *Zeit* fills it in. The video
starts together with the item's clock. *Länge ermitteln* re-measures a video
uploaded before.

Hovering an asset's name — in the asset list, on a playlist card — shows it: the
image, a video's first frame, a PDF's first page. Every asset picker shows a
thumbnail of its choice. It is rendered in the page from the file; nothing is
generated or stored on the device.

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

The box is placed on a **24×24 grid** — the same grid the layout widgets use —
rather than pinned to a fixed corner. Drag and resize it in the editor, or take
one of the presets (the old corners, plus *Mitte*) as a starting point. **The box
fills the rectangle you draw** — what you see in the editor is what stands on the
screen — and the clock, text and QR scale with it, centred inside; a small
rectangle in a corner behaves like the old corner badge, a big one is a big
badge. An item overlay left with no region of its own joins the global box.

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
the thing that became unreadable. An item that shares the global region
recolours that shared box; one with a region of its own recolours only its own.

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
for dates and times, and a link to the accounts. It also
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

## Accounts and roles

The operator pages are behind accounts once the first one exists (until then they
are open, as a device without credentials always was). **Admins** run the system
side — settings, webhooks, accounts, casting, audio. **Managers** change content
directly, pin overrides and approve proposals. **Editors** change content too, but
their changes are *proposals*: collected in a draft, submitted as a bundle, and
applied only when a manager approves it on `/approvals.html`. A bundle is applied
as a whole in order, is refused as stale if something it touches changed in the
meantime, and an editor's upload stays invisible to others until then.

The draft holds one line per object: editing the same item twice updates the
line, deleting something the draft created removes both. Editing an object that
sits in one's own *submitted* bundle pulls that bundle back into the draft rather
than racing it — the manager would otherwise approve a version its author has
already moved past. **Meine Vorschläge** (`/proposals.html`) lists open bundles
(withdraw one to keep working on it) and decided ones with the manager's note;
decided bundles can be hidden, singly or all at once, so the list stays short.
Managers keep the full history. Scripts
sign in with HTTP Basic against the same accounts; the command-line credential is
always the way back in.

## LLM assistants (MCP)

`/mcp` lets an LLM client — Claude Code, Claude Desktop, anything that speaks the
Model Context Protocol — work the device the way a person at the operator pages
would: read what is on the screens, look at them, rearrange playlists, set up a
timetable, upload a picture. It is not a second API. The model reads the API
section of the README and makes ordinary API calls, each replayed through the
same router as the account behind it, so what it may do is exactly what that
account may do. An assistant given an **editor** account proposes, and a manager
approves on `/approvals.html` as for any editor.

It signs in with an **API token**, minted on `/tokens.html` (linked as
*API-Tokens* in the bar at the top of every operator page). The page shows the
token once, with the command that adds it to Claude Code. A token can be given a
lifetime, renamed or re-timed later, is revoked there at once, and dies with its
account. An admin sees every account's tokens on the same page, with their owner,
and can edit or revoke any of them — the token itself is shown to nobody after it
was created. It cannot mint
further tokens or change its account's password — handing an assistant access
must not let it lock out the person who did.

## Single sign-on

With a provider configured on the admin page (any OpenID Connect provider:
Nextcloud, Keycloak, Authentik, …) the login page offers "Mit ‹Anbieter›
anmelden". The provider's groups decide what the person is here: a group mapped
to Admin, Manager or Redakteur gives an account with that role — created on the
first sign-in, updated on every later one, disabled when no mapped group is left.
A group mapped to *Nur Casten* (or "alle vom Anbieter dürfen casten") gives no
account at all, only a signed-in session for casting to screens set to *nur
angemeldet*. Local passwords can be switched off once an admin has signed in
through the provider; the command-line credential keeps working either way.

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

Twelve events, in five families:

| Event | Carries |
|---|---|
| `playback.item_changed` | `item_id`, `kind` (`asset`/`url`), `title`, `url`, `advance` (`{on: "time", seconds}` or `{on: "passes", count}`) |
| `playback.playlist_empty` | — |
| `override.set` | `url`, `source` (`operator`/`cast`/`guest_page`) |
| `override.cleared` | `source` |
| `cast.started` | `sender_ip`, `mode`, `user` (the account, or `null` for a guest) |
| `cast.ended` | `reason`, `duration_secs` |
| `guest_page.shown` | `url`, `sender_ip`, `user` |
| `guest_page.ended` | `reason`, `duration_secs` |
| `display.disconnected` | `error` |
| `display.connected` | `reconnect` |
| `display.frozen` | `seconds` (since the last painted frame), `restarted` |
| `display.recovered` | `seconds` (how long it was frozen) |

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

**That is what `display.frozen` is for.** The controller watches that each
screen is still being *painted*: a page that stops getting frames for
`--freeze-timeout` seconds (default 60) counts as frozen, and a browser the
controller started itself is restarted — once; a second freeze within half an
hour is only reported, so a panel switched off at night does not become a
restart loop. `display.recovered` follows when the screen paints again. Tried
on a real device by stopping its display server for 90 seconds: noticed after
about a minute, both browsers restarted, both dashboards back seconds after the
display server resumed.

## What each screen shows

The admin page's status line, each card on the displays page and the playlist
item that is playing now show a small picture of the screen — what is really on
it, overlay, override or guest page included, not the asset's file. It is taken
only while someone has one of these pages open, at most every ten seconds, and a
frozen screen's picture is marked "eingefroren seit …".
