#!/usr/bin/env bash
# Backups in object storage against a real S3 server (PLAN.md §M14.18
# O3b): rclone's `serve s3` (it checks Signature Version 4 as S3 does),
# a full backup and an incremental one written there by `BACKUP TO
# 's3://…'`, restored from there by `nucleus restore`, to each point.
#
#   scripts/s3-test.sh <nucleus binary>
#
# Needs rclone on the PATH (or RCLONE); CI downloads it.
set -euo pipefail
nucleus=$(cd "$(dirname "${1:?usage: scripts/s3-test.sh <nucleus binary>}")" && pwd)/$(basename "$1")
rclone=${RCLONE:-rclone}
work=$(mktemp -d)
pid=
cleanup() { [ -n "$pid" ] && kill "$pid" 2>/dev/null; wait 2>/dev/null || true; rm -rf "$work"; }
trap cleanup EXIT
fail() {
  echo "s3 test: $*" >&2
  for f in "$work"/*.log; do echo "== $f"; tail -20 "$f"; done >&2
  if [ -n "${GITHUB_ACTIONS:-}" ]; then
    echo "::error title=s3 test::$*"
    for f in "$work"/*.log; do
      [ -s "$f" ] || continue
      echo "::error title=$(basename "$f")::$(tail -15 "$f" | cut -c1-300 | sed 's/%/%25/g' | awk '{printf "%s%%0A", $0}')"
    done
  fi
  exit 1
}

mkdir -p "$work/store/bkt"
"$rclone" serve s3 --auth-key test-key,test-secret --addr 127.0.0.1:29000 "$work/store" \
  >"$work/rclone.log" 2>&1 &
pid=$!
for _ in $(seq 60); do curl -s -o /dev/null http://127.0.0.1:29000/ && break; sleep 0.5; done
export AWS_ACCESS_KEY_ID=test-key AWS_SECRET_ACCESS_KEY=test-secret
export AWS_ENDPOINT_URL=http://127.0.0.1:29000 AWS_REGION=us-east-1
sql() { "$nucleus" sql "$work/db" -c "$1" >>"$work/sql.log" 2>&1 || fail "$1"; }
count() { # <database>: its rows' count and sum, one line
  "$nucleus" sql "$1" -c "SELECT count(*) || ' ' || sum(id) AS n FROM t" 2>>"$work/sql.log" | sed -n 3p | tr -d ' '
}

echo "== a full backup, then what changed, into s3://bkt/db"
sql "CREATE TABLE t (id INT PRIMARY KEY, v TEXT); CREATE INDEX t_v ON t (v); INSERT INTO t SELECT g, 'v' || g FROM generate_series(1, 20000) g"
sql "BACKUP TO 's3://bkt/db'"
grep -q "000001-full" "$work/sql.log" || fail "no full backup"
first=$(count "$work/db")
sql "DELETE FROM t WHERE id > 15000; UPDATE t SET v = 'x' WHERE id < 100"
sql "BACKUP TO 's3://bkt/db'"
grep -q "000002-incr" "$work/sql.log" || fail "no incremental backup"
last=$(count "$work/db")
ls -R "$work/store/bkt" >"$work/objects.log"

echo "== restored from there, to each point"
"$nucleus" restore s3://bkt/db "$work/r1" >"$work/restore.log" 2>&1 || fail "the restore failed"
grep -q "from 2 backups" "$work/restore.log" || fail "not the chain of two"
test "$(count "$work/r1")" = "$last" || fail "the restore holds $(count "$work/r1"), not $last"
"$nucleus" restore s3://bkt/db "$work/r2" --at 000001-full >>"$work/restore.log" 2>&1 || fail "the restore to the first failed"
test "$(count "$work/r2")" = "$first" || fail "the first restored holds $(count "$work/r2"), not $first"
# A wrong secret: refused by the store.
if AWS_SECRET_ACCESS_KEY=wrong "$nucleus" restore s3://bkt/db "$work/r3" >>"$work/restore.log" 2>&1; then
  fail "a wrong secret was taken"
fi
echo "the s3 test passed: $first then $last, restored from the store"
