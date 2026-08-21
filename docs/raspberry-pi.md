# Raspberry Pi quirks

Everything here was measured on hardware rather than inferred. Where a number is
quoted, it came off a device.

## The build

`armv7-unknown-linux-gnueabihf` through `cross` works, including the parts that
looked risky: `ring`, `rcgen` and `qrcode` all compile, and the binary generates
its own certificate on the device. It links against nothing but glibc, libgcc,
librt, libpthread, libm and libdl.

Verified on a Raspberry Pi 2 Model B, Raspbian bookworm, glibc 2.36, built against
an older glibc in the cross image. That direction is the safe one.

`rustls` is pinned to `ring` rather than the default `aws-lc-rs` precisely because
of this target. The same reasoning kept `libpulse-binding` out of the audio code.

## The "translate this page?" bubble

Not suppressible by a command-line flag on Linux, and there is no CDP command for
it either. Three flags look like they should do it and do not: `--disable-translate`
and `--disable-infobars` are ignored outright by current Chromium, and
`--disable-features=Translate` *is* applied — child processes inherit it — but does
not gate the bubble.

The prompt appears because the profile's accepted languages do not include the
language of the page. So the fix is the language list, not a translate switch.

Because the controller owns the launch, it rewrites the profile before **every**
start: `intl.accept_languages` is set from `--browser-language`, and
`translate.enabled` to false. Measured: with those written, `navigator.languages`
follows and Chromium keeps both keys when it rewrites the file. Rewriting every
time is also what makes it survive a profile directory that is wiped on boot,
which is why a profile preference used to be useless here.

The managed policy remains the right answer when the browser is started **outside**
this controller. It needs root, at
`/etc/chromium/policies/managed/no-translate.json`:

```json
{ "TranslateEnabled": false }
```

Verify with `chrome://policy`: the row must read
`TranslateEnabled / false / Platform / Machine / Mandatory / OK`.

## What a Pi 2 cannot do: 1080p WebRTC

A cast of 1920x1080 to a Pi 2 Model B behaves like this:

```
t+0s    1920x1080  24 fps  rtt 9 ms      load 0.44
t+32s   1920x1080  20 fps  rtt 1174 ms   540 packets lost, jitter 0.125
t+34s   1920x1080   5 fps                652 frames dropped
t+96s   1920x1080  19 fps                1335 frames dropped, load 5.6
t+111s  sender declares the connection dead
```

What did **not** happen is as informative as what did: no out-of-memory kill, no
browser crash, not a single error in the log, CPU peaking at 72 %, ICE reporting
`connected` to the last sample. The device did not fall over — it fell behind, and
never caught up, until the *sender* gave up on it.

That is `maintain-resolution`, Chrome's default for screen content, working exactly
as specified: it held the resolution and spent the framerate. Packet loss and
jitter are the only feedback WebRTC carries and they were not enough to converge,
because the bottleneck was decode rather than the network.

The fix is a ceiling. `--cast-max-edge 1280` is 44 % of the pixels and holds.

### The lever deliberately not pulled

That Pi has a **hardware H.264 decoder** (`/dev/video10`–`14`,
`vcgencmd codec_enabled H264` reports enabled) and no hardware VP8 or VP9, while
Chrome tends to pick VP8 for screen shares. Preferring H.264 through
`setCodecPreferences()` on the display might move decoding into silicon entirely.

It was left alone for two reasons. It is unverified whether Chromium on Raspbian
wires up V4L2 decode for **WebRTC** at all — the `/usr/bin/chromium` wrapper sets
no decode-related flags, and `chrome://gpu` times out on that hardware. And one
slow device is not enough evidence to bend a codec default around.

Worth revisiting if a second receiver shows the same pattern: cheap display
hardware generally has H.264 in silicon and not VP9, so this may yet turn out to
be a general win rather than a Pi special case.

## mDNS

`avahi-daemon` announces `<hostname>.local` and nothing else. A custom name needs
`avahi-utils` for `avahi-publish`, which a stock Raspberry Pi OS image does not
have — or simply set the hostname to the name you want, and then the daemon
publishes it for free and the controller correctly does not try to.

A device that publishes with Avahi but resolves through systemd-resolved may fail
to look up its **own** published names. `avahi-resolve` finds them, `curl` on the
same box does not, and a guest on the LAN does. Only the last one matters, but the
local failure looks alarming while testing.

Changing the hostname needs `/etc/hosts` changed too — `hostnamectl` does not touch
it, and a stale `127.0.1.1` entry makes every `sudo` pause with
`unable to resolve host`.

## Slowness that is not a fault

- The first `CREATE TABLE` on an SD card took **4.4 seconds**. It is logged as a
  slow statement and is harmless, but the first start is visibly unhurried.
- Chromium's first launch on a 900 MHz Cortex-A7 takes a while. Give it a minute
  before concluding something is broken.

## No sound server

A stock image has `alsa-utils` and no `pactl`, and no sound server running. The
audio backend reports itself unavailable and the guest page hides the panel,
rather than showing dead controls. That is what the backend seam is for; an ALSA
implementation has deliberately not been written, because no device has needed it.
