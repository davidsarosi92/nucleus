#!/usr/bin/env bash
# Starts `nucleus server` and drives it with the real psql (PLAN.md §M12).
#
#   scripts/psql-smoke.sh <path-to-nucleus-binary>
set -euo pipefail
bin=${1:?usage: scripts/psql-smoke.sh <nucleus binary>}
work=$(mktemp -d)
port=54329
"$bin" server "$work/db" --listen "127.0.0.1:$port" 2>"$work/server.log" &
pid=$!
trap 'kill $pid 2>/dev/null; rm -rf "$work"' EXIT
export PGHOST=127.0.0.1 PGPORT=$port PGUSER=nucleus PGDATABASE=nucleus PGCONNECT_TIMEOUT=2
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

echo "psql smoke test passed ($(psql --version))"
