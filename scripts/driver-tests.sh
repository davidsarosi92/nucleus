#!/usr/bin/env bash
# Starts `nucleus server` (default credentials) on a free port and runs the
# drivers' test suites and the examples against it (PLAN.md §M14.8).
#
#   scripts/driver-tests.sh <path-to-nucleus-binary> [rust|python|js|java|c|cpp ...]
#
# Without a list it runs every language whose toolchain is installed.
set -euo pipefail
bin=$(cd "$(dirname "${1:?usage: scripts/driver-tests.sh <nucleus binary> [langs]}")" && pwd)/$(basename "$1")
shift
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
"$bin" server "$work/db" --listen 127.0.0.1:0 2>"$work/server.log" &
pid=$!
trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null || true; rm -rf "$work"' EXIT
for _ in $(seq 100); do grep -q "listening on" "$work/server.log" 2>/dev/null && break; sleep 0.1; done
port=$(sed -n 's/.*listening on 127\.0\.0\.1:\([0-9]*\).*/\1/p' "$work/server.log" | head -1)
[ -n "$port" ] || { cat "$work/server.log"; echo "server did not start"; exit 1; }
export NUCLEUS_HOST=127.0.0.1 NUCLEUS_PORT=$port
echo "nucleus server on port $port"

have() { command -v "$1" >/dev/null; }
langs=("$@")
if [ ${#langs[@]} -eq 0 ]; then
  have cargo && langs+=(rust)
  have python3 && langs+=(python)
  have node && langs+=(js)
  have javac && langs+=(java)
  have cc && langs+=(c)
  have c++ && langs+=(cpp)
fi

for lang in "${langs[@]}"; do
  echo "== $lang"
  case $lang in
    rust)
      cargo run -q --manifest-path "$root/Cargo.toml" -p nucleus-example ;;
    python)
      python3 "$root/drivers/python/tests/test_nucleus.py" -v 2>&1 | tail -3
      python3 "$root/examples/python/quickstart.py" ;;
    js)
      node --test "$root/drivers/js/test/nucleus.test.ts"
      node "$root/examples/js/quickstart.ts" ;;
    java)
      out="$work/java"
      javac -Xlint:all -Werror -d "$out" $(find "$root/drivers/java/src" "$root/examples/java" -name '*.java')
      java -cp "$out" io.github.davidsarosi92.nucleus.NucleusTest
      java -cp "$out" Quickstart ;;
    c)
      make -s -C "$root/drivers/c" OUT="$work/c" test
      "$work/c/test_nucleus"
      cc -std=c99 -Wall -Wextra -Werror -I"$root/drivers/c" -o "$work/c/quickstart" \
        "$root/examples/c/quickstart.c" "$root/drivers/c/nucleus.c"
      "$work/c/quickstart" ;;
    cpp)
      mkdir -p "$work/cpp"
      cc -std=c99 -Wall -Wextra -Werror -c -o "$work/cpp/nucleus.o" "$root/drivers/c/nucleus.c"
      for src in "$root/drivers/cpp/test/test_nucleus.cpp" "$root/examples/cpp/quickstart.cpp"; do
        exe="$work/cpp/$(basename "$src" .cpp)"
        c++ -std=c++17 -Wall -Wextra -Werror -I"$root/drivers/c" -I"$root/drivers/cpp" \
          -o "$exe" "$src" "$work/cpp/nucleus.o"
        "$exe"
      done ;;
    *) echo "unknown language $lang"; exit 1 ;;
  esac
done
echo "driver tests passed: ${langs[*]}"
