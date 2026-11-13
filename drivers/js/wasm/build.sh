#!/bin/sh
# Builds the engine for the browser (PLAN.md §M14.23): the embedded edition
# for wasm32-wasip1, without the model modules (a browser calls no
# provider's API with a key in the page, and runs no local model), into
# drivers/js/wasm/nucleus.wasm — made smaller by wasm-opt when it is there.
#
#   rustup target add wasm32-wasip1
#   drivers/js/wasm/build.sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
cd "$root"
# Whole-program optimization (LTO, one codegen unit): 16% smaller at the
# same speed.  opt-level s or z would save 0.15-0.2 MB more after brotli
# but run 20-50% slower (PLAN.md §M14.23 step 2).
cargo build --release --locked --target wasm32-wasip1 --target-dir target/wasm \
  -p nucleus-embedded --no-default-features \
  --config profile.release.lto=true --config profile.release.codegen-units=1
out="$here/nucleus.wasm"
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext \
    target/wasm/wasm32-wasip1/release/nucleus_embedded.wasm -o "$out"
else
  cp target/wasm/wasm32-wasip1/release/nucleus_embedded.wasm "$out"
fi
ls -l "$out"
