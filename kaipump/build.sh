#!/bin/sh
# Build the device-side pump and (with --push) install it.
#
# The NDK only supplies the linker: the binary is static, so nothing from it
# ends up as a runtime dependency and the API level in the toolchain name is
# just which headers clang links against, not a floor on the device.
set -e
NDK=${ANDROID_NDK_HOME:-$HOME/Android/android-ndk-r27c}
BIN="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"
[ -x "$BIN/armv7a-linux-androideabi21-clang" ] || {
  echo "error: no NDK at $NDK (set ANDROID_NDK_HOME)" >&2; exit 1; }

export PATH="$BIN:$PATH"
cd "$(dirname "$0")"
cargo build --release --target armv7-linux-androideabi
OUT=target/armv7-linux-androideabi/release/kaipump
file "$OUT" | grep -q "statically linked" || {
  echo "error: $OUT is not static -- it will not load on an old bionic" >&2; exit 1; }
echo "built: $OUT ($(du -h "$OUT" | cut -f1))"

[ "$1" = "--push" ] || exit 0
adb push "$OUT" /data/local/tmp/kaipump >/dev/null
adb shell chmod 755 /data/local/tmp/kaipump
echo "pushed: /data/local/tmp/kaipump"
