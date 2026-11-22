#!/usr/bin/env bash
# What CI would find on the platforms this machine does not run, found
# before a push: most of CI's failures were code that builds here and not
# there — `std::os::unix` outside `cfg(unix)` (Windows, WASI), a C
# `unsigned long` taken for 64 bits (Windows, 32-bit Linux), a warning
# only another platform's `cfg` shows (CI builds with `-D warnings`).
#
#   scripts/precheck.sh            # format, lints, every target checked, and
#                                  # the wasm job's tests under Node's WASI (~5 min)
#   scripts/precheck.sh --quick    # without the wasm tests (~30 s once built)
#
# The wasm tests run because some failures only show running there: a
# test that starts a thread (WASI has none) builds and aborts.
#
# Checked, as CI builds them (`RUSTFLAGS=-D warnings`, tests included):
# x86_64-pc-windows-msvc and i686-unknown-linux-musl (the workspace),
# wasm32-wasip1, aarch64-linux-android and aarch64-apple-ios (the crates
# their jobs test, and the embedded edition).  Nothing is linked or run
# there but the wasm tests.
#
# Needs: the targets (`rustup target add …`, listed when missing); LLVM's
# clang, clang-cl, llvm-ar and llvm-lib (macOS: `brew install llvm`;
# LLVM=<its bin directory> elsewhere) for ring's C code (the tls module),
# compiled for the check only, against stand-in headers in
# target/precheck/include where no SDK for the target is at hand (Windows,
# Android, musl); Node for the wasm tests.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
wasm_tests=1
for a in "$@"; do
  case "$a" in
    --quick) wasm_tests=0 ;;
    -h|--help) sed -n '2,26p' "$0"; exit 0 ;;
    *) echo "precheck: unknown argument $a" >&2; exit 2 ;;
  esac
done

llvm=${LLVM:-}
if [ -z "$llvm" ]; then
  for d in /opt/homebrew/opt/llvm/bin /usr/local/opt/llvm/bin /usr/lib/llvm-19/bin /usr/lib/llvm-18/bin; do
    [ -x "$d/clang-cl" ] && llvm=$d && break
  done
fi
[ -n "$llvm" ] || { echo "precheck: LLVM's clang-cl not found (brew install llvm, or LLVM=<bin dir>)" >&2; exit 1; }

targets=(x86_64-pc-windows-msvc i686-unknown-linux-musl wasm32-wasip1 aarch64-linux-android aarch64-apple-ios)
installed=$(rustup target list --installed)
missing=()
for t in "${targets[@]}"; do
  grep -qx "$t" <<<"$installed" || missing+=("$t")
done
if [ ${#missing[@]} -gt 0 ]; then
  echo "precheck: rustup target add ${missing[*]}" >&2
  exit 1
fi

# Stand-ins for the C library headers ring includes, enough to compile
# it (never to run it) for Windows.
inc=$root/target/precheck/include
mkdir -p "$inc"
printf '#define assert(x) ((void)0)\n' >"$inc/assert.h"
printf '#include <stddef.h>\nvoid abort(void);\nvoid *malloc(size_t);\nvoid free(void *);\nunsigned long _byteswap_ulong(unsigned long);\nunsigned __int64 _byteswap_uint64(unsigned __int64);\n' >"$inc/stdlib.h"
printf '#include <stddef.h>\nvoid *memcpy(void *, const void *, size_t);\nvoid *memmove(void *, const void *, size_t);\nvoid *memset(void *, int, size_t);\nint memcmp(const void *, const void *, size_t);\n' >"$inc/string.h"
export CC_x86_64_pc_windows_msvc=$llvm/clang-cl AR_x86_64_pc_windows_msvc=$llvm/llvm-lib
export CFLAGS_x86_64_pc_windows_msvc="--target=x86_64-pc-windows-msvc /X -ffreestanding -I$inc -Wno-implicit-function-declaration"
for t in i686-unknown-linux-musl aarch64-linux-android aarch64-apple-ios; do
  v=$(tr '-' '_' <<<"$t")
  export "CC_$v=$llvm/clang" "AR_$v=$llvm/llvm-ar" \
    "CFLAGS_$v=--target=$t -ffreestanding -I$inc -Wno-implicit-function-declaration"
done

failed=()
step() {
  local name=$1
  shift
  printf '== %s\n' "$name"
  if ! "$@"; then
    failed+=("$name")
  fi
}

step "format" cargo fmt --all --check
step "lints (this machine)" cargo clippy --locked --workspace --all-targets -- -D warnings
step "module boundaries" python3 scripts/check-boundaries.py
export RUSTFLAGS="-D warnings"
step "x86_64-pc-windows-msvc" cargo check --locked --workspace --all-targets --target x86_64-pc-windows-msvc
step "i686-unknown-linux-musl" cargo check --locked --workspace --all-targets --target i686-unknown-linux-musl
step "wasm32-wasip1" cargo check --locked --all-targets --target wasm32-wasip1 \
  -p nucleus-platform -p nucleus-storage -p nucleus-txn -p nucleus-kv -p nucleus-sql
for t in aarch64-linux-android aarch64-apple-ios; do
  step "$t" cargo check --locked --all-targets --target "$t" -p nucleus-platform \
    -p nucleus-storage -p nucleus-txn -p nucleus-kv -p nucleus-sql -p nucleus-embedded
done
if [ $wasm_tests = 1 ]; then
  export CARGO_TARGET_WASM32_WASIP1_RUNNER="node $root/scripts/wasi-run.mjs"
  step "wasm32-wasip1 tests" cargo test --locked --target wasm32-wasip1 \
    -p nucleus-platform -p nucleus-storage -p nucleus-txn -p nucleus-kv -p nucleus-sql
fi

if [ ${#failed[@]} -gt 0 ]; then
  printf 'precheck: failed: %s\n' "${failed[@]}" >&2
  exit 1
fi
echo "precheck: all passed"
