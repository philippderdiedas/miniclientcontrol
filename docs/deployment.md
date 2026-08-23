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
    --public-url mdns
Restart=always
RestartSec=10
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

## Two displays on one machine

Run one controller per screen. Everything that can collide must differ:

```bash
miniclientcontrol --port 3000 --cdp-url http://127.0.0.1:9222 \
    --chromium-class chrome-1 --database-path .../one.db --assets-dir .../one
miniclientcontrol --port 3001 --cdp-url http://127.0.0.1:9223 \
    --chromium-class chrome-2 --database-path .../two.db --assets-dir .../two
```

`--chromium-user-data-dir` and `--chromium-class` default to values derived from
the CDP port, so they are already distinct, and the TLS port takes the next free
one by itself. Only the HTTP port, the database and the assets directory have to
be spelled out.

Sharing a profile directory is the nasty one: a second Chromium started on a
profile that is already in use hands its URL to the running instance and exits,
taking its debugging port with it — so the second display silently never appears
and nothing looks wrong except that it is not there.

### Placing the windows

That is the window manager's job. `--chromium-class` sets `WM_CLASS`, which i3
matches on:

```
assign [class="chrome-1"] 1
assign [class="chrome-2"] 2
workspace 1 output HDMI-1
workspace 2 output HDMI-3
```

The last two lines matter. `assign` only chooses a workspace; without pinning
workspaces to outputs, i3 decides which screen a workspace lands on, and not
reliably the same way after a restart.

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

On a machine driving two displays only one instance can hold 443. The other falls
back, which works, but only one of the two gets the short address.

## Runtime settings versus flags

`cast_enabled`, `cast_auth`, `cast_code`, the locale, the overlay and the operator
credentials live in the database and are edited from the admin UI. A flag actually passed on
the command line **pins** that setting: the API answers `409` naming the flag and
the UI renders the control as locked.

Besides deferring to whoever wrote the unit file, that is the recovery path — an
operator who enables authentication and forgets the password can always get back
in by passing `--basic-auth-user` and `--basic-auth-password`.

Passwords are stored as PBKDF2 hashes, so a copy of the database is not a copy of
the credentials.
