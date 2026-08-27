#!/usr/bin/env python3
"""kaimirror -- scrcpy-style screen mirroring for KaiOS 3.x devices.

KaiOS runs Gecko (b2g) directly on the Android HAL with no SurfaceFlinger and
no Android framework, so scrcpy itself cannot work: its server needs
android.jar, SurfaceControl and a MediaCodec encoder bound to a virtual
display.  /system/bin/screencap and screenrecord ship on the device but hang
forever waiting for a SurfaceFlinger binder that never registers.

What KaiOS does provide is /system/bin/gfxdebugger, which asks b2g over
/dev/socket/gfxdebugger-ipc to dump the composited primary display to a PNG.
That is the capture primitive.  Input injection goes through sendevent on the
raw /dev/input nodes.  This tool wires both into a live mirror.

Requires: adb root on a userdebug build, and ffplay for the viewer.
"""

import argparse
import os
import shutil
import subprocess
import sys
import time

DEVICE_SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                             "kaimirror_device.sh")
REMOTE_SCRIPT = "/data/local/tmp/kaimirror_device.sh"
PNG_SIG = b"\x89PNG\r\n\x1a\n"
MAX_CHUNK = 1 << 24     # anything larger is garbage, not a 240x320 frame

# Linux input event codes, per `getevent -pl` on the device.
# matrix-keypad (event1) carries the whole keypad; the power/volume keys live
# on separate nodes because they are wired to different controllers.
KEYS = {
    "0": (1, 11), "1": (1, 2), "2": (1, 3), "3": (1, 4), "4": (1, 5),
    "5": (1, 6), "6": (1, 7), "7": (1, 8), "8": (1, 9), "9": (1, 10),
    "UP": (1, 103), "DOWN": (1, 108), "LEFT": (1, 105), "RIGHT": (1, 106),
    "OK": (1, 352), "CENTER": (1, 352),
    "BACK": (1, 158), "MENU": (1, 139), "HELP": (1, 138),
    "CALL": (1, 231), "SEND": (1, 231),
    "STAR": (1, 522), "POUND": (1, 523),
    "SOFT_LEFT": (1, 30), "SOFT_RIGHT": (1, 48),   # KEY_A / KEY_B
    "POWER": (0, 116), "VOLUMEDOWN": (0, 114),
    "VOLUMEUP": (2, 115), "CAMERA": (2, 212),
}


def adb(*args, **kw):
    return subprocess.run(("adb",) + args, capture_output=True, **kw)


def ensure_root():
    who = adb("shell", "id").stdout.decode(errors="replace")
    if "uid=0" in who:
        return
    adb("root")
    time.sleep(2)
    adb("wait-for-device")
    who = adb("shell", "id").stdout.decode(errors="replace")
    if "uid=0" not in who:
        sys.exit("error: could not get adb root (needs a userdebug/ro.debuggable build)")


def push_script():
    if not os.path.exists(DEVICE_SCRIPT):
        sys.exit(f"error: missing {DEVICE_SCRIPT}")
    adb("push", DEVICE_SCRIPT, REMOTE_SCRIPT)


def send_key(name, hold=0.05):
    """Inject a key down/up pair via sendevent."""
    key = name.upper()
    if key not in KEYS:
        sys.exit(f"error: unknown key {name!r}; known: {', '.join(sorted(KEYS))}")
    node, code = KEYS[key]
    dev = f"/dev/input/event{node}"
    def ev(val):
        return f"sendevent {dev} 1 {code} {val}; sendevent {dev} 0 0 0"
    adb("shell", f"{ev(1)}; sleep {hold}; {ev(0)}")


def wake():
    """Tap power so the panel is lit -- a blanked screen captures as solid black."""
    send_key("POWER")
    time.sleep(1)
    out = adb("shell", "cat /sys/class/leds/lcd-backlight/brightness").stdout
    try:
        return int(out.decode().strip() or 0)
    except ValueError:
        return 0


class FrameStream:
    """Reads the PNG stream produced by kaimirror_device.sh.

    The device sends back-to-back PNGs with no length prefix -- framing them
    device-side would cost a `stat` fork per frame, and forks are the pump's
    dominant cost.  So we walk the PNG chunk headers to find each IEND, which
    is free here and self-synchronising: if the stream is ever damaged we scan
    forward to the next signature rather than dying.
    """

    def __init__(self, delay_us=5000, display=0):
        self.proc = subprocess.Popen(
            ["adb", "exec-out", "sh", REMOTE_SCRIPT, str(delay_us), str(display)],
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.buf = b""
        self.pos = 8        # chunk-walk offset within the frame being parsed

    def _fill(self):
        # read1() returns as soon as bytes are available; read() would block
        # for the full request and add a frame of latency.
        chunk = self.proc.stdout.read1(65536)
        if not chunk:
            return False
        self.buf += chunk
        return True

    def _next_png(self):
        while True:
            if len(self.buf) < 8:
                if not self._fill():
                    return None
                continue
            if not self.buf.startswith(PNG_SIG):
                j = self.buf.find(PNG_SIG, 1)
                if j < 0:
                    self.buf = self.buf[-(len(PNG_SIG) - 1):]
                    if not self._fill():
                        return None
                else:
                    self.buf = self.buf[j:]
                self.pos = 8
                continue
            if self.pos + 8 > len(self.buf):
                if not self._fill():
                    return None
                continue
            length = int.from_bytes(self.buf[self.pos:self.pos + 4], "big")
            ctype = self.buf[self.pos + 4:self.pos + 8]
            if length > MAX_CHUNK or not ctype.isalpha():
                self.buf = self.buf[1:]     # not a real header; resync
                self.pos = 8
                continue
            end = self.pos + 8 + length + 4
            if end > len(self.buf):
                if not self._fill():
                    return None
                continue
            if ctype == b"IEND":
                png, self.buf = self.buf[:end], self.buf[end:]
                self.pos = 8
                return png
            self.pos = end

    def frames(self):
        while True:
            png = self._next_png()
            if png is None:
                return
            yield png

    def close(self):
        try:
            self.proc.kill()
        except Exception:
            pass


def pipe_to(stream, sink_cmd, label, scale, fps_report):
    sink = subprocess.Popen(sink_cmd, stdin=subprocess.PIPE)
    n, t0, last = 0, time.time(), time.time()
    try:
        for png in stream.frames():
            try:
                sink.stdin.write(png)
                sink.stdin.flush()
            except BrokenPipeError:
                break
            n += 1
            if fps_report and time.time() - last >= 5:
                el = time.time() - t0
                print(f"[{label}] {n} frames, {n/el:.1f} fps", file=sys.stderr)
                last = time.time()
    except KeyboardInterrupt:
        pass
    finally:
        stream.close()
        try:
            sink.stdin.close()
        except Exception:
            pass
        sink.wait()
        el = time.time() - t0
        if n:
            print(f"[{label}] {n} frames in {el:.1f}s ({n/el:.1f} fps)", file=sys.stderr)


def cmd_view(a):
    if not shutil.which("ffplay"):
        sys.exit("error: ffplay not found (install ffmpeg)")
    stream = FrameStream(a.poll_delay, a.display)
    cmd = ["ffplay", "-hide_banner", "-loglevel", "error",
           "-f", "image2pipe", "-vcodec", "png",
           "-framerate", str(a.fps), "-i", "-",
           "-window_title", "kaimirror", "-autoexit"]
    if a.scale != 1:
        cmd[-3:-3] = ["-vf", f"scale=iw*{a.scale}:ih*{a.scale}:flags=neighbor"]
    pipe_to(stream, cmd, "view", a.scale, True)


def cmd_record(a):
    if not shutil.which("ffmpeg"):
        sys.exit("error: ffmpeg not found")
    stream = FrameStream(a.poll_delay, a.display)
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
           "-f", "image2pipe", "-vcodec", "png", "-framerate", str(a.fps),
           "-i", "-", "-pix_fmt", "yuv420p", a.output]
    pipe_to(stream, cmd, "record", 1, True)
    print(f"wrote {a.output}", file=sys.stderr)


def cmd_shot(a):
    stream = FrameStream(a.poll_delay, a.display)
    try:
        for png in stream.frames():
            with open(a.output, "wb") as fh:
                fh.write(png)
            print(f"wrote {a.output} ({len(png)} bytes)")
            return
    finally:
        stream.close()


def cmd_key(a):
    for name in a.names:
        send_key(name)


def cmd_wake(a):
    b = wake()
    print(f"backlight={b}" + ("" if b else "  (still off -- press power manually)"))


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--poll-delay", type=int, default=5000,
                   help="device-side inter-poll usleep in us (default 5000)")
    p.add_argument("--display", type=int, default=0,
                   help="0=primary (default), 1=external/cover")
    p.add_argument("--no-wake", action="store_true",
                   help="do not tap power before streaming")
    sub = p.add_subparsers(dest="cmd", required=True)

    v = sub.add_parser("view", help="live mirror in an ffplay window")
    v.add_argument("--scale", type=float, default=2.0)
    v.add_argument("--fps", type=int, default=6)
    v.set_defaults(func=cmd_view)

    r = sub.add_parser("record", help="record the screen to a video file")
    r.add_argument("output")
    r.add_argument("--fps", type=int, default=6)
    r.set_defaults(func=cmd_record)

    s = sub.add_parser("shot", help="save a single screenshot")
    s.add_argument("output", nargs="?", default="kaishot.png")
    s.set_defaults(func=cmd_shot)

    k = sub.add_parser("key", help="inject key presses")
    k.add_argument("names", nargs="+")
    k.set_defaults(func=cmd_key)

    w = sub.add_parser("wake", help="tap power to light the panel")
    w.set_defaults(func=cmd_wake)

    a = p.parse_args()
    ensure_root()
    if a.cmd in ("view", "record", "shot"):
        push_script()
        if not a.no_wake:
            wake()
    a.func(a)


if __name__ == "__main__":
    main()
