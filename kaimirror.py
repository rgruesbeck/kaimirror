#!/usr/bin/env python3
"""kaimirror -- scrcpy-style screen mirroring for KaiOS 3.x devices.

KaiOS runs Gecko (b2g) directly on the Android HAL with no SurfaceFlinger and
no Android framework, so scrcpy itself cannot work: its server needs
android.jar, SurfaceControl and a MediaCodec encoder bound to a virtual
display.  /system/bin/screencap and screenrecord ship on the device but hang
forever waiting for a SurfaceFlinger binder that never registers.

What KaiOS does provide is b2g's own /dev/socket/gfxdebugger-ipc, which dumps
the composited display to a file.  /system/bin/gfxdebugger is a thin client for
that socket; the device-side pump speaks it directly instead, which is what
makes the frame rate a device limit rather than a process-startup one.  Input
injection goes through sendevent on the raw /dev/input nodes.

Requires: adb root on a userdebug build, and ffplay for the viewer.
"""

import argparse
import itertools
import os
import select
import shutil
import struct
import subprocess
import sys
import termios
import threading
import time
import tty
from queue import Queue

VERSION = "0.1.0"

_HERE = os.path.dirname(os.path.abspath(__file__))
# The native pump talks to b2g's socket directly and runs ~4x faster, but it
# has to be cross-compiled.  The shell script stays as a fallback so a
# checkout without an NDK still works, just slowly.
DEVICE_PUMP = os.path.join(_HERE, "kaipump", "target",
                           "armv7-linux-androideabi", "release", "kaipump")
REMOTE_PUMP = "/data/local/tmp/kaipump"
DEVICE_SCRIPT = os.path.join(_HERE, "kaimirror_device.sh")
REMOTE_SCRIPT = "/data/local/tmp/kaimirror_device.sh"
REMOTE_STAGING = "/data/local/tmp/.kaimirror_* /data/local/tmp/kaipump.stats"
MAX_FPS = 30            # uncapped costs most of b2g's CPU; see README

# Terminal keystrokes -> device keys, for `view --control`.  Arrow keys arrive
# as escape sequences; the rest are what a phone keypad has anyway.
CONTROL_KEYS = {
    "\x1b[A": "UP", "\x1b[B": "DOWN", "\x1b[C": "RIGHT", "\x1b[D": "LEFT",
    "\r": "OK", "\n": "OK", "\x7f": "BACK", "\x08": "BACK",
    "*": "STAR", "#": "POUND", ",": "SOFT_LEFT", ".": "SOFT_RIGHT",
    "m": "MENU", "c": "CALL", "-": "VOLUMEDOWN", "+": "VOLUMEUP",
    **{str(d): str(d) for d in range(10)},
}
# Reaping a pump means killing its whole process group: an orphan wedges in
# `cat`, blocked writing to a half-open adb socket that never returns EPIPE, so
# signalling the shell alone leaves it stuck with the write still pending.  The
# bracketed dot keeps the pattern from matching the shell running the sweep,
# and the pgid compare keeps that shell from killing its own session.
# The bracketed letters keep each pattern from matching the shell running the
# sweep, whose own command line contains this text.
PUMP_SWEEP = (
    'me=$(cut -d" " -f5 /proc/$$/stat); '
    'for p in $(pgrep -f "kaipum[p]|kaimirror_device[.]sh"); do '
    'g=$(cut -d" " -f5 /proc/$p/stat 2>/dev/null); '
    '[ -n "$g" ] && [ "$g" != "$me" ] && kill -9 -"$g" 2>/dev/null; '
    'done; true'
)
PNG_SIG = b"\x89PNG\r\n\x1a\n"
MAX_CHUNK = 1 << 24     # anything larger is garbage, not a 240x320 frame
RAW_HDR = 16            # w, h, format, planes -- uint32 LE each
KEY_HOLD = 0.05         # seconds a key is held down before the release event

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


def push_pump():
    """Install the device-side pump, and report which one got installed."""
    if os.path.exists(DEVICE_PUMP):
        adb("push", DEVICE_PUMP, REMOTE_PUMP)
        adb("shell", "chmod", "755", REMOTE_PUMP)
        kind = "native"
    elif os.path.exists(DEVICE_SCRIPT):
        adb("push", DEVICE_SCRIPT, REMOTE_SCRIPT)
        print("note: native pump not built (run kaipump/build.sh) -- using the "
              "shell fallback at ~6 fps", file=sys.stderr)
        kind = "shell"
    else:
        sys.exit(f"error: no pump to push; expected {DEVICE_PUMP} or "
                 f"{DEVICE_SCRIPT}")
    # A pump orphaned by a host crash would fight this session over the same
    # staging paths, so clear any before starting.
    adb("shell", PUMP_SWEEP)
    return kind


class Controller:
    """A persistent key-injection channel to the device.

    Each `adb shell sendevent` costs ~140ms, nearly all of it the round trip
    and the process spawns rather than the injection -- the same startup cost
    that dominated frame capture.  One long-lived `adb shell` amortises it
    away, leaving a pipe write.

    This cannot ride the frame connection: the stream needs `adb exec-out`,
    because `adb shell` mangles binary output, and `exec-out` does not forward
    stdin at all.  So control gets its own connection.
    """

    def __init__(self, pump):
        # The shell fallback has no control mode; fall back to one-shot keys.
        self.proc = None
        if pump != "native":
            return
        self.proc = subprocess.Popen(
            ["adb", "shell", REMOTE_PUMP, "control"], stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def send(self, name):
        if self.proc is None:
            send_key(name)
            return
        node, code = KEYS[name]
        try:
            self.proc.stdin.write(f"{node} {code}\n".encode())
            self.proc.stdin.flush()
        except (BrokenPipeError, ValueError, OSError):
            pass    # the channel died; the mirror itself is still fine

    def close(self):
        if self.proc is None:
            return
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        self.proc.kill()


def control_loop(ctl, stop, on_quit):
    """Forward terminal keystrokes to the device until stopped.

    Runs the terminal in cbreak mode so keys arrive unbuffered, and restores
    it whatever happens -- leaving a terminal in cbreak is worse than any
    failure this can hit.
    """
    fd = sys.stdin.fileno()
    saved = termios.tcgetattr(fd)
    try:
        tty.setcbreak(fd)
        while not stop.is_set():
            # Poll rather than block, so a stop set elsewhere is noticed
            # without needing one more keystroke to wake this up.
            if not select.select([fd], [], [], 0.2)[0]:
                continue
            ch = os.read(fd, 1).decode(errors="ignore")
            if ch == "\x1b":               # escape sequence: arrow keys
                ch += os.read(fd, 2).decode(errors="ignore")
            if ch in ("q", "\x03"):
                on_quit()
                return
            name = CONTROL_KEYS.get(ch)
            if name:
                ctl.send(name)
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, saved)


def send_key(name):
    """Inject a key down/up pair via sendevent."""
    key = name.upper()
    if key not in KEYS:
        sys.exit(f"error: unknown key {name!r}; known: {', '.join(sorted(KEYS))}")
    node, code = KEYS[key]
    dev = f"/dev/input/event{node}"
    def ev(val):
        return f"sendevent {dev} 1 {code} {val}; sendevent {dev} 0 0 0"
    adb("shell", f"{ev(1)}; sleep {KEY_HOLD}; {ev(0)}")


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
         the cover display's 128x128 needs no special casing.
    png  walk the chunk headers to the IEND chunk.
    """

    def __init__(self, delay_us, display, fmt, pump, max_fps=MAX_FPS):
        self.fmt = fmt
        self.geom = None    # (w, h), learned from the first raw frame header
        if pump == "native":
            # limit=0 streams without end; the cap is what keeps b2g off the
            # CPU, since uncapped the pump outruns the panel several times over.
            argv = [REMOTE_PUMP, str(delay_us), str(display), fmt,
                    "1", "0", "ipc", str(max_fps)]
        else:
            argv = ["sh", REMOTE_SCRIPT, str(delay_us), str(display), fmt, "1"]
        self.proc = subprocess.Popen(
            ["adb", "exec-out"] + argv,
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
        # Killing the local adb client does not reap the remote shell, and its
        # EXIT trap only fires if it gets a signal -- so signal it, give any
        # gfxdebugger write b2g still owes us time to land, then sweep.  Left
        # alone the pump loops forever, re-creating the staging files and
        # fighting the next session over the same paths.
        adb("shell", f"{PUMP_SWEEP}; sleep 0.3; rm -f {REMOTE_STAGING}")


def sink_input(stream, fps):
    """ffmpeg/ffplay input args for whatever the device is sending."""
    if stream.fmt == "raw":
        w, h = stream.geom
        return ["-f", "rawvideo", "-pixel_format", "rgb565le",
                "-video_size", f"{w}x{h}", "-framerate", str(fps), "-i", "-"]
    return ["-f", "image2pipe", "-vcodec", "png", "-framerate", str(fps), "-i", "-"]


def pipe_to(stream, make_cmd, label):
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
        for frame in itertools.chain([first], frames):
            if not pump.is_alive():
                break
            queue.put(frame)
            n += 1
            if time.time() - last >= 5:
                el = time.time() - t0
                print(f"[{label}] {n} frames, {n/el:.1f} fps", file=sys.stderr)
                last = time.time()
    except KeyboardInterrupt:
        pass
    finally:
        # Every step here has to run even if an interrupt lands inside the
        # teardown itself -- draining the queue and reaping the sink are what
        # finalize a recording, and a KeyboardInterrupt raised in one step
        # would otherwise skip the rest and escape as a traceback.  Each step
        # absorbs its own, so one stray Ctrl-C costs that step and no more.
        def quietly(fn, *args, **kw):
            try:
                fn(*args, **kw)
            except (KeyboardInterrupt, Exception):
                pass

        quietly(stream.close)
        # Let the writer drain what is already queued before closing the sink,
        # so a recording keeps its last frames.
        quietly(queue.put, None, timeout=5)
        quietly(pump.join, timeout=10)
        quietly(sink.stdin.close)
        quietly(sink.wait)
        el = time.time() - t0
        if n:
            print(f"[{label}] {n} frames in {el:.1f}s ({n/el:.1f} fps)", file=sys.stderr)


def cmd_view(a):
    if not shutil.which("ffplay"):
        sys.exit("error: ffplay not found (install ffmpeg)")
    stream = FrameStream(a.poll_delay, a.display, a.format, a.pump, a.max_fps)

    def make(s):
        cmd = ["ffplay", "-hide_banner", "-loglevel", "error"]
        cmd += sink_input(s, a.fps)
        if a.scale != 1:
            cmd += ["-vf", f"scale=iw*{a.scale}:ih*{a.scale}:flags=neighbor"]
        return cmd + ["-window_title", "kaimirror", "-autoexit"]

    # ffplay keeps its own keystrokes, so control is driven from the terminal
    # rather than from the mirror window.
    ctl = keys = stop = None
    if a.control and sys.stdin.isatty():
        ctl, stop = Controller(a.pump), threading.Event()
        keys = threading.Thread(target=control_loop,
                                args=(ctl, stop, stream.close), daemon=True)
        keys.start()
        print("control: arrows/enter navigate, backspace=back, digits, "
              "m=menu, q=quit", file=sys.stderr)
    elif a.control:
        print("note: --control needs a terminal; ignoring", file=sys.stderr)

    try:
        pipe_to(stream, make, "view")
    finally:
        if stop:
            stop.set()
            keys.join(timeout=1)
            ctl.close()


def cmd_record(a):
    if not shutil.which("ffmpeg"):
        sys.exit("error: ffmpeg not found")
    stream = FrameStream(a.poll_delay, a.display, a.format, a.pump, a.max_fps)

    def make(s):
        return (["ffmpeg", "-hide_banner", "-loglevel", "error", "-y"]
                + sink_input(s, a.fps) + ["-pix_fmt", "yuv420p", a.output])

    pipe_to(stream, make, "record")
    print(f"wrote {a.output}", file=sys.stderr)


def cmd_shot(a):
    # Always PNG: a single frame is not worth optimising, and PNG carries more
    # colour precision than the RGB565 raw dump.
    stream = FrameStream(a.poll_delay, a.display, "png", a.pump)
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
        p.add_argument("--max-fps", type=positive_int, default=dflt(MAX_FPS),
                       help=f"cap the device-side capture rate (default: "
                            f"{MAX_FPS}).  Uncapping costs most of b2g's CPU "
                            f"to re-capture a screen that is not changing "
                            f"that fast; native pump only")
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
  kaimirror view --control           drive the phone from the terminal
  kaimirror view --max-fps 60        uncap-ish; costs b2g a lot of CPU
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
    v.add_argument("--control", action="store_true",
                   help="forward terminal keystrokes to the device over a "
                        "persistent channel (~0.3ms per key against ~140ms "
                        "for `kaimirror key`); needs a TTY")
    v.add_argument("--scale", type=positive_float, default=2.0,
                   help="window magnification, nearest-neighbour (default: 2)")
    v.add_argument("--fps", type=positive_int, default=MAX_FPS,
                   help=f"rate declared to ffplay; the native pump delivers "
                        f"~{MAX_FPS}, the shell fallback ~6 "
                        f"(default: {MAX_FPS})")
    v.set_defaults(func=cmd_view, capture=True)

    r = sub.add_parser("record", parents=[stream_opts], formatter_class=fmt,
                       help="record the screen to a video file",
                       description="Record the screen to a video file.\n"
                                   "Ctrl-C finalizes it; an existing file is "
                                   "overwritten.")
    r.add_argument("output", help="output path; the extension picks the "
                                  "container (e.g. out.mp4)")
    r.add_argument("--fps", type=positive_int, default=MAX_FPS,
                   help=f"rate declared to ffmpeg; the native pump delivers "
                        f"~{MAX_FPS}, the shell fallback ~6 "
                        f"(default: {MAX_FPS})")
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
        a.pump = push_pump()
        if not a.no_wake:
            wake()
    a.func(a)


if __name__ == "__main__":
    main()
