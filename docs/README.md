# Documentation

`miniclientcontrol` is a single-binary digital-signage controller that runs **on
the display device itself**. It serves an operator UI and a JSON API, drives a
local Chromium over the Chrome DevTools Protocol, and lets a guest on the LAN
share their screen to the display over WebRTC.

| Document | What it covers |
|---|---|
| [features.md](features.md) | What the thing does, from the operator's and the guest's side |
| [architecture.md](architecture.md) | How it is put together, and the invariants that hold it together |
| [casting.md](casting.md) | The screen-cast subsystem in detail: signaling, auth, quality |
| [deployment.md](deployment.md) | Building, shipping, service files, declaring the screens |
| [raspberry-pi.md](raspberry-pi.md) | Device quirks, measurements, and what they cost |
| [troubleshooting.md](troubleshooting.md) | Symptoms, what they mean, and how to tell them apart |
| [roadmap.md](roadmap.md) | What is still missing, in order, and the decisions each item needs |

`CLAUDE.md` in the repository root is a different kind of document: a list of
traps and invariants for whoever edits the code next. It overlaps with these
files on purpose, but it is written to prevent regressions rather than to
explain the system.

## Fastest possible start

```bash
cargo run --release
```

That is enough. The controller finds Chrome or Chromium, starts it in kiosk mode
with the flags it needs, and serves:

- `https://<device>/` — the page a guest opens to share their screen
- `https://<device>/admin.html` — the operator UI
- `http://127.0.0.1:3000/` — the same thing over plain HTTP, loopback only,
  which is what the display browser itself uses

On a private network `<device>` is a name derived from the LAN address, such as
`192-168-178-15.clientctrl.cc`, and the certificate for it is real — no warning
page. Where that cannot be arranged the device falls back to its bare address on
port 3443 with a self-signed certificate, and a browser warns once. Port 443 is
used when the process is allowed to bind it, which is a
[one-line deployment step](deployment.md#giving-the-controller-port-443); without
it the address carries `:3443`.

There is no other setup.
