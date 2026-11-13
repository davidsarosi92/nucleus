#!/bin/sh
# Builds NucleusEmbedded.xcframework (PLAN.md §M14.23 step 3): the embedded
# edition — the engine as a static library, without the model modules —
# for iOS devices, the iOS simulator and macOS, with its C header, for
# the package's NucleusEmbedded product.
#
#   rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios \
#     aarch64-apple-darwin x86_64-apple-darwin
#   drivers/swift/build-xcframework.sh            # every slice
#   ARCHS=arm64 drivers/swift/build-xcframework.sh  # Apple silicon only (faster)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
cd "$root"
archs=${ARCHS:-"arm64 x86_64"}
# The package's minimum versions (Package.swift).
export IPHONEOS_DEPLOYMENT_TARGET=15.0 MACOSX_DEPLOYMENT_TARGET=12.0
out=target/mobile
build() {
  # Whole-program optimization, one codegen unit, as for WebAssembly.
  cargo rustc --release --locked --target "$1" --target-dir "$out" \
    -p nucleus-embedded --no-default-features --crate-type staticlib \
    --config profile.release.lto=true --config profile.release.codegen-units=1
}
slices() { # a platform's slices, lipo'd into one library: $1 name, then targets
  name=$1
  shift
  libs=""
  for t in "$@"; do
    build "$t"
    libs="$libs $out/$t/release/libnucleus_embedded.a"
  done
  mkdir -p "$out/apple/$name"
  # shellcheck disable=SC2086
  lipo -create $libs -output "$out/apple/$name/libnucleus_embedded.a"
}
pick() { # the targets of $archs among arm64=$1 x86_64=$2
  for a in $archs; do
    case $a in arm64) printf '%s ' "$1" ;; x86_64) printf '%s ' "$2" ;; esac
  done
}
slices ios aarch64-apple-ios
# shellcheck disable=SC2046
slices ios-simulator $(pick aarch64-apple-ios-sim x86_64-apple-ios)
# shellcheck disable=SC2046
slices macos $(pick aarch64-apple-darwin x86_64-apple-darwin)
# The header and a module for Swift, in a directory of their own (the
# headers of an app's static XCFrameworks share one directory).
headers=$out/apple/include
mkdir -p "$headers/NucleusEmbeddedC"
cp editions/embedded/include/nucleus_embedded.h "$headers/NucleusEmbeddedC/"
cat > "$headers/NucleusEmbeddedC/module.modulemap" <<'MAP'
module NucleusEmbeddedC {
    header "nucleus_embedded.h"
    link "iconv"
    export *
}
MAP
xc="$here/NucleusEmbedded.xcframework"
if [ -d "$xc" ]; then rm -r "$xc"; fi
xcodebuild -create-xcframework \
  -library "$out/apple/ios/libnucleus_embedded.a" -headers "$headers" \
  -library "$out/apple/ios-simulator/libnucleus_embedded.a" -headers "$headers" \
  -library "$out/apple/macos/libnucleus_embedded.a" -headers "$headers" \
  -output "$xc"
du -sh "$xc"/*/libnucleus_embedded.a
# The size budget (PLAN.md §M14.23 step 3): what the engine adds to an
# iOS app — a program using its C API, linked as an app is (dead code
# stripped), then stripped.
probe="$out/apple/size"
mkdir -p "$probe"
cat > "$probe/main.c" <<'C'
#include "nucleus_embedded.h"
int main(void) {
    char *error = 0;
    nce_database *db = nce_open(":memory:", 0, &error);
    nce_connection *c = nce_connect(db, &error);
    unsigned char b[64];
    nce_send(c, b, 0);
    nce_recv(c, b, sizeof b, 0);
    nce_disconnect(c);
    return nce_close(db, &error);
}
C
xcrun --sdk iphoneos clang -target arm64-apple-ios15.0 -Os -dead_strip -Ieditions/embedded/include \
  -o "$probe/app" "$probe/main.c" "$out/apple/ios/libnucleus_embedded.a" -liconv
strip -x "$probe/app"
size=$(wc -c < "$probe/app")
echo "an iOS app's engine: $size bytes"
if [ "$size" -gt $((8 << 20)) ]; then
  echo "over the size budget: $size bytes, 8 MiB at most for arm64"
  exit 1
fi
