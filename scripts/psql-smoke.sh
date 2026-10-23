#!/usr/bin/env bash
# Starts `nucleus server` and drives it with the real psql (PLAN.md §M12).
#
#   scripts/psql-smoke.sh <path-to-nucleus-binary>
set -euo pipefail
bin=${1:?usage: scripts/psql-smoke.sh <nucleus binary>}
work=$(mktemp -d)
port=54329
# A self-signed certificate, so TLS is offered (and checked below).
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$work/key.pem" -out "$work/cert.pem" -days 1 \
  -subj "/CN=127.0.0.1" >/dev/null 2>&1
"$bin" server "$work/db" --listen 127.0.0.1:0 --pg-compat "127.0.0.1:$port" \
  --tls-cert "$work/cert.pem" --tls-key "$work/key.pem" 2>"$work/server.log" &
pid=$!
trap 'kill $pid 2>/dev/null; rm -rf "$work"' EXIT
export PGHOST=127.0.0.1 PGPORT=$port PGUSER=nucleus PGPASSWORD=nucleus PGDATABASE=nucleus PGCONNECT_TIMEOUT=2
for _ in $(seq 50); do psql -X -At -c 'SELECT 1' >/dev/null 2>&1 && break; sleep 0.2; done

psql -X -v ON_ERROR_STOP=1 -q <<'SQL'
CREATE TABLE people (id INT PRIMARY KEY, name TEXT NOT NULL, city TEXT);
INSERT INTO people VALUES (1, 'Anna', 'Budapest'), (2, 'Béla', 'Szeged'), (3, 'Csilla', NULL);
CREATE INDEX people_city ON people (city);
BEGIN;
UPDATE people SET city = 'Pécs' WHERE id = 3;
COMMIT;
SQL

got=$(psql -X -At -F '|' -c "SELECT id, name, coalesce(city, '-') FROM people ORDER BY id")
want=$'1|Anna|Budapest\n2|Béla|Szeged\n3|Csilla|Pécs'
[ "$got" = "$want" ] || { echo "unexpected output:"; echo "$got"; exit 1; }

# Errors come back with their SQLSTATE, and the session survives them.
if psql -X -v ON_ERROR_STOP=1 -c "INSERT INTO people VALUES (1, 'dup', 'x')" 2>"$work/err"; then
  echo "duplicate key accepted"; exit 1
fi
grep -q 'duplicate key value violates unique constraint' "$work/err"
[ "$(psql -X -At -c 'SELECT count(*) FROM people')" = 3 ]

# psql's \d family reads the pg_catalog compatibility views (PLAN.md §M14.5).
psql -X -v ON_ERROR_STOP=1 -q -c "CREATE TABLE dept (id INT PRIMARY KEY, name TEXT UNIQUE)" \
  -c "ALTER TABLE people ADD COLUMN dept_id INT REFERENCES dept ON DELETE SET NULL"
describe() { psql -X -v ON_ERROR_STOP=1 -c "$1" 2>&1 || { echo "psql $1 failed"; exit 1; }; }
expect() { grep -qF -- "$2" <<<"$1" || { echo "missing in output: $2"; echo "$1"; exit 1; }; }
out=$(describe '\d people')
expect "$out" 'Table "public.people"'
expect "$out" '"people_pkey" PRIMARY KEY, btree (id)'
expect "$out" '"people_city" btree (city)'
expect "$out" 'FOREIGN KEY (dept_id) REFERENCES dept(id) ON DELETE SET NULL'
out=$(describe '\d dept')
expect "$out" '"dept_name_key" UNIQUE CONSTRAINT, btree (name)'
expect "$out" 'TABLE "people" CONSTRAINT "people_dept_id_fkey"'
expect "$(describe '\d')" 'people'
expect "$(describe '\dt')" 'dept'
expect "$(describe '\di')" 'people_city'
expect "$(describe '\dn')" 'nc_catalog'
expect "$(describe '\l')" 'nucleus'
expect "$(describe '\d+ people')" 'Access method: heap'
[ "$(psql -X -At -c "SELECT name FROM nc_catalog.tables ORDER BY name")" = $'dept\npeople' ]

# TLS on request (and SCRAM inside it); plain still works.
[ "$(PGSSLMODE=require psql -X -At -c 'SELECT 1')" = 1 ]
# SCRAM-SHA-256-PLUS: the proof bound to the TLS session.
[ "$(PGSSLMODE=require PGCHANNELBINDING=require psql -X -At -c 'SELECT 1')" = 1 ]
expect "$(PGSSLMODE=require psql -X -c '\conninfo')" 'SSL'
[ "$(PGSSLMODE=disable psql -X -At -c 'SELECT 1')" = 1 ]

echo "psql smoke test passed ($(psql --version))"
