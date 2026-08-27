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
import itertools
import os
import shutil
import struct
import subprocess
import sys
import threading
import time
from queue import Queue

VERSION = "0.1.0"

DEVICE_SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                             "kaimirror_device.sh")
REMOTE_SCRIPT = "/data/local/tmp/kaimirror_device.sh"
PNG_SIG = b"\x89PNG\r\n\x1a\n"
MAX_CHUNK = 1 << 24     # anything larger is garbage, not a 240x320 frame
RAW_HDR = 16            # w, h, format, planes -- uint32 LE each

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


def ensure_device():
    """Fail fast if nothing is attached -- `adb wait-for-device` blocks forever."""
    out = adb("devices").stdout.decode(errors="replace").splitlines()[1:]
    if not any(line.split()[1:2] == ["device"] for line in out if line.strip()):
        sys.exit("error: no device (check the cable; `adb devices` should list it)")


def ensure_root():
    ensure_device()
    who = adb("shell", "id").stdout.decode(errors="replace")
    if "uid=0" in who:
        return
    adb("root")
    time.sleep(2)
    try:
        adb("wait-for-device", timeout=30)
    except subprocess.TimeoutExpired:
        sys.exit("error: device did not come back after `adb root`")
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
    """Reads the frame stream produced by kaimirror_device.sh.

    Frames arrive back to back with no length prefix -- framing them
    device-side would cost a fork per frame, and forks are the pump's dominant
    cost.  Both formats are self-describing enough to split here for free, and
    both parsers resynchronise rather than dying if the stream is damaged:

    raw  16-byte header (w, h, format, planes) then w*h*2 bytes of RGB565;
         the header doubles as the sync marker and reports the geometry, so
         the cover display's 128x160 needs no special casing.
    png  walk the chunk headers to the IEND chunk.
    """

    def __init__(self, delay_us=5000, display=0, fmt="raw", guard=True):
        self.fmt = fmt
        self.geom = None    # (w, h), learned from the first raw frame header
        self.proc = subprocess.Popen(
            ["adb", "exec-out", "sh", REMOTE_SCRIPT, str(delay_us), str(display),
             fmt, "1" if guard else "0"],
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

    def _raw_geom(self, at=0):
        """Read a raw frame header, or None if it is not a plausible one."""
        if at + RAW_HDR > len(self.buf):
            return None
        w, h, fmt, planes = struct.unpack_from("<4I", self.buf, at)
        if 0 < w <= 1024 and 0 < h <= 1024 and fmt < 64 and 0 < planes <= 4:
            return w, h
        return None

    def _next_raw(self):
        while True:
            geom = self._raw_geom()
            if geom is None:
                if len(self.buf) < RAW_HDR:
                    if not self._fill():
                        return None
                    continue
                # Desync (a torn frame under --no-device-guard): hunt forward
                # for the next plausible header rather than giving up.
                j = 1
                while j <= len(self.buf) - RAW_HDR:
                    if self._raw_geom(j):
                        break
                    j += 1
                else:
                    self.buf = self.buf[-(RAW_HDR - 1):]
                    if not self._fill():
                        return None
                    continue
                self.buf = self.buf[j:]
                continue
            w, h = geom
            need = RAW_HDR + w * h * 2
            if len(self.buf) < need:
                if not self._fill():
                    return None
                continue
            frame, self.buf = self.buf[:need], self.buf[need:]
            self.geom = geom
            return frame[RAW_HDR:]

    def frames(self):
        nxt = self._next_png if self.fmt == "png" else self._next_raw
        while True:
            frame = nxt()
            if frame is None:
                return
            yield frame

    def close(self):
        try:
            self.proc.kill()
        except Exception:
            pass


def sink_input(stream, fps):
    """ffmpeg/ffplay input args for whatever the device is sending."""
    if stream.fmt == "raw":
        w, h = stream.geom
        return ["-f", "rawvideo", "-pixel_format", "rgb565le",
                "-video_size", f"{w}x{h}", "-framerate", str(fps), "-i", "-"]
    return ["-f", "image2pipe", "-vcodec", "png", "-framerate", str(fps), "-i", "-"]


def pipe_to(stream, make_cmd, label, fps_report):
    # The first frame carries the geometry, so the sink cannot be built until
    # it has arrived.
    frames = stream.frames()
    try:
        first = next(frames)
    except StopIteration:
        stream.close()
        sys.exit("error: no frames from device (is the panel awake?)")
    sink = subprocess.Popen(make_cmd(stream), stdin=subprocess.PIPE)

    # A raw frame (153KB) is larger than the 64KB pipe buffer, so writing it
    # inline blocks until ffmpeg drains -- and while we are blocked we are not
    # draining adb, which stalls the device pump.  Hand the writes to a thread
    # so reading and feeding overlap.
    queue = Queue(maxsize=16)

    def writer():
        while True:
            frame = queue.get()
            if frame is None:
                return
            try:
                sink.stdin.write(frame)
                sink.stdin.flush()
            except (BrokenPipeError, ValueError):
                return

    pump = threading.Thread(target=writer, daemon=True)
    pump.start()

    n, t0, last = 0, time.time(), time.time()
    try:
        for png in itertools.chain([first], frames):
            if not pump.is_alive():
                break
            queue.put(png)
            n += 1
            if fps_report and time.time() - last >= 5:
                el = time.time() - t0
                print(f"[{label}] {n} frames, {n/el:.1f} fps", file=sys.stderr)
                last = time.time()
    except KeyboardInterrupt:
        pass
    finally:
        stream.close()
        # Let the writer drain what is already queued before closing the sink,
        # so a recording keeps its last frames.
        try:
            queue.put(None, timeout=5)
            pump.join(timeout=10)
        except Exception:
            pass
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
    stream = FrameStream(a.poll_delay, a.display, a.format, not a.no_device_guard)

    def make(s):
        cmd = ["ffplay", "-hide_banner", "-loglevel", "error"]
        cmd += sink_input(s, a.fps)
        if a.scale != 1:
            cmd += ["-vf", f"scale=iw*{a.scale}:ih*{a.scale}:flags=neighbor"]
        return cmd + ["-window_title", "kaimirror", "-autoexit"]

    pipe_to(stream, make, "view", True)


def cmd_record(a):
    if not shutil.which("ffmpeg"):
        sys.exit("error: ffmpeg not found")
    stream = FrameStream(a.poll_delay, a.display, a.format, not a.no_device_guard)

    def make(s):
        return (["ffmpeg", "-hide_banner", "-loglevel", "error", "-y"]
                + sink_input(s, a.fps) + ["-pix_fmt", "yuv420p", a.output])

    pipe_to(stream, make, "record", True)
    print(f"wrote {a.output}", file=sys.stderr)


def cmd_shot(a):
    # Always PNG: a single frame is not worth optimising, and PNG carries more
    # colour precision than the RGB565 raw dump.
    stream = FrameStream(a.poll_delay, a.display, "png", True)
    try:
        for png in stream.frames():
            with open(a.output, "wb") as fh:
                fh.write(png)
            print(f"wrote {a.output} ({len(png)} bytes)")
            return
    finally:
        stream.close()


def cmd_key(a):
    if a.list:
        print("\n".join(sorted(KEYS)))
        return
    if not a.names:
        sys.exit("error: give at least one key name, or --list to see them all")
    for name in a.names:
        send_key(name)


def cmd_wake(a):
    b = wake()
    print(f"backlight={b}" + ("" if b else "  (still off -- press power manually)"))


def positive_int(s):
    v = int(s)
    if v <= 0:
        raise argparse.ArgumentTypeError(f"must be a positive integer, got {s!r}")
    return v


def positive_float(s):
    v = float(s)
    if v <= 0:
        raise argparse.ArgumentTypeError(f"must be a positive number, got {s!r}")
    return v


def capture_opts(stream, defaults=False):
    """The shared capture options, as a parent parser.

    Attached both to the top level and to each capture subcommand, so they
    work on either side of the subcommand name.  The subcommand copies default
    to SUPPRESS, which keeps an unset option out of the namespace entirely, so
    they cannot overwrite a value parsed before the subcommand name; only the
    top-level copy carries the real defaults.  The two must be built by
    separate calls: parents= shares action *instances*, so one set_defaults
    would rewrite both.
    """
    def dflt(v):
        return v if defaults else argparse.SUPPRESS

    p = argparse.ArgumentParser(add_help=False)
    p.add_argument("--poll-delay", type=positive_int, metavar="US",
                   default=dflt(5000),
                   help="device-side inter-poll usleep in us (default: 5000)")
    p.add_argument("--display", type=int, choices=(0, 1), default=dflt(0),
                   help="0=primary panel, 1=external/cover display "
                        "(default: 0); geometry is read from the frame")
    p.add_argument("--no-wake", action="store_true", default=dflt(False),
                   help="do not tap power before capturing; a blanked panel "
                        "captures as solid black")
    if stream:
        p.add_argument("--format", choices=("raw", "png"), default=dflt("raw"),
                       help="stream format: raw RGB565 (default, "
                            "content-independent rate) or png (12x less "
                            "bandwidth, slower)")
        p.add_argument("--no-device-guard", action="store_true",
                       default=dflt(False),
                       help="skip the device-side completeness wait and "
                            "resync on the frame header instead: faster, but "
                            "a torn frame becomes possible")
    return p


EXAMPLES = """\
examples:
  kaimirror view                     live mirror window (2x, nearest-neighbour)
  kaimirror view --scale 3
  kaimirror record out.mp4           Ctrl-C to finalize
  kaimirror shot screen.png
  kaimirror shot --display 1 cover.png
  kaimirror key DOWN OK              inject key presses (key --list for names)
  kaimirror view --format png        12x less bandwidth, painful over wifi
  kaimirror view --no-device-guard   ~8.8 fps, torn frames become possible
"""


def build_parser():
    fmt = argparse.RawDescriptionHelpFormatter
    stream_opts = capture_opts(stream=True)
    still_opts = capture_opts(stream=False)

    p = argparse.ArgumentParser(
        prog="kaimirror", description=__doc__, epilog=EXAMPLES,
        formatter_class=fmt, parents=[capture_opts(True, defaults=True)])
    p.set_defaults(capture=False)
    p.add_argument("-V", "--version", action="version",
                   version=f"kaimirror {VERSION}")
    sub = p.add_subparsers(dest="cmd", metavar="COMMAND")

    v = sub.add_parser("view", parents=[stream_opts], formatter_class=fmt,
                       help="live mirror in an ffplay window",
                       description="Live mirror in an ffplay window.")
    v.add_argument("--scale", type=positive_float, default=2.0,
                   help="window magnification, nearest-neighbour (default: 2)")
    v.add_argument("--fps", type=positive_int, default=7,
                   help="rate declared to ffplay; the device delivers ~6.5 "
                        "(default: 7)")
    v.set_defaults(func=cmd_view, capture=True)

    r = sub.add_parser("record", parents=[stream_opts], formatter_class=fmt,
                       help="record the screen to a video file",
                       description="Record the screen to a video file.\n"
                                   "Ctrl-C finalizes it; an existing file is "
                                   "overwritten.")
    r.add_argument("output", help="output path; the extension picks the "
                                  "container (e.g. out.mp4)")
    r.add_argument("--fps", type=positive_int, default=7,
                   help="rate declared to ffmpeg; the device delivers ~6.5 "
                        "(default: 7)")
    r.set_defaults(func=cmd_record, capture=True)

    s = sub.add_parser("shot", parents=[still_opts], formatter_class=fmt,
                       help="save a single screenshot",
                       description="Save a single screenshot.  Always captured as PNG:\n"
                                   "one frame is not worth optimising, and PNG keeps more\n"
                                   "colour precision than the RGB565 raw dump.")
    s.add_argument("output", nargs="?", default="kaishot.png",
                   help="output path (default: kaishot.png)")
    s.set_defaults(func=cmd_shot, capture=True)

    k = sub.add_parser("key", formatter_class=fmt,
                       help="inject key presses",
                       description="Inject key presses via sendevent on the raw\n"
                                   "/dev/input nodes.",
                       epilog="key names:\n  "
                              + "\n  ".join(", ".join(sorted(KEYS)[i:i + 8])
                                            for i in range(0, len(KEYS), 8)))
    k.add_argument("names", nargs="*", metavar="KEY",
                   help="one or more key names, pressed in order")
    k.add_argument("--list", action="store_true",
                   help="print the known key names and exit")
    k.set_defaults(func=cmd_key)

    w = sub.add_parser("wake", formatter_class=fmt,
                       help="tap power to light the panel",
                       description="Tap power to light the panel.  Power *toggles*: if\n"
                                   "the panel was already lit, this turns it off.")
    w.set_defaults(func=cmd_wake)
    return p


def main():
    p = build_parser()
    a = p.parse_args()
    if a.cmd is None:
        p.print_help()
        return
    if a.cmd == "key" and a.list:      # nothing to talk to a device about
        return a.func(a)
    ensure_root()
    if a.capture:
        push_script()
        if not a.no_wake:
            wake()
    a.func(a)


if __name__ == "__main__":
    main()
