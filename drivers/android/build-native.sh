#!/bin/sh
# Builds libnucleus_android.so for each Android ABI (PLAN.md §M14.23): the
# embedded edition — the engine as a static library, without the model
# modules — linked with its JNI glue (nucleus/src/main/cpp) by the NDK's
# clang, into target/android/jniLibs, where the library's Gradle build
# takes it from.  Only the JNI functions are exported, so the linker drops
# what they do not reach.
#
#   rustup target add aarch64-linux-android x86_64-linux-android
#   drivers/android/build-native.sh               # arm64-v8a and x86_64
#   ABIS=arm64-v8a drivers/android/build-native.sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
cd "$root"
sdk=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}
ndk=${ANDROID_NDK_HOME:-$(ls -d "$sdk"/ndk/* 2>/dev/null | sort -V | tail -1)}
[ -d "$ndk" ] || { echo "no NDK: set ANDROID_NDK_HOME"; exit 1; }
bin=$(ls -d "$ndk"/toolchains/llvm/prebuilt/*/bin | head -1)
api=26 # the library's minSdk
out=target/android
mkdir -p "$out"
exports="$out/exports.map"
printf '{ global: Java_*; local: *; };\n' > "$exports"
for abi in ${ABIS:-arm64-v8a x86_64}; do
  case $abi in
    arm64-v8a) target=aarch64-linux-android ;;
    x86_64) target=x86_64-linux-android ;;
    *) echo "unknown ABI $abi"; exit 1 ;;
  esac
  # Whole-program optimization, one codegen unit, as for iOS and WebAssembly.
  cargo rustc --release --locked --target "$target" --target-dir target/mobile \
    -p nucleus-embedded --no-default-features --crate-type staticlib \
    --config profile.release.lto=true --config profile.release.codegen-units=1
  mkdir -p "$out/jniLibs/$abi"
  so="$out/jniLibs/$abi/libnucleus_android.so"
  # 16 KB pages: Android 15's devices may use them.
  "$bin/$target$api-clang" -shared -fPIC -O2 -std=c99 -Wall -Wextra -Werror \
    -I editions/embedded/include -o "$so" \
    "$here/nucleus/src/main/cpp/nucleus_android.c" "target/mobile/$target/release/libnucleus_embedded.a" \
    -llog -ldl -lm -Wl,--gc-sections -Wl,--version-script="$exports" -Wl,-z,max-page-size=16384
  "$bin/llvm-strip" --strip-unneeded "$so"
  size=$(wc -c < "$so")
  echo "$so: $size bytes"
  # The size budget (PLAN.md §M14.23 step 3): what the engine adds to an
  # app on a device's architecture.
  if [ "$abi" = arm64-v8a ] && [ "$size" -gt $((9 << 20)) ]; then
    echo "over the size budget: $size bytes, 9 MiB at most for arm64-v8a"
    exit 1
  fi
done
