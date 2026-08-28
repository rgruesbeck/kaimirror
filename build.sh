#!/bin/sh
# Build both halves: the device pump cross-compiled for the phone, and the
# host CLI with that pump embedded in it.  Order matters -- the host build
# reads the pump artifact (see kaimirror/build.rs).
#
#   ./build.sh            device pump + host CLI
#   ./build.sh --push     ...and install the pump on the device
#   ./build.sh --dist     ...and link the host half against musl, for release
#
# The NDK only supplies the linker for the device half: that binary is static,
# so nothing from the NDK ends up as a runtime dependency and the API level in
# the toolchain name is not a floor on the device.
set -e
cd "$(dirname "$0")"

PUSH=""
DIST=""
for arg in "$@"; do
  case "$arg" in
    --push) PUSH=1 ;;
    --dist) DIST=1 ;;
    *) echo "usage: $0 [--push] [--dist]" >&2; exit 2 ;;
  esac
done

NDK=${ANDROID_NDK_HOME:-$HOME/Android/android-ndk-r27c}
BIN="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"
[ -x "$BIN/armv7a-linux-androideabi21-clang" ] || {
  echo "error: no NDK at $NDK (set ANDROID_NDK_HOME)" >&2; exit 1; }
export PATH="$BIN:$PATH"

cargo build --release -p kaipump --target armv7-linux-androideabi

PUMP=target/armv7-linux-androideabi/release/kaipump
file "$PUMP" | grep -q "statically linked" || {
  echo "error: $PUMP is not static -- it will not load on an old bionic" >&2; exit 1; }

# The host half embeds the pump, so it is built second and told where to find
# it.  Release builds link against musl: a glibc binary carries the glibc of
# whatever box built it as a floor, which makes it useless as a download.
export KAIPUMP_BIN="$PWD/$PUMP"
if [ -n "$DIST" ]; then
  HOST_TARGET=x86_64-unknown-linux-musl
  cargo build --release -p kaimirror --target "$HOST_TARGET"
  HOST=target/$HOST_TARGET/release/kaimirror
else
  cargo build --release -p kaimirror
  HOST=target/release/kaimirror
fi

echo "device: $PUMP ($(du -h "$PUMP" | cut -f1))"
echo "host:   $HOST ($(du -h "$HOST" | cut -f1), pump embedded)"

[ -n "$PUSH" ] || exit 0
adb push "$PUMP" /data/local/tmp/kaipump >/dev/null
adb shell chmod 755 /data/local/tmp/kaipump
echo "pushed: /data/local/tmp/kaipump"
