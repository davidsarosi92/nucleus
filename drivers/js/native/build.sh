#!/usr/bin/env bash
# Builds the embedded edition's addon for this machine (PLAN.md §M14.23):
# the engine (a static library) and native/addon.c linked into one file,
#   drivers/js/native/<platform>-<arch>/nucleus.node
# which the driver loads for `connect("file:...")` (NUCLEUS_EMBEDDED_ADDON
# names another).  Needs cargo, a C compiler and Node's headers (with
# Node; or NODE_INCLUDE).  Windows: build it with node-gyp (not yet here).
set -euo pipefail
# The oldest macOS the package runs on (Rust's own for Apple silicon):
# the engine's C parts too, so the linker sees one target.
[ "$(uname -s)" = Darwin ] && export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-11.0}
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
cargo build -q --release --manifest-path "$root/Cargo.toml" -p nucleus-embedded
include=${NODE_INCLUDE:-$(cd "$(dirname "$(command -v node)")/../include/node" && pwd)}
platform=$(node -p 'process.platform + "-" + process.arch')
out="$here/$platform"
mkdir -p "$out"
case "$(uname -s)" in
  Darwin) link=(-bundle -undefined dynamic_lookup -liconv) ;;
  *) link=(-shared -lpthread -ldl -lm) ;;
esac
cc -std=c11 -O2 -fPIC -Wall -Wextra -Werror -I"$include" -I"$root/editions/embedded/include" \
  -o "$out/nucleus.node" "$here/addon.c" "$root/target/release/libnucleus_embedded.a" "${link[@]}"
strip -x "$out/nucleus.node" 2>/dev/null || true
ls -l "$out/nucleus.node"
