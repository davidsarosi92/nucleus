#!/usr/bin/env bash
# The sample project (PLAN.md §M14.16 M6): migrations, named queries, the
# code `nucleus codegen` makes of them in four languages, and a program in
# each that runs it.
#
#   examples/migrations/run.sh <nucleus binary> [--generate] [ts python java rust ...]
#
# It starts a server on a database of its own, applies migrations/, and
# checks that the generated code (ts/queries.ts, python/queries.py,
# java/Queries.java, rust/src/queries.rs) is what the schema and
# queries.sql make now — `--generate` writes it instead — then runs each
# language's program against the server; then migrates down to nothing
# and up again.  Without a list: every language
# whose toolchain is installed.  CI runs it (scripts/driver-tests.sh codegen).
set -euo pipefail
bin=$(cd "$(dirname "${1:?usage: run.sh <nucleus binary> [--generate] [languages]}")" && pwd)/$(basename "$1")
shift
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
generate=no
if [ "${1:-}" = --generate ]; then generate=yes; shift; fi
have() { command -v "$1" >/dev/null; }
langs=("$@")
if [ ${#langs[@]} -eq 0 ]; then
  have node && langs+=(ts)
  have python3 && langs+=(python)
  have javac && langs+=(java)
  have cargo && langs+=(rust)
fi

work=$(mktemp -d)
"$bin" server "$work/db" --listen 127.0.0.1:0 2>"$work/server.log" &
pid=$!
trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null || true; rm -rf "$work"' EXIT
for _ in $(seq 100); do grep -q "listening on" "$work/server.log" 2>/dev/null && break; sleep 0.1; done
port=$(sed -n 's/.*listening on 127\.0\.0\.1:\([0-9]*\).*/\1/p' "$work/server.log" | head -1)
[ -n "$port" ] || { cat "$work/server.log"; echo "the server did not start"; exit 1; }
export NUCLEUS_HOST=127.0.0.1 NUCLEUS_PORT=$port
server=(-h "127.0.0.1:$port")

cd "$here"
"$bin" migrate up "${server[@]}" --dir migrations

# The code from the queries: written, or checked against what is here.
codegen() { # <language> <file> [options]
  local lang=$1 file=$2
  shift 2
  if [ $generate = yes ]; then
    "$bin" codegen "$lang" "${server[@]}" --queries queries.sql "$@" --out "$file"
    echo "wrote $file"
  else
    "$bin" codegen "$lang" "${server[@]}" --queries queries.sql "$@" --out "$work/generated"
    diff -u "$file" "$work/generated" ||
      { echo "$file is not what the schema and the queries make: run.sh --generate"; exit 1; }
  fi
}
codegen ts ts/queries.ts --import ../../../drivers/js/src/index.ts
codegen python python/queries.py
codegen java java/Queries.java
codegen rust rust/src/queries.rs

for lang in "${langs[@]}"; do
  case $lang in
    ts) node ts/app.ts ;;
    python) python3 python/app.py ;;
    java)
      javac -Xlint:all -Werror -d "$work/java" $(find "$root/drivers/java/src/main" -name '*.java') java/*.java
      java -cp "$work/java" App ;;
    rust) cargo run -q --manifest-path "$root/Cargo.toml" -p nucleus-example-migrations ;;
    *) echo "unknown language $lang"; exit 1 ;;
  esac
done
# Down to nothing and up again: every migration's `down` undoes its `up`.
"$bin" migrate down --all "${server[@]}" --dir migrations
left=$("$bin" sql "${server[@]}" -c "SELECT count(*) FROM nc_catalog.tables WHERE name IN ('users', 'posts')" | sed -n 3p | tr -d ' ')
[ "$left" = 0 ] || { echo "migrate down --all left $left tables"; exit 1; }
"$bin" migrate up "${server[@]}" --dir migrations
echo "the migrations example passed: ${langs[*]}"
