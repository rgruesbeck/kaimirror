#!/system/bin/sh
# Device-side frame pump for kaimirror.
# Emits a stream of back-to-back PNGs on stdout; the host splits them on PNG
# chunk boundaries (see FrameStream in kaimirror.py).
#
# gfxdebugger asks b2g (over /dev/socket/gfxdebugger-ipc) to write a PNG and
# returns as soon as b2g *accepts* the request -- b2g then encodes and writes
# asynchronously, so the file is not complete at return.  Measured back to
# back, 23 of 40 frames were still short at that point.
#
# The dominant cost here is not capture (~41ms) or flash, it is process
# startup: every external command on this device costs ~34ms, so each fork
# removed is worth almost as much as the capture itself.  Hence the pipeline
# below -- the capture for the next frame is issued *before* the current one
# is shipped, so b2g's encode overlaps the transfer and the wait for IEND
# almost always succeeds on the first check.  3 forks per frame plus the
# guard, against 6 in the older serial design.
W=/data/local/tmp/.kaimirror_w.png
R=/data/local/tmp/.kaimirror_r.png
IEND=49454e44ae426082
DELAY=${1:-5000}      # inter-poll usleep; the pipeline means we rarely poll
DISP=${2:-0}          # 0 = primary, 1 = external

rm -f "$W" "$R"
trap 'rm -f "$W" "$R"' EXIT

# Wait until b2g has finished the PNG it is writing to $W, then hand it to $R
# so a late writer can never truncate the copy we are about to send.
harvest() {
  while [ "$(tail -c 8 "$W" 2>/dev/null | od -An -tx1 | tr -d ' \n')" != "$IEND" ]; do
    usleep "$DELAY"
  done
  mv -f "$W" "$R" 2>/dev/null
}

gfxdebugger -c screencap -d "$DISP" -p "$W" >/dev/null 2>&1
harvest || exit 1

while true; do
  gfxdebugger -c screencap -d "$DISP" -p "$W" >/dev/null 2>&1  # b2g encodes...
  cat "$R" 2>/dev/null                                         # ...while we ship
  harvest || continue
done
