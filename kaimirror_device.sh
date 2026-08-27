#!/system/bin/sh
# Device-side frame pump for kaimirror.
# Captures the b2g-composited primary display via gfxdebugger and emits a
# framed stream on stdout:  "FRAME" + %010d size + <that many PNG bytes>
#
# gfxdebugger asks b2g (over /dev/socket/gfxdebugger-ipc) to write a PNG, and
# b2g finishes that write asynchronously -- so we must wait for the PNG IEND
# chunk before touching the file, then rename it away so a late writer can
# never truncate the copy we are about to send.

W=/data/local/tmp/.kaimirror_w.png
R=/data/local/tmp/.kaimirror_r.png
IEND=49454e44ae426082
DELAY=${1:-30000}     # inter-poll usleep; busy-spinning starves b2g's encoder
DISP=${2:-0}          # 0 = primary, 1 = external

rm -f "$W" "$R"
trap 'rm -f "$W" "$R"' EXIT

while true; do
  gfxdebugger -c screencap -d "$DISP" -p "$W" >/dev/null 2>&1
  while [ "$(tail -c 8 "$W" 2>/dev/null | od -An -tx1 | tr -d ' \n')" != "$IEND" ]; do
    usleep "$DELAY"
  done
  mv -f "$W" "$R" 2>/dev/null || continue
  sz=$(stat -c %s "$R" 2>/dev/null) || continue
  [ "${sz:-0}" -gt 100 ] || continue
  printf 'FRAME%010d' "$sz"
  cat "$R"
done
