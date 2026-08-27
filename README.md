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

**~1.5–4 fps end to end, depending on what is on screen.** This is a slideshow,
not smooth video. It is fine for watching a UI flow, debugging a layout or
recording a repro; it is not fine for anything motion-heavy.

The rate is content-dependent because every frame is a full PNG encode plus a
flash round trip, and both scale with how well the screen compresses:

| Screen content | PNG size | Observed |
|---|---|---|
| Animated lock-screen wallpaper | ~66 KB/frame | **~1.6 fps** |
| App grid / menus / mostly-flat UI | ~11–31 KB/frame | **~3.5–3.7 fps** |

Other measurements on the test device:

- capture alone (no transfer): **~80 ms/frame ≈ 12 fps** — the ceiling
- adb transfer: 9 MB/s (`exec-out`) / 30 MB/s (`pull`) — *not* the bottleneck
- fork+exec: ~6 ms — *not* the bottleneck either

The cost is the per-frame round trip through the filesystem: `b2g` PNG-encodes
240x320 and writes it to `/data` flash, then the shell renames, stats and reads
it back. Two notes that cost real time to discover:

- **Do not busy-spin while waiting for the frame.** A tight poll loop steals CPU
  from b2g's PNG encoder and makes things *slower*: 0 µs poll delay → 10 fps
  capture, 30000 µs → 12 fps. Hence the `usleep` in the device script.
- The screen must be awake. A blanked panel captures as a valid 303-byte solid
  black PNG, which looks like success. `view`/`record`/`shot` tap power first
  unless you pass `--no-wake`.

### The two races this design defends against

`gfxdebugger` returns immediately; `b2g` finishes the PNG **asynchronously**.

1. **Truncated reads.** The file grows in 8192-byte chunks, so a naive read gets
   a valid-looking PNG with no `IEND`. The device script waits for the `IEND`
   chunk before touching the file.
2. **Size/content skew.** Stat'ing the size *before* confirming `IEND` pairs a
   partial size with a complete file and desyncs the stream permanently. Size is
   read only after `IEND`, and the frame is renamed away first so a late writer
   can never truncate the copy being sent.

Wire format is `"FRAME" + %010d size + <size> PNG bytes`.

## Making it faster

The ceiling here is the shell pipeline, not the hardware. The scrcpy-shaped fix
is the one scrcpy itself uses: push a small native binary that speaks
`/dev/socket/gfxdebugger-ipc` directly and streams frames over a socket,
skipping the PNG-to-flash round trip entirely. That needs an NDK cross-compile
and reverse-engineering the IPC parcel format (the binary's strings show a
`parcel size` / `received %zu bytes` protocol). Expect roughly the 12 fps
capture ceiling if done.

## Files

- `kaimirror.py` — host-side CLI and frame-stream reader
- `kaimirror_device.sh` — device-side frame pump, pushed to `/data/local/tmp`
