#!/bin/sh
# Runs an Android program (a test binary) on the device or emulator adb
# reaches, the way cargo runs a target's binaries (PLAN.md §M14.23):
#
#   export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$NDK/.../aarch64-linux-android26-clang
#   export CARGO_TARGET_AARCH64_LINUX_ANDROID_RUNNER="$PWD/scripts/adb-run.sh"
#   cargo test --target aarch64-linux-android -p nucleus-sql
#
# The program runs in a fresh directory under /data/local/tmp (also its
# TMPDIR), removed afterwards; its exit status is the device's.
set -eu
program=$1
shift
dir=/data/local/tmp/nucleus-run-$$
adb shell "mkdir -p $dir/tmp" >/dev/null
trap 'adb shell "rm -r $dir" >/dev/null 2>&1 || true' EXIT
adb push "$program" "$dir/program" >/dev/null
quoted=
for arg in "$@"; do
  quoted="$quoted '$(printf %s "$arg" | sed "s/'/'\\\\''/g")'"
done
env=
for name in RUST_TEST_THREADS RUST_BACKTRACE SLT; do
  eval "value=\${$name:-}"
  [ -n "$value" ] && env="$env $name='$value'"
done
# adb shell's own status is the command's (adb 31 and later).
adb shell "cd $dir && TMPDIR=$dir/tmp$env ./program$quoted"
