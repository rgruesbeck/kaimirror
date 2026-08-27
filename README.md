# kaimirror — scrcpy-style screen mirroring for KaiOS 3.x

Live mirroring, recording, screenshots and key injection for a KaiOS device
over adb. Tested on a **TCL Flip 3 (`T435SP` / `Gflip7_VZW`), KaiOS 3.x**.

## Why scrcpy itself cannot work here

KaiOS runs Gecko (`b2g`) directly on the Android HAL. The device has an
Android 10 (SDK 29) base — `init`, binder, `dalvik.*` props, apexd — but
**no Android framework and no SurfaceFlinger**:

```
$ adb shell service list        # 35 services, no "SurfaceFlinger", no "window"
$ adb shell ps -A | grep -i surfaceflinger   # nothing; b2g composes via hwcomposer directly
```

scrcpy's server needs `android.jar`, `SurfaceControl.createDisplay()` and a
`MediaCodec` encoder bound to a virtual display. None of that exists.

Things that look promising but are dead ends:

| Approach | Result |
|---|---|
| `screencap` / `screenrecord` | Ship on the device but **hang forever** — they block on a SurfaceFlinger binder that never registers. |
| `/dev/graphics/fb0` | Exists (240x640 virtual, RGB565, stride 512) but `read()` returns `ENODEV` — MDSS composites through overlay planes, the fb is not the scanout source. |
| tmpfs for frame staging | `b2g` is SELinux-confined (`Enforcing`) and can only write under `/data/local/tmp`. |
| Gecko DevTools socket | `/data/local/firefox-debugger-socket` is live (`devtools.debugger.remote-enabled=true`, no connection prompt) — a viable alternative route, not used here. |

## What actually works

`/system/bin/gfxdebugger` asks `b2g` over `/dev/socket/gfxdebugger-ipc` to dump
the composited display to a PNG:

```
gfxdebugger -c screencap [-d 0|1] -p /data/local/tmp/frame.png
```

`-d 0` = primary 240x320 panel, `-d 1` = 128x160 cover display (`st7735s`).
Input injection goes through `sendevent` on the raw `/dev/input` nodes.
Together these are the capture + control primitives scrcpy would otherwise get
from the framework.

## Requirements

- `adb root` — the build is `userdebug` with `ro.debuggable=1`, so this works.
- `ffmpeg` / `ffplay` on the host (the viewer is an ffplay window).
- Python 3, stdlib only.

## Usage

```sh
./kaimirror.py view                  # live mirror window (2x, nearest-neighbour)
./kaimirror.py view --scale 3
./kaimirror.py record out.mp4        # Ctrl-C to finalize
./kaimirror.py shot screen.png
./kaimirror.py key DOWN OK           # inject key presses
./kaimirror.py wake                  # tap power so the panel is lit
./kaimirror.py --display 1 shot cover.png
```

Key names: digits `0`–`9`, `UP` `DOWN` `LEFT` `RIGHT`, `OK`, `BACK`, `MENU`,
`CALL`, `STAR`, `POUND`, `SOFT_LEFT`, `SOFT_RIGHT`, `POWER`, `VOLUMEUP`,
`VOLUMEDOWN`, `CAMERA`.

## Performance — read this before expecting scrcpy

**~6 fps end to end on flat UI, less on busy screens.** This is a fast
slideshow, not smooth video. It is fine for watching a UI flow, debugging a
layout or recording a repro; it is not fine for anything motion-heavy.

| Screen content | PNG size | Observed |
|---|---|---|
| App grid / menus / mostly-flat UI | ~11–31 KB/frame | **~6.0–6.4 fps** |
| Animated lock-screen wallpaper | ~66 KB/frame | lower — encode and transfer both scale with frame size |

### Where the time actually goes

The bottleneck is **process startup**, not capture, not PNG encoding, and not
flash. Every external command on this device costs ~34 ms:

| | ms/call |
|---|---|
| shell builtin (`true`) | 1.4 |
| `stat` / `od` / `tail` (toybox) | 33.7 / 35.8 / 33.9 |
| `gfxdebugger`, usage only (no capture) | 34.5 |
| `gfxdebugger -c screencap` | 75.0 |

So the capture itself is only ~41 ms of that 75 ms — the other ~34 ms is fork,
exec and dynamic linking. **The real capture ceiling is ~24 fps, not 12.**

An earlier serial design ran six external commands per frame and spent ~205 ms
of every 314 ms frame in process startup alone:

| Stage | ms |
|---|---|
| `gfxdebugger` call | 108 |
| wait-for-`IEND` (even at zero polls — the check itself) | 71 |
| `mv` + `stat` + `cat` | 134 |
| **total** | **314 → 3.2 fps** |

Hence the current pump's shape: **pipeline the capture and remove every fork
that can be removed.** The capture for frame N+1 is issued *before* frame N is
shipped, so b2g's encode overlaps the transfer, and framing moved to the host
so no `stat` is needed. That is 4 forks per frame instead of 6, and it roughly
doubles throughput (3.9 → 6.0–6.4 fps measured end to end).

Two more things that cost real time to discover:

- **Do not busy-spin while waiting for the frame.** A tight poll loop steals CPU
  from b2g's PNG encoder. With the pipeline the wait almost always succeeds on
  the first check, so the `usleep` is now 5000 µs rather than 30000 µs.
- The screen must be awake. A blanked panel captures as a valid 303-byte solid
  black PNG, which looks like success. `view`/`record`/`shot` tap power first
  unless you pass `--no-wake`. Note that power *toggles* — if the panel was
  already lit, `wake` turns it off.

Other measurements: adb transfer is 9 MB/s (`exec-out`) / 30 MB/s (`pull`) —
*not* the bottleneck.

### The race this design defends against

`gfxdebugger` returns as soon as b2g **accepts** the request; b2g encodes and
writes the PNG asynchronously. The reply is a 4-byte parcel: `0` = accepted,
`1` = rejected. `0` does *not* mean the file was written — capturing to an
unwritable path also returns `0`. Only a bad display id is rejected
synchronously.

Measured back to back, **23 of 40 frames were still incomplete when
`gfxdebugger` returned** (with a pause between captures, none were). So the
guard is real and necessary: the device script waits for the PNG `IEND` chunk,
then renames the frame away before sending it, so a late writer can never
truncate the copy in flight.

Wire format is a stream of back-to-back PNGs with no length prefix — length
framing would cost a `stat` fork per frame. The host walks the PNG chunk
headers to find each `IEND`, which also lets it resynchronise on the next
signature instead of dying if the stream is ever damaged.

## Making it faster

Two dead ends, both tested on the device:

- **Let b2g write straight into a FIFO**, skipping the file entirely: rejected
  synchronously (`result: 1`, zero bytes). b2g requires a regular file.
- **Stage frames on tmpfs** instead of flash: `mount` is denied even as root
  under SELinux. It would not have helped anyway — flash is not the cost.

What is left is the scrcpy-shaped fix: push a small native binary that speaks
`/dev/socket/gfxdebugger-ipc` directly and streams frames over a socket. The
win is not skipping flash, it is collapsing every per-frame fork into one
long-lived process. That needs an NDK cross-compile and the IPC parcel format
(`gfxdebugger` is a 15 KB stripped ARM32 binary using `android::Parcel`
`writeUint32`/`writeCString`/`readUint32` over a unix socket). Expect
something near the ~24 fps capture ceiling if done.

## Files

- `kaimirror.py` — host-side CLI and frame-stream reader
- `kaimirror_device.sh` — device-side frame pump (pipelined), pushed to `/data/local/tmp`
