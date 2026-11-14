#!/usr/bin/env bash
# The JSON measurements (PLAN.md §M14.15): fetches the JSON benchmarks'
# corpora (serde-rs/json-benchmark's copies of nativejson-benchmark's
# canada, citm_catalog and twitter), then runs bench/json against
# serde_json and, given a nucleus binary, bench/json/pg.py against
# PostgreSQL in Docker.
#
#   bench/json/run.sh [target/release/nucleus]
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
data=$root/target/bench-data/json
mkdir -p "$data"
for f in canada citm_catalog twitter; do
  [ -f "$data/$f.json" ] || curl -sfL -o "$data/$f.json" \
    "https://raw.githubusercontent.com/serde-rs/json-benchmark/master/data/$f.json"
done
(cd "$root/bench/json" && CARGO_TARGET_DIR=$root/target/json cargo run --release -q -- \
  "$data/canada.json" "$data/citm_catalog.json" "$data/twitter.json")
if [ $# -gt 0 ]; then
  python3 "$root/bench/json/pg.py" --nucleus "$1"
fi
