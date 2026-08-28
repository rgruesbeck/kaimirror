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

`-d 0` = primary 240x320 panel, `-d 1` = 128x128 cover display (`st7735s`).
Input injection goes through `sendevent` on the raw `/dev/input` nodes.
Together these are the capture + control primitives scrcpy would otherwise get
from the framework.

`gfxdebugger` is only a thin client for that socket, though, and starting it
costs more than the capture does. The device-side pump speaks the socket
directly instead — see [the protocol](#the-gfxdebugger-ipc-protocol).

## Requirements

- `adb root` — the build is `userdebug` with `ro.debuggable=1`, so this works.
- `ffmpeg` / `ffplay` on the host (the viewer is an ffplay window).
- Python 3, stdlib only.

For the fast path, the device-side pump also needs building once:

```sh
kaipump/build.sh          # needs Rust + the Android NDK; see below
```

That wants a Rust toolchain with the `armv7-linux-androideabi` target and an
NDK at `$ANDROID_NDK_HOME` (default `~/Android/android-ndk-r27c`); the NDK
supplies only the linker, since the binary is statically linked and carries
its own libc. **This is optional** — without it `kaimirror` falls back to the
shell pump and says so, at roughly a fifth of the speed.

## Usage

`./kaimirror.py --help` lists everything; each subcommand has its own `-h`.

```sh
./kaimirror.py view                     # live mirror window (2x, nearest-neighbour)
./kaimirror.py view --scale 3
./kaimirror.py record out.mp4           # Ctrl-C to finalize
./kaimirror.py shot screen.png
./kaimirror.py key DOWN OK              # inject key presses
./kaimirror.py wake                     # tap power so the panel is lit
./kaimirror.py shot --display 1 cover.png
./kaimirror.py view --control           # drive the phone from the terminal
./kaimirror.py view --format png        # PNG stream: 12x less bandwidth, slower
./kaimirror.py record --fps 10 out.mp4  # lighter on the device, correctly timed
```

The capture options (`--display`, `--no-wake`, and for `view`/`record` also
`--format`, `--fps`, `--control`) work on either side of
the subcommand name, so the older `./kaimirror.py --display 1 shot cover.png`
form still works too.

`view` and `record` stream uncompressed RGB565 by default; `shot` always uses
PNG. See [Performance](#performance).

Key names — `./kaimirror.py key --list` prints them: digits `0`–`9`, `UP`
`DOWN` `LEFT` `RIGHT`, `OK`/`CENTER`, `BACK`, `MENU`, `HELP`, `CALL`/`SEND`,
`STAR`, `POUND`, `SOFT_LEFT`, `SOFT_RIGHT`, `POWER`, `VOLUMEUP`,
`VOLUMEDOWN`, `CAMERA`.

## Performance

**~29 fps streaming.** That is a usable live mirror rather than the slideshow
this started as. The device-side pump is no longer the bottleneck — b2g's CPU
budget is — so the rate is capped by choice, not by capability.

| Pump | fps | notes |
|---|---|---|
| `kaipump`, IPC, capped at 30 (default) | **28.7** | b2g at 16% CPU |
| `kaipump`, IPC, uncapped | 66 | b2g at 74% CPU, 22 MB/s to flash |
| `kaipump`, `exec` backend | ~20 | spawns `gfxdebugger` per frame |
| `kaimirror_device.sh` (fallback) | 6.1 | four forks per frame |

Uncapped is measured but not recommended: it re-captures a screen that is not
changing that fast, and pays most of b2g's CPU and 22 MB/s of flash writes to
do it.

`--fps` is one knob: it caps what the device captures **and** is what the sink
is told. Those used to be separate (`--max-fps` and `--fps`), which meant any
gap between them silently produced a wrong-speed video — 6s recorded at 10 fps
but declared as 30 came out as a 1.7s file playing at 3.5x. There is no way to
express that mismatch now.

Uncapped is no longer reachable from the CLI, because a variable rate cannot
be declared honestly to a container. The pump still does it directly, for
benchmarking:

```sh
adb exec-out /data/local/tmp/kaipump 5000 0 raw 1 150 ipc 0   # 0 = uncapped
```

There is also no `--poll-delay` any more. It set how long the device-side
guard slept between checks for a complete frame; swept from 200us to 5ms it
made no measurable difference, because the pump spends its time waiting on b2g
rather than on the poll, and going much lower only competes for CPU with the
process producing the frames. It is a constant now.

Other paths, all with the IPC pump:

| Path | fps | notes |
|---|---|---|
| `--display 1` (128x128 cover), uncapped | 123 | geometry read from the frame header |
| `--format png` | 12.4 | encode-bound: the cap makes no difference |

### Where the time actually goes

The bottleneck was **process startup**, not capture, not encoding, not flash.
Every external command on this device costs ~34 ms:

| | ms/call |
|---|---|
| shell builtin (`true`) | 1.4 |
| `stat` / `od` / `tail` (toybox) | 33.7 / 35.8 / 33.9 |
| `gfxdebugger`, usage only (no capture) | 34.5 |
| `gfxdebugger -c screencap` | 75.0 |

The shell pump spent three or four of those per frame — ~136 ms of overhead —
and `gfxdebugger` itself is a process like any other. Replacing the loop with
one long-lived binary that speaks b2g's socket directly removes every one of
them.

**An earlier version of this section claimed the remaining 41 ms of that 75 ms
was b2g's capture, and predicted a ~24 fps ceiling. That was wrong.** The IPC
pump sustains 150 captures/second with no process involved, so b2g composites
in single-digit milliseconds; essentially all of the 75 ms was `gfxdebugger`
starting up and dynamically linking `libbinder`, `libutils` and `libc++`. The
ceiling was never the capture.

### The gfxdebugger IPC protocol

`gfxdebugger` is a thin client for `/dev/socket/gfxdebugger-ipc`. The whole
exchange is one `connect()` and one `write()` — it never reads a reply, it
just closes:

```
u32   0x04          constant
u32   0x01          constant
u32   0x02          command: screencap
u32   display       0 = primary, 1 = cover
cstr  path          NUL-terminated, zero-padded to a 4-byte boundary
```

Recovered by tracing the real binary (`strace` ships on the device), not by
disassembling it, and each field pinned by varying the inputs: `-d 1` flips
word 4 alone, and a 28-character path yields a 48-byte message against 40 for
a 21-character one, matching the padding rule exactly.

The "reply is a 4-byte parcel" claim in earlier notes is not something the
client ever waits on — file completeness is the only real signal.

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

Streaming defaults to raw because it skips the PNG encode entirely, which is
now the dominant cost on that path — PNG sits at 12.4 fps whether capped or
not, while raw runs to 66. The header also doubles as a sync marker and
reports geometry, so the host splits frames for free and the cover display
needs no special casing.

The costs: **~12x the bandwidth** (~4.4 MB/s at 29 fps, fine over USB where
adb does 9, painful over adb-on-wifi — use `--format png` there), and RGB565
colour. Raw is *not* pixel-identical to the PNG: only 0.2% of pixels match
exactly, mean error ~0.5–6 per channel, PSNR 35.5 dB. Indistinguishable on
flat UI, but `shot` always asks for PNG, where fidelity matters and throughput
does not.

### The race this design defends against

b2g writes the frame file **asynchronously**. The capture request returns as
soon as it is accepted, and measured back to back, **23 of 40 PNG frames were
still incomplete** at that point. So the pump waits for the frame to be
complete — `IEND` for PNG, the expected size for raw — then renames it aside
before sending, so a late writer can never truncate the copy in flight.

This guard used to cost a 34 ms fork and was worth making optional. It is now
a single `stat` syscall costing ~9%, so it is unconditional and
`--no-device-guard` is gone: skipping it produced intermittent duplicate
frames (4 in one 150-frame run, 0 in the next) for no meaningful gain.

### Verifying the frame rate is real

A pump that re-ships a staged frame would inflate its rate while showing a
stale screen, and on a static UI that is invisible in the output. So the
numbers above were checked rather than trusted: the pump counts captures
against frames shipped (`/data/local/tmp/kaipump.stats`), every frame is
verified to start on a header with no trailing remainder, and IPC output is
byte-identical to `exec` output on a static screen.

### Driving the device

`--control` forwards terminal keystrokes to the phone while the mirror runs:
arrows navigate, enter is OK, backspace is BACK, digits and `*`/`#` are
themselves, `m` is MENU, `,`/`.` are the soft keys, `q` quits.

Keys go over a **persistent channel**, which is the whole point: a one-shot
`kaimirror key` costs ~140 ms, of which ~104 ms is just the `adb shell` round
trip and the process spawns. Holding one `adb shell` open for the session
turns that into a 0.2 ms pipe write, and the pump writes `struct input_event`
straight to `/dev/input/eventN` instead of forking `sendevent` twice per key.

It needs its own connection rather than riding the frame stream: the stream
uses `adb exec-out`, because `adb shell` mangles binary output, and
**`exec-out` does not forward stdin at all**.

Note that keystrokes are read from the *terminal*, not the mirror window —
ffplay keeps its own key handling and offers no way to forward them.

End-to-end, a keypress reaches the screen in **~240 ms**, and that is now
dominated by b2g's own repaint plus the capture pipeline rather than by the
transport; varying the key hold between 5 ms and 50 ms does not move it
outside the noise.

### Dead ends, all tested on the device

- **JPEG**: does not exist on this path. `gfxdebugger` has no format strings
  at all; b2g gives you PNG or raw.
- **Let b2g write straight into a FIFO**, skipping the file: rejected
  synchronously (`result: 1`, zero bytes). b2g requires a regular file.
- **Stage frames on tmpfs** instead of flash: `mount` is denied even as root
  under SELinux, and it would not have helped — flash was never the cost.

## Files

- `kaimirror.py` — host-side CLI and frame-stream reader
- `kaipump/` — device-side frame pump in Rust, statically linked for ARM32.
  Speaks b2g's socket directly; `build.sh [--push]` builds and installs it.
  Keeps an `exec` backend that spawns `gfxdebugger` per frame, so the claim
  that the socket is what makes it fast stays falsifiable.
- `kaimirror_device.sh` — the original shell pump, kept as the fallback when
  `kaipump` has not been built
