#!/bin/sh
# Build both halves: the host CLI, and the device pump cross-compiled for the
# phone.  --push also installs the pump.
#
# The NDK only supplies the linker for the device half: that binary is static,
# so nothing from the NDK ends up as a runtime dependency and the API level in
# the toolchain name is not a floor on the device.
set -e
cd "$(dirname "$0")"
NDK=${ANDROID_NDK_HOME:-$HOME/Android/android-ndk-r27c}
BIN="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"
[ -x "$BIN/armv7a-linux-androideabi21-clang" ] || {
  echo "error: no NDK at $NDK (set ANDROID_NDK_HOME)" >&2; exit 1; }
export PATH="$BIN:$PATH"

cargo build --release -p kaimirror
cargo build --release -p kaipump --target armv7-linux-androideabi

PUMP=target/armv7-linux-androideabi/release/kaipump
file "$PUMP" | grep -q "statically linked" || {
  echo "error: $PUMP is not static -- it will not load on an old bionic" >&2; exit 1; }
echo "host:   target/release/kaimirror"
echo "device: $PUMP ($(du -h "$PUMP" | cut -f1))"

[ "$1" = "--push" ] || exit 0
adb push "$PUMP" /data/local/tmp/kaipump >/dev/null
adb shell chmod 755 /data/local/tmp/kaipump
echo "pushed: /data/local/tmp/kaipump"
