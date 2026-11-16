#!/usr/bin/env bash
# Every benchmark of docs/benchmarks.md by §M14.31's method (PLAN.md
# §M14.31 P1d): each step writes bench/results/<step>.json — every run's
# numbers, their medians and intervals, the environment, the command —
# and docs/benchmarks.md's numbers are read from those files
# (`python3 bench/method.py show bench/results/<step>.json`).
#
#   bench/all.sh                  # build, then every step
#   bench/all.sh list             # the steps
#   bench/all.sh tpch knn …       # some steps (build first: `build`)
#
# RESULTS (default bench/results) is where the files go — a second run
# into another directory, then `bench/method.py reproduce` on each pair,
# checks that the numbers reproduce.  RUNS (default 5) is the runs of a
# step; LONG_RUNS (default 3) those of the steps that take longer than
# ten minutes a run.  The data each step needs is fetched by the command
# its error names.  Heavy steps run under target/bench-tools/memcap.sh.
#
# PostgreSQL runs native (brew install postgresql@18 and pgvector;
# bench/pg.py's cluster under target/bench-pg); ParadeDB in Docker (the
# container nucleus-paradedb, started and stopped here).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
N=$ROOT/target/release/nucleus
PY=$ROOT/target/ann-data/.venv/bin/python
R=${RESULTS:-bench/results}
RUNS=${RUNS:-5}
LONG=${LONG_RUNS:-3}
CAP="bash target/bench-tools/memcap.sh"
ANN=target/ann-data
SIFT=$ANN/sift-128-euclidean
DBPEDIA=$ANN/dbpedia-openai3-1536-100k
export NUCLEUS_BENCH_HOST=${NUCLEUS_BENCH_HOST:-::1}
mkdir -p "$R"

STEPS="suite tpch clickbench knn ann-sift ann-sift-int8 ann-filtered ann-dbpedia ann-related ann-sparse
stats analyzers bm25 bm25-filters bm25-snippets msmarco hybrid hybrid-sparse tsearch tsearch-rank
embed-speed embed-beir longmemeval recall erasure backup retype sealing page-crypto encrypted aead aes
keys json json-pg json-index json-schema json-schema-pg wamp history branches python wasm browser"

need() { # need PATH HOW: a step's data, or how to get it
  [ -e "$1" ] || { echo "missing $1: $2" >&2; exit 1; }
}

# m STEP RUNS [method.py run options …] -- COMMAND …
m() {
  local step=$1 runs=$2
  shift 2
  python3 bench/method.py run "$step" "$R/$step.json" --runs "$runs" "$@"
}

paradedb() { # paradedb start|stop
  if [ "$1" = start ]; then
    docker start nucleus-paradedb >/dev/null 2>&1 ||
      docker run -d --name nucleus-paradedb -e POSTGRES_PASSWORD=x paradedb/paradedb >/dev/null
    sleep 5
  else
    docker stop nucleus-paradedb >/dev/null
  fi
}

step_build() {
  cargo build --release -p nucleus-cli -p nucleus-embedded
  cargo test --release --no-run -p nucleus-sql --test sealed --test encrypted --test recall_bench \
    --test erasure_bench --test retype_bench
  cargo test --release --no-run -p nucleus-storage --test page_crypto_speed
  cargo test --release --no-run -p nucleus-kernels --test aes_speed
  cargo test --release --no-run -p nucleus-crypto --lib
  cargo test --release --no-run -p nucleus-kv --lib
  cargo build --release -p nucleus-kv --examples
  (cd bench/json && CARGO_TARGET_DIR=$ROOT/target/json cargo build --release --bins)
  drivers/js/wasm/build.sh && (cd drivers/js && npx tsc)
}

# --- the suite, TPC-H, ClickBench (in-script method)
step_suite() { python3 bench/run.py --nucleus "$N" --rows 1000000 --runs "$RUNS" --out "$R/suite.json"; }
step_tpch() {
  python3 bench/tpch.py --nucleus "$N" --sf 1 --data target/tpch-sf1 --runs "$RUNS" --out "$R/tpch.json"
}
step_clickbench() {
  python3 bench/clickbench.py --nucleus "$N" --rows 1000000 --runs "$RUNS" --out "$R/clickbench.json"
}

# --- vectors
step_knn() { m knn "$RUNS" -- $CAP 8000 "$PY" -I bench/vectors/knn.py "$N" --pg; }
step_ann-sift() {
  need "$SIFT" "$PY bench/ann/data.py"
  m ann-sift "$LONG" --also "pgvector=$CAP 8000 $PY bench/ann/pgvector_ref.py $SIFT" -- \
    $CAP 8000 "$PY" bench/ann/sql.py "$N" "$SIFT" --efs 10,20,40,60,80,120,200
}
step_ann-sift-int8() {
  m ann-sift-int8 "$LONG" -- $CAP 8000 "$PY" bench/ann/sql.py "$N" "$SIFT" --efs 10,20,40,60,80,120,200 --quantize int8
}
step_ann-filtered() {
  m ann-filtered "$LONG" --also "pgvector=$CAP 8000 $PY bench/ann/filtered.py pgvector $SIFT" -- \
    $CAP 8000 "$PY" bench/ann/filtered.py nucleus "$N" "$SIFT"
}
step_ann-dbpedia() {
  need "$DBPEDIA" "$PY bench/ann/data.py dbpedia-openai3-1536-100k"
  local pgv="$CAP 8000 $PY bench/ann/pgvector_ref.py $DBPEDIA --no-inserts --bulk 99000 --queries 1000 --efs 10,20,40,60,80,200 --server-efs 20,40,60,80,200"
  m ann-dbpedia "$LONG" --also "pgvector=$pgv" --also "half=$pgv --quantize half" \
    --also "binary=$pgv --quantize binary --rescore 5" -- \
    $CAP 8000 "$PY" bench/ann/sql.py "$N" "$DBPEDIA" --no-inserts --bulk 99000 --queries 1000 \
    --efs 10,20,40,80,200 --server-efs 40,60,80,120,200 --quantize int8
}
step_ann-related() {
  m ann-related "$LONG" --also "pgvector=$CAP 8000 $PY bench/ann/related.py pgvector $DBPEDIA" -- \
    $CAP 8000 "$PY" bench/ann/related.py nucleus "$N" "$DBPEDIA"
}
step_ann-sparse() {
  m ann-sparse "$RUNS" --also "pgvector=$CAP 8000 $PY bench/ann/sparse.py pgvector" -- \
    $CAP 8000 "$PY" bench/ann/sparse.py nucleus "$N"
}

# --- statistics, text
step_stats() {
  m stats "$RUNS" --also "work_mem 1GB=$CAP 8000 $PY bench/stats/aggs.py $N --work-mem 1GB" -- \
    $CAP 8000 "$PY" bench/stats/aggs.py "$N" --pg --numpy --python
}
step_analyzers() {
  need target/text-data/beir-fiqa "python3 bench/text/data.py beir"
  m analyzers "$RUNS" --also "postgresql=$PY bench/text/analyzers.py postgres" -- \
    "$PY" bench/text/analyzers.py nucleus "$N"
}
step_bm25() {
  paradedb start
  m bm25 "$RUNS" --also "paradedb=$PY bench/text/beir.py paradedb" -- \
    "$PY" bench/text/beir.py nucleus "$N" --analyzer english --k1 1.2 --b 0.75
  paradedb stop
}
step_bm25-filters() {
  m bm25-filters "$RUNS" -- "$PY" bench/text/beir.py nucleus "$N" --analyzer english --k1 1.2 --b 0.75 \
    --filters 50,10,1 --unranked
}
step_bm25-snippets() {
  paradedb start
  m bm25-snippets "$RUNS" --also "paradedb=$PY bench/text/beir.py paradedb --snippets" -- \
    "$PY" bench/text/beir.py nucleus "$N" --analyzer english --k1 1.2 --b 0.75 --snippets
  paradedb stop
}
step_msmarco() {
  need target/text-data/beir-msmarco "python3 bench/text/data.py msmarco"
  paradedb start
  m msmarco "$LONG" --also "paradedb=$PY bench/text/beir.py paradedb --sets msmarco --split dev --queries 1000" -- \
    $CAP 8000 "$PY" bench/text/beir.py nucleus "$N" --sets msmarco --split dev --queries 1000 \
    --analyzer english --k1 1.2 --b 0.75 --filters 50,10,1
  paradedb stop
}
step_hybrid() {
  paradedb start
  m hybrid "$RUNS" --also "paradedb=$PY bench/text/hybrid.py paradedb" -- "$PY" bench/text/hybrid.py nucleus "$N"
  paradedb stop
}
step_hybrid-sparse() {
  paradedb start
  m hybrid-sparse "$RUNS" --also "paradedb=$PY bench/text/hybrid.py paradedb --sets scifact,nfcorpus,arguana --sparse" -- \
    "$PY" bench/text/hybrid.py nucleus "$N" --sets scifact,nfcorpus,arguana --sparse
  paradedb stop
}
step_tsearch() {
  m tsearch "$RUNS" --also "postgresql=$PY bench/text/tsearch.py postgres" -- \
    "$PY" bench/text/tsearch.py nucleus "$N" --values
}
step_tsearch-rank() {
  m tsearch-rank "$RUNS" --also "postgresql=$PY bench/text/tsearch.py postgres --sets scifact,nfcorpus --rank" -- \
    "$PY" bench/text/tsearch.py nucleus "$N" --sets scifact,nfcorpus --rank
}

# --- embeddings, memory
step_embed-speed() {
  need target/models "$PY bench/embed/models.py"
  m embed-speed "$LONG" -- $CAP 7000 "$PY" -I bench/embed/speed.py
}
step_embed-beir() { m embed-beir "$LONG" -- $CAP 6000 "$PY" bench/embed/beir.py "$N"; }
step_longmemeval() {
  need target/memory-data/longmemeval_s_cleaned.json "see docs/benchmarks.md, Memory stores"
  m longmemeval "$LONG" -- $CAP 8000 "$PY" bench/memory/longmemeval.py "$N" --db target/memory-data/lme-db
}
step_recall() {
  m recall "$RUNS" --also "request id=env REQUEST_ID=gen RECALL_BENCH=1 cargo test --release -q -p nucleus-sql --test recall_bench -- --ignored --nocapture" -- \
    env RECALL_BENCH=1 cargo test --release -q -p nucleus-sql --test recall_bench -- --ignored --nocapture
}
step_erasure() {
  m erasure "$RUNS" -- env ERASURE_DIR=target/erasure-bench cargo test --release -q -p nucleus-sql \
    --test erasure_bench -- --ignored --nocapture
}

# --- operations
step_backup() { m backup "$RUNS" -- python3 bench/backup.py --nucleus "$N"; }
step_retype() {
  m retype 1 -- env RETYPE_DIR=$ROOT/target/retype-bench RETYPE_ROWS=1000000 cargo test --release -q \
    -p nucleus-sql --test retype_bench -- --ignored --nocapture
}
step_sealing() { m sealing "$RUNS" -- cargo test --release -q -p nucleus-sql --test sealed -- --ignored --nocapture; }
step_page-crypto() {
  m page-crypto "$RUNS" -- cargo test --release -q -p nucleus-storage --test page_crypto_speed -- --ignored \
    --nocapture --test-threads=1
}
step_encrypted() { m encrypted "$RUNS" -- cargo test --release -q -p nucleus-sql --test encrypted -- --ignored --nocapture; }
step_aead() { m aead "$RUNS" -- cargo test --release -q -p nucleus-crypto speed -- --ignored --nocapture; }
step_aes() { m aes "$RUNS" -- cargo test --release -q -p nucleus-kernels --test aes_speed -- --ignored --nocapture; }
step_keys() { m keys "$RUNS" -- python3 bench/keys.py --nucleus "$N"; }

# --- JSON
step_json() {
  need target/bench-data/json/twitter.json "bash bench/json/run.sh"
  local d=target/bench-data/json
  m json "$RUNS" -- target/json/release/nucleus-json-bench $d/canada.json $d/citm_catalog.json $d/twitter.json
}
step_json-pg() { m json-pg "$RUNS" -- python3 bench/json/pg.py --nucleus "$N"; }
step_json-index() { m json-index "$RUNS" -- python3 bench/json/index.py --nucleus "$N"; }
step_json-schema() { m json-schema "$RUNS" -- target/json/release/schema; }
step_json-schema-pg() { m json-schema-pg "$RUNS" -- python3 bench/json/schema.py --nucleus "$N"; }

# --- writes
step_wamp() {
  local db=$ROOT/target/wamp-db
  m wamp "$LONG" -- python3 -I bench/wamp/wamp.py nucleus "$N" "$db" 21006 0,1,2,3,4,5
}
step_history() {
  m history "$RUNS" --label "no history" --also "history=env NUCLEUS_HISTORY=3600 target/release/examples/bench" -- \
    target/release/examples/bench
  m history-sql "$RUNS" --label "no history" --also "history=python3 bench/inserts.py --nucleus $N --history 1h" -- \
    python3 bench/inserts.py --nucleus "$N"
}
step_branches() { m branches "$LONG" -- target/release/examples/branches; }

# --- embedded editions
step_python() { m python "$RUNS" -- python3 bench/embedded/python.py --nucleus "$N"; }
step_wasm() { m wasm "$RUNS" -- node bench/embedded/wasm.mjs; }
step_browser() { m browser "$RUNS" -- node drivers/js/test/browser.mjs; }

if [ "${1:-}" = list ]; then
  echo build $STEPS | tr ' ' '\n'
  exit 0
fi
todo=${*:-build $STEPS}
for s in $todo; do
  echo "== $s $(date '+%H:%M:%S')" >&2
  "step_$s"
done
