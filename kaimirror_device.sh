#!/system/bin/sh
# Device-side frame pump for kaimirror.
# usage: kaimirror_device.sh [delay_us] [display] [raw|png] [guard 0|1]
#
# b2g picks the output format from the file extension, and it is the only
# choice it offers: a path ending in .png gets a PNG, and *any* other
# extension gets an uncompressed RGB565 dump -- 16-byte header (w, h, format,
# planes; all uint32 LE) followed by w*h*2 bytes.  There is no JPEG encoder on
# this path.
#
# raw is the default for streaming because it skips the PNG encode entirely,
# which makes the frame cost independent of what is on screen (PNG throughput
# decays as the screen gets busier) and makes the completeness guard a single
# stat instead of a three-fork tail|od|tr pipeline.  It costs ~12x the
# bandwidth -- fine over USB at ~1.3 MB/s, painful over adb-on-wifi -- and is
# RGB565, so `shot` still asks for PNG where fidelity matters.
#
# gfxdebugger returns as soon as b2g *accepts* the request; b2g writes the
# file asynchronously.  So the frame is guarded before it is handed over, then
# renamed away so a late writer can never truncate the copy being sent.
DELAY=${1:-5000}
DISP=${2:-0}
FMT=${3:-raw}
GUARD=${4:-1}

[ "$FMT" = "png" ] && EXT=png || EXT=raw
W=/data/local/tmp/.kaimirror_w.$EXT
R=/data/local/tmp/.kaimirror_r.$EXT
IEND=49454e44ae426082
SZ=""

rm -f "$W" "$R"
trap 'rm -f "$W" "$R"' EXIT

# PNG grows in chunks, so wait for the IEND chunk to land.
wait_png() {
  while [ "$(tail -c 8 "$W" 2>/dev/null | od -An -tx1 | tr -d ' \n')" != "$IEND" ]; do
    usleep "$DELAY"
  done
}

# Raw frames are a fixed size, so completeness is just a size compare.  The
# size depends on the display (240x320 and the 128x160 cover differ), so learn
# it from the first frame by waiting for the size to stop changing.
wait_raw() {
  if [ -z "$SZ" ]; then
    prev=-1; cur=0
    while [ "$cur" = "0" ] || [ "$cur" != "$prev" ]; do
      prev=$cur; usleep "$DELAY"
      cur=$(stat -c %s "$W" 2>/dev/null || echo 0)
    done
    SZ=$cur
  else
    while [ "$(stat -c %s "$W" 2>/dev/null || echo 0)" != "$SZ" ]; do
      usleep "$DELAY"
    done
  fi
}

settle() {
  if [ "$FMT" = "png" ]; then wait_png; else wait_raw; fi
}

# With guard=0 the device-side wait is skipped and the host resyncs on the
# frame header instead -- faster, but a torn frame is then possible.  The
# priming frame is always guarded so the stream starts aligned.
guard() {
  [ "$GUARD" = "1" ] && settle
  return 0
}

gfxdebugger -c screencap -d "$DISP" -p "$W" >/dev/null 2>&1
settle
mv -f "$W" "$R" 2>/dev/null

while true; do
  gfxdebugger -c screencap -d "$DISP" -p "$W" >/dev/null 2>&1  # b2g writes...
  cat "$R" 2>/dev/null                                         # ...while we ship
  guard
  mv -f "$W" "$R" 2>/dev/null || continue
done
