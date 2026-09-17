# Deployment

## Building

```bash
cargo build --release
```

For a 32-bit ARM device (Raspberry Pi 2/3/4 on a 32-bit image), cross-compile with
[`cross`](https://github.com/cross-rs/cross), which needs a running Docker daemon:

```bash
cross build --release --target armv7-unknown-linux-gnueabihf --target-dir target/cross-armv7
```

Give each cross target its **own** `--target-dir`. Host proc-macro shared objects
land in the shared `target/release/build/`, and the per-target images carry
different glibc versions, so reusing one directory across two targets fails with
`symbol getrandom, version GLIBC_2.25 not defined`.

The result links against nothing but glibc, libgcc and libm — SQLite and TLS are
statically linked — so it can be copied to any glibc Linux of the same
architecture. Building against an older glibc and running on a newer one is the
supported direction.

There is no linting configuration. `cargo build` is the gate for the Rust side;
`tests/cast/` holds stdlib-only Python end-to-end tests, not wired into any CI.

## Updating a device that is already running

The binary is being executed, so writing over it in place gets `ETXTBSY`. Copy
beside it and rename over the top, and keep the previous one for a rollback:

```bash
scp <binary> device:~/miniclientcontrol/miniclientcontrol.new
ssh device 'cd ~/miniclientcontrol \
  && cp -a miniclientcontrol miniclientcontrol.bak \
  && mv miniclientcontrol.new miniclientcontrol'
```

The kiosk this was developed against is `pi@10.124.11.124`, and it reports
`uname -m` = `armv7l` — so `armv7-unknown-linux-gnueabihf` is the target that
actually ships, not the `arm-unknown-linux-gnueabihf` also configured in the tree.

Stopping the service first works too and is simpler. The browser is **not** killed
when the controller stops — a deploy should not blank the screen — so the new
process finds the CDP port answering and reattaches to the browser that is already
up.

Beware `pkill -f` over SSH: a pattern like `miniclientcontrol/miniclientcontrol`
matches the remote shell running the command and kills it mid-script, so the rest
never runs and the output is silently empty. Match on the truncated process name
or find the process by the port it holds.

## Service files

The controller starts Chromium itself, so its unit needs the display in its
environment — a unit that only spoke CDP did not:

```ini
[Unit]
Description=miniclientcontrol
After=graphical-session.target

[Service]
Environment=DISPLAY=:0
Environment=RUST_LOG=info
ExecStart=/home/pi/miniclientcontrol/miniclientcontrol \
    --database-path /home/pi/miniclientcontrol/miniclient.db \
    --assets-dir /home/pi/miniclientcontrol/assets \
    --display foyer --display werkstatt \
    --public-url mdns
Restart=always
RestartSec=10
```

Drop the two `--display` flags for a single-screen device: with no `--display` at
all the controller drives one implicit screen exactly as it always did (see
[Declaring the screens](#declaring-the-screens-a-deployment-drives)). This flag
can come from the environment instead, which is the form to reach for when the
unit is generated: `--display` reads **`DISPLAYS`** — note the plural, so the
X11 `DISPLAY` above is not mistaken for a screen declaration — and takes a
comma-separated list.

```ini
Environment=DISPLAYS=foyer,werkstatt
```

Started from the session, typically by the window manager:

```
exec --no-startup-id systemctl --user import-environment DISPLAY XAUTHORITY
exec --no-startup-id systemctl --user start miniclientcontrol.service
```

Make sure the service's working files are writable by the user it runs as. A
directory left owned by root means no database and no certificate, and the failure
reads like something else entirely.

### On Wayland

A sway kiosk needs `WAYLAND_DISPLAY=wayland-1` and
`--chromium-arg=--ozone-platform=wayland` in place of `DISPLAY`. A systemd *user*
service inherits neither from the compositor, **even when sway's own config is
what starts it** — `exec systemctl --user start ...` does not carry the
compositor's environment across.

Without them the failure is close to invisible. Chromium launches, finds no
display server, and exits before it opens the debugging port; the supervisor
relaunches it every ten seconds forever. There is no `DevToolsActivePort` file,
nothing listening on the CDP port, and on an image where journald stores nothing,
no log either. Give the unit `StandardOutput=append:<path>` before trying to
diagnose it.

### Restarting the whole session

On a device where the session is started by an autologin `/bin/login -f`, use
`sudo loginctl terminate-session <id>` rather than restarting the getty
*service*. The session lives in a logind scope that restarting the service leaves
alone: the controller and the browser survive as orphans re-parented to PID 1, the
old controller keeps the port, and the fresh one dies on the bind — leaving the
display running an already-deleted binary. Terminating the session kills the whole
cgroup, orphans included, and autologin brings everything back. Find the id with
`loginctl list-sessions`.

## The browser

The controller finds Chrome or Chromium itself through
`chromiumoxide::detection` (`CHROME`, then `google-chrome-stable`, `chromium`,
`chromium-browser`, and the usual paths) and starts it in kiosk mode with the
flags it needs. `--chromium` pins the binary.

If something is **already listening** on the CDP port, the controller connects to
that instead of starting its own — so an existing deployment that launches
Chromium from a session file keeps working untouched. `--no-launch-browser`
disables starting one entirely.

It also restarts the browser if it exits, which the connect-only arrangement could
not do: the loop just sat there reconnecting.

## Declaring the screens a deployment drives

One controller drives every screen in the venue. Say which:

```bash
miniclientcontrol --display foyer --display werkstatt
```

One HTTP port, one database, one asset library and one admin page; each declared
screen gets its own Chromium, its own control loop and its own assigned playlist.
Which playlist is an operator decision, made on `/displays.html` and stored
against the display's name, so it survives a restart.

Each display derives what running a second controller used to make somebody spell
out:

| | derived from |
|---|---|
| CDP port | `9222 + declaration index`, or the explicit `name:port` form |
| WM class / Wayland `app_id` | `miniclientcontrol-<name>` |
| browser profile | `/tmp/miniclientcontrol-chromium-<name>` |

`--display werkstatt:9300` pins one screen's CDP port and leaves the others
alone: implicit ports still count from 9222 by declaration index, so pinning one
does not shift another. A name is the identity an assignment is stored against and
it reaches both a window-manager config and a filesystem path, so it is restricted
to letters, digits, `-` and `_`. A duplicate name, a duplicate port or a name with
a space in it is a refusal to boot, not a screen quietly missing.

**With no `--display` at all, nothing changes.** One implicit display named
`default`, on `--cdp-url`, with the class and profile derived from its port as
before. These machines run unattended in venues; an upgrade must not move a CDP
port.

### Flags that are refused rather than ignored

Alongside any `--display`, these are startup errors:

- `--chromium-class`, `--chromium-user-data-dir`, and a `--cdp-url` that is not
  the default. The declared branch derives all three from the display's name and
  position and never reads the flags back, so accepting them would be a setting
  that silently does nothing — the failure mode the "a flag actually passed pins
  that setting" rule exists to prevent. Refused for one declared display as well
  as for several, because a rule that changes meaning when a venue adds a second
  panel is a rule that breaks later. The `--cdp-url` check compares the whole
  URL, not the port: `--cdp-url http://192.168.1.5:9222` names a different host,
  and waving it through would silently rewrite it to loopback.
- `--class` smuggled in through `--chromium-arg`. It is appended after the derived
  `--class` on the Chromium command line and Chromium is last-wins for a repeated
  switch, so it would collapse every display onto one `app_id` — precisely the
  placement failure a named display exists to avoid. Without any `--display` it is
  still accepted, because there is no second window for it to collide with and
  refusing would break a command line that works today.

Sharing one profile directory between two Chromiums is the reason the profile is
derived per display at all. A second Chromium started on a profile that is already
in use hands its URL to the running instance and exits, taking its debugging port
with it — so that display silently never appears and nothing looks wrong except
that it is not there.

### Upgrading a device that is already running

**Do not run an older binary against an upgraded database.** The schema migration
is harmless to roll back over — the old code simply ignores the columns it does
not know — but the old *behaviour* is not. Its `add_to_playlist` writes no
`playlist_id` at all, so everything added while it runs lands with a `NULL` one:
no display's loop selects those rows and the playlist page, which lists by
playlist, never shows them. And its `move_playlist_item` reads
`SELECT id FROM playlist_items ORDER BY play_order` across the *whole table* and
renumbers every row it finds to `1..n`, so one arrow click on one screen's
playlist interleaves the order of all of them. The new code anticipates the
first half of that — `ordered_ids_in_playlist` matches the playlist with `IS`, so
`NULL` rows form their own list rather than joining someone's — but nothing
anticipates the renumbering, and nothing can undo it: the order it overwrote is
gone. Keep a copy of the database file before upgrading, and use it if the binary
goes back.

**Declaring displays for the first time: kill the running Chromium, or reboot.**
The controller deliberately does not kill the browser when it stops, and it skips
starting one whenever the CDP port already answers — that is what lets an existing
deployment launch Chromium from a session file. So on a machine that has been
running, adding `--display foyer --display werkstatt` and restarting leaves
`foyer` attached to the *pre-existing* browser on 9222, which was started under
the old class `miniclientcontrol-9222`: the `assign [app_id="miniclientcontrol-foyer"]`
rule never matches it, so that screen is placed by whatever the window manager
would have done anyway. It also leaves that display's `browser_pid` unset, because
the controller did not start the process — and without it the audio panel cannot
single out a cast's own stream on that screen. `werkstatt` spawns correctly on
9223. A reboot resolves it on its own; killing the old Chromium before the restart
resolves it now.

### Placing the windows

Still the window manager's job, which is where this project has always put it.
The controller only makes its windows distinguishable: the display's class becomes
`WM_CLASS` under X11 and the `app_id` under Wayland. Measured on a real sway 1.12
session — a Chromium launched with `--class=screen-A` appears in
`swaymsg -t get_tree` as `app_id: "screen-A"`, and
`[app_id="screen-A"] move container to output …` moves it. Not verified under
GNOME/mutter, which matches `app-id` against `.desktop` files differently.

```
assign [app_id="miniclientcontrol-foyer"]     output HDMI-A-1
assign [app_id="miniclientcontrol-werkstatt"] output DP-1
```

Under i3, `assign` only chooses a workspace, so pin the workspaces to outputs as
well — without that i3 decides which screen a workspace lands on, and not reliably
the same way after a restart:

```
assign [class="miniclientcontrol-foyer"] 1
assign [class="miniclientcontrol-werkstatt"] 2
workspace 1 output HDMI-1
workspace 2 output HDMI-3
```

### Why not discover the screens

Discovery was prototyped against sway 1.12 and it works: `swaymsg -t get_outputs`
lists the connector names, `[app_id="…"] move container to output …` places a
window, and an unplugged output hands its window to another rather than losing it.
It was dropped anyway, for two reasons. It puts compositor-specific knowledge
inside the controller — sway names an output `HDMI-A-1` where i3 says `HDMI-1` —
and it takes window placement away from the window manager. A third screen is a
once-per-installation act, not a daily one, and a flag plus two lines of
window-manager config is the right price for it.

### One process, every screen: what that costs

Two controllers had one genuine virtue, and folding them into one process gives it
up: **a crash now takes every screen, where before it took one.** So does a
restart, and so does a deploy that goes wrong. That is not a detail that turned out
not to matter — it is a cost that was accepted, because the separation charged more
than it paid: the same PDF uploaded twice into two asset directories, two ports to
keep straight, two admin pages to choose between before any edit, and no prospect
of ever sending a guest to a particular screen, since each controller only knew
about its own. That last one is now collected: one controller holds every screen's
cast session, so a guest picks the panel they are standing in front of — see
[casting.md](casting.md#which-screen-a-cast-lands-on).

If it does bite, **the answer is a supervisor that restarts the process, not a
second controller.** `Restart=always` in the unit file (see [Service
files](#service-files)) brings every screen back together; a second controller
brings back the two asset libraries.

## Giving the controller port 443

With no `--cast-tls-port` the listener tries 443 before falling back to 3443, so
the port drops out of the address guests are given. Ports below 1024 are
privileged, and the controller does not ask for anything special — if it cannot
have 443 it takes 3443 and says nothing, because on most machines that is simply
the normal outcome.

Granting it is a sysadmin decision, not something the binary should arrange for
itself. Three ways, roughly in order of how little they change:

```bash
# 1. the capability, on the binary
sudo setcap cap_net_bind_service=+ep ~/miniclientcontrol/miniclientcontrol

# 2. lower the threshold for the whole machine
sudo sysctl -w net.ipv4.ip_unprivileged_port_start=443   # /etc/sysctl.d to persist

# 3. run it as a system service with AmbientCapabilities=CAP_NET_BIND_SERVICE
```

### Why `setcap` needs redoing after every update

`setcap` does not mark a *path*. It writes an extended attribute onto the
**inode** — the file object itself. The update procedure above deliberately
replaces that object:

```bash
scp <binary> device:~/miniclientcontrol/miniclientcontrol.new   # a new inode
ssh device 'cd ~/miniclientcontrol && mv miniclientcontrol.new miniclientcontrol'
```

After the `mv` the name points at the file that was just copied over, and nothing
ever ran `setcap` on *that* one. The capability is gone.

Nothing complains. The controller tries 443, is refused, and falls back to 3443
exactly as designed — while every QR code already printed, scanned or written
down says `https://<name>/` with no port. Guests reach nothing, and the logs read
like an ordinary startup.

So either re-run `setcap` as part of the deploy:

```bash
ssh device 'sudo setcap cap_net_bind_service=+ep ~/miniclientcontrol/miniclientcontrol'
```

or use one of the other two options, which are properties of the machine or the
unit rather than of the file, and therefore survive.

A systemd **user** service cannot be given `AmbientCapabilities`: the user
manager has no capabilities to hand out. That leaves options 1 and 2 for the
usual kiosk arrangement.

A machine driving several displays runs one process and therefore needs 443 once,
however many screens it drives. Two *controllers* on one machine are the case where
only one can hold it — the other falls back, which works, but only one of the two
gets the short address.

## Runtime settings versus flags

`cast_enabled`, `cast_auth`, `cast_code`, `cast_qr_target`, the locale, the
overlay and the operator credentials live in the database and are edited from the
admin UI. A flag actually passed on
the command line **pins** that setting: the API answers `409` naming the flag and
the UI renders the control as locked. `cast_qr_target` has no flag at all, so it
is the one in that list that is always editable.

Besides deferring to whoever wrote the unit file, that is the recovery path — an
operator who enables authentication and forgets the password can always get back
in by passing `--basic-auth-user` and `--basic-auth-password`.

Passwords are stored as PBKDF2 hashes, so a copy of the database is not a copy of
the credentials.

## A webhook target may hold somebody else's secret

A webhook target's `headers` are where an API token goes — a Discord webhook
secret in the URL, a bearer token for a monitoring system, a `X-Api-Key` for a
home-automation hub. That is the first time this controller stores a credential
belonging to a *third party*, and it is worth one paragraph of deployment
thought.

`GET /api/webhooks` returns each target's headers as stored. That is deliberate:
the operator has to be able to see and correct what they typed, and a write-only
field would be inconsistent for no gain against an attacker who is already
authenticated. But **basic auth is unconfigured by default**, and with no
credentials set the operator API is served to anyone who can reach the LAN-facing
HTTPS listener. `CorsLayer::permissive()` additionally makes that response
readable to any website the operator happens to visit from a browser on the same
network.

None of that is new — `GET /api/settings` already returns `cast_code` in the
clear under exactly the same conditions, and the operator surface has always been
"protected when you protect it". The difference is only that a webhook target is
the first thing here that can hold a secret which is not the device's own.

So: **on any device where a webhook target carries a credential, configure basic
auth.** Either from the admin page, or with `--basic-auth-user` and
`--basic-auth-password`, which additionally pins it — see
[Runtime settings versus flags](#runtime-settings-versus-flags). A device with no
webhook targets, or with targets whose URLs are unauthenticated internal
endpoints, is in the same position it was before.

Two related properties, both by design:

- **A redirect is refused, never followed.** A `3xx` is reported with its
  `Location` as the failure it is. Following one would send the target's
  `Authorization` header to a host the operator never configured and cannot see,
  chosen by whoever controls the receiver.
- **The target's URL is fixed, never templated.** A URL assembled from event data
  would be a request to a host chosen by the payload, which is the same problem
  wearing a different hat.
