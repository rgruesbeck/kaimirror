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
the composited display to a file — a PNG, or an uncompressed RGB565 frame if
the path ends in anything else:

```
gfxdebugger -c screencap [-d 0|1] -p /data/local/tmp/frame.png
gfxdebugger -c screencap [-d 0|1] -p /data/local/tmp/frame.raw
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
./kaimirror.py --format png view      # PNG stream: 12x less bandwidth, slower
./kaimirror.py --no-device-guard view # ~8.8 fps, torn frames become possible
```

`view` and `record` stream uncompressed RGB565 by default; `shot` always uses
PNG. See [Performance](#performance--read-this-before-expecting-scrcpy).

Key names: digits `0`–`9`, `UP` `DOWN` `LEFT` `RIGHT`, `OK`, `BACK`, `MENU`,
`CALL`, `STAR`, `POUND`, `SOFT_LEFT`, `SOFT_RIGHT`, `POWER`, `VOLUMEUP`,
`VOLUMEDOWN`, `CAMERA`.

## Performance — read this before expecting scrcpy

**~6.5 fps streaming, ~8.8 with `--no-device-guard`.** This is a fast
slideshow, not smooth video. It is fine for watching a UI flow, debugging a
layout or recording a repro; it is not fine for anything motion-heavy.

| Path | fps | notes |
|---|---|---|
| `view` / `record`, raw (default) | **6.5–6.6** | flat, whatever is on screen |
| `record --no-device-guard` | **8.8** | torn frames become possible |
| `record --format png` | 6.4 | decays as the screen gets busier |
| `record --display 1` (128x160 cover) | 7.1 | geometry read from the frame header |

### The two output formats

b2g picks the format from the file extension, and it offers exactly two
choices: a path ending in `.png` gets a PNG, and **any** other extension gets
an uncompressed RGB565 dump. There is no JPEG encoder anywhere on this path —
`.jpg`, `.jpeg`, `.bmp` and `.webp` all produce the same raw bytes.

```
.png                        -> 89504e47   PNG
.jpg .jpeg .bmp .webp .raw  -> f0000000   16-byte header + w*h*2

header: f0 00 00 00  40 01 00 00  04 00 00 00  01 00 00 00
           w=240        h=320       format=4     planes=1
```

Streaming defaults to raw because it skips the PNG encode entirely:

- **The frame cost stops depending on the screen.** Raw sits at ~6.8 fps on
  anything; PNG measured 6.40 fps on a 12 KB screen and 6.06 on a 23 KB one,
  and keeps sliding as screens get busier.
- **The completeness guard gets cheap.** Frame size is constant, so the guard
  is one `stat` rather than a three-fork `tail | od | tr` pipeline.
- **The header is a sync marker and reports geometry**, so the host splits
  frames for free and the 128x160 cover display needs no special casing.

The costs: **~12x the bandwidth** (~1.3 MB/s, fine over USB where adb does
9 MB/s, painful over adb-on-wifi — use `--format png` there), and RGB565
colour. Raw is *not* pixel-identical to the PNG: only 0.2% of pixels match
exactly, mean error ~0.5–6 per channel, PSNR 35.5 dB. Indistinguishable on
flat UI, but `shot` always asks for PNG, where fidelity matters and throughput
does not.

### Where the time actually goes

The bottleneck is **process startup**, not capture, not encoding, not flash.
Every external command on this device costs ~34 ms:

| | ms/call |
|---|---|
| shell builtin (`true`) | 1.4 |
| `stat` / `od` / `tail` (toybox) | 33.7 / 35.8 / 33.9 |
| `gfxdebugger`, usage only (no capture) | 34.5 |
| `gfxdebugger -c screencap` | 75.0 |

So the capture itself is only ~41 ms of that 75 ms — the rest is fork, exec
and dynamic linking. **The real capture ceiling is ~24 fps, not 12.** This is
also why switching format buys less than the 12x data reduction suggests:
three or four forks per frame still cost ~136 ms whatever the format.

An earlier serial design ran six external commands per frame and spent ~205 ms
of every 314 ms frame in process startup alone (3.2 fps). The pump now
pipelines instead: the capture for frame N+1 is issued *before* frame N is
shipped, so b2g's write overlaps the transfer.

Two more things that cost real time to discover:

- **A raw frame is bigger than the pipe buffer.** Writing 153 KB to ffmpeg
  inline blocks until it drains, and while blocked the host is not draining
  adb, which stalls the device pump — that alone cost 6.6 fps -> 4.7. The host
  feeds the sink from a writer thread.
- The screen must be awake. A blanked panel captures as a valid 303-byte solid
  black PNG, which looks like success. `view`/`record`/`shot` tap power first
  unless you pass `--no-wake`. Note power *toggles* — if the panel was already
  lit, `wake` turns it off.

### The race this design defends against

`gfxdebugger` returns as soon as b2g **accepts** the request; b2g writes the
file asynchronously. The reply is a 4-byte parcel: `0` = accepted, `1` =
rejected. `0` does *not* mean the file was written — capturing to an
unwritable path also returns `0`. Only a bad display id is rejected
synchronously.

Measured back to back, **23 of 40 PNG frames were still incomplete when
`gfxdebugger` returned** (with a pause between captures, none were). So the
device waits for the frame to be complete — `IEND` for PNG, the expected size
for raw — then renames it away before sending, so a late writer can never
truncate the copy in flight.

`--no-device-guard` drops that wait and lets the host resync on the frame
header instead. It measured clean (zero torn frames in 258 frames under
continuously changing content) because the pipeline leaves b2g plenty of
slack, but the guarantee is timing, not structure.

## Making it faster

Dead ends, all tested on the device:

- **JPEG**: does not exist on this path. `gfxdebugger` has no format strings
  at all; b2g gives you PNG or raw.
- **Let b2g write straight into a FIFO**, skipping the file: rejected
  synchronously (`result: 1`, zero bytes). b2g requires a regular file.
- **Stage frames on tmpfs** instead of flash: `mount` is denied even as root
  under SELinux, and it would not have helped — flash was never the cost.

What is left is the scrcpy-shaped fix: push a small native binary that speaks
`/dev/socket/gfxdebugger-ipc` directly and streams frames over a socket. The
win is not skipping flash or compression, it is collapsing every per-frame
fork into one long-lived process. That needs an NDK cross-compile and the IPC
parcel format (`gfxdebugger` is a 15 KB stripped ARM32 binary using
`android::Parcel` `writeUint32`/`writeCString`/`readUint32` over a unix
socket). Expect something near the ~24 fps capture ceiling if done.

## Files

- `kaimirror.py` — host-side CLI and frame-stream reader
- `kaimirror_device.sh` — device-side frame pump (pipelined), pushed to `/data/local/tmp`
