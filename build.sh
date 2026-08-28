#!/bin/sh
# Build both halves: the device pump cross-compiled for the phone, and the
# host CLI with that pump embedded in it.  Order matters -- the host build
# reads the pump artifact (see kaimirror/build.rs).
#
#   ./build.sh            device pump + host CLI
#   ./build.sh --push     ...and install the pump on the device
#   ./build.sh --dist     ...and link the host half for release, then
#                         package dist/
#
# Runs on Linux and macOS.  Only the host half differs between them: the
# device pump is the same ARM32 binary either way, and the NDK only supplies
# the linker for it -- that binary is static, so nothing from the NDK ends up
# as a runtime dependency and the API level in the toolchain name is not a
# floor on the device.
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

# The NDK ships one prebuilt toolchain per host OS, and only an x86-64 one for
# macOS -- on Apple silicon it runs under Rosetta, which is why the name below
# is darwin-x86_64 on every Mac.
case "$(uname -s)" in
  Linux)  NDK_HOST=linux-x86_64 ;;
  Darwin) NDK_HOST=darwin-x86_64 ;;
  *) echo "error: unsupported host $(uname -s) (Linux and macOS only)" >&2; exit 1 ;;
esac
CLANG=toolchains/llvm/prebuilt/$NDK_HOST/bin/armv7a-linux-androideabi21-clang

# $ANDROID_NDK_HOME wins; otherwise take the newest NDK from the places the
# standalone download and Android Studio put one.  Globs sort ascending, so
# the last match is the highest version.
NDK=$ANDROID_NDK_HOME
if [ -z "$NDK" ]; then
  for d in "$HOME"/Android/android-ndk-* "$HOME"/Library/Android/sdk/ndk/*; do
    [ -x "$d/$CLANG" ] && NDK=$d
  done
fi
[ -n "$NDK" ] && [ -x "$NDK/$CLANG" ] || {
  echo "error: no NDK for $NDK_HOST${NDK:+ at $NDK} (set ANDROID_NDK_HOME)" >&2; exit 1; }
export PATH="$NDK/toolchains/llvm/prebuilt/$NDK_HOST/bin:$PATH"

cargo build --release -p kaipump --target armv7-linux-androideabi

PUMP=target/armv7-linux-androideabi/release/kaipump
# `file` is not the same tool on both hosts, so confirm static by what a
# dynamic binary would carry and this one must not: the path to the device's
# loader, which a dynamic ELF holds in its PT_INTERP.  -a and the C locale
# are what make grep read a binary as text on both.
if LC_ALL=C grep -qa "/system/bin/linker" "$PUMP"; then
  echo "error: $PUMP is not static -- it will not load on an old bionic" >&2; exit 1
fi

# The host half embeds the pump, so it is built second and told where to find
# it.  Release builds are self-contained per platform: on Linux that means
# musl, because a glibc binary carries the glibc of whatever box built it as a
# floor, which makes it useless as a download; on macOS it means one universal
# binary, so Intel and Apple silicon share a single download.
export KAIPUMP_BIN="$PWD/$PUMP"
if [ -n "$DIST" ]; then
  case "$NDK_HOST" in
    linux-*)
      HOST_TARGETS=x86_64-unknown-linux-musl
      SLUG=x86_64-linux ;;
    darwin-*)
      HOST_TARGETS="aarch64-apple-darwin x86_64-apple-darwin"
      SLUG=universal-macos ;;
  esac
  for t in $HOST_TARGETS; do
    cargo build --release -p kaimirror --target "$t"
  done
  if [ "$SLUG" = universal-macos ]; then
    HOST=target/kaimirror-universal
    lipo -create -output "$HOST" \
      target/aarch64-apple-darwin/release/kaimirror \
      target/x86_64-apple-darwin/release/kaimirror
  else
    HOST=target/x86_64-unknown-linux-musl/release/kaimirror
  fi
else
  cargo build --release -p kaimirror
  HOST=target/release/kaimirror
fi

echo "device: $PUMP ($(du -h "$PUMP" | cut -f1))"
echo "host:   $HOST ($(du -h "$HOST" | cut -f1), pump embedded)"

# A release is one tarball per platform: the host binary already carries the
# pump inside it.  The two tarballs can only be built on their own machines,
# so this keeps any that are already here and rewrites SHA256SUMS over the
# whole directory -- collect both, and the file covers both.
if [ -n "$DIST" ]; then
  VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' kaimirror/Cargo.toml | head -1)
  TARBALL=kaimirror-$VERSION-$SLUG.tar.gz
  mkdir -p dist
  rm -f "dist/$TARBALL" dist/SHA256SUMS
  cp "$HOST" dist/kaimirror
  tar czf "dist/$TARBALL" -C dist kaimirror
  rm dist/kaimirror
  # coreutils on Linux, the BSD tool on macOS; both write the same format.
  SHA=sha256sum
  command -v $SHA >/dev/null || SHA="shasum -a 256"
  (cd dist && $SHA *.tar.gz > SHA256SUMS)
  echo "dist:   dist/$TARBALL ($(du -h "dist/$TARBALL" | cut -f1)) + SHA256SUMS"
fi

[ -n "$PUSH" ] || exit 0
adb push "$PUMP" /data/local/tmp/kaipump >/dev/null
adb shell chmod 755 /data/local/tmp/kaipump
echo "pushed: /data/local/tmp/kaipump"
