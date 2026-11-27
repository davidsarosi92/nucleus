#!/usr/bin/env bash
# Starts `nucleus server --mongo-compat` and drives it with a standard
# MongoDB driver (pymongo, scripts/mongo-smoke.py) and with mongosh
# (PLAN.md §M16 Q5).
#
#   scripts/mongo-smoke.sh <path-to-nucleus-binary>
#
# PYTHON: a python with pymongo (default python3).  MONGOSH: how to run
# mongosh (default: mongosh if installed, else mongo:8's in docker on the
# host's network); MONGOSH_HOST: the server's address as mongosh sees it
# (default 127.0.0.1).  With neither, that part is skipped and said so.
set -euo pipefail
bin=${1:?usage: scripts/mongo-smoke.sh <nucleus binary>}
work=$(mktemp -d)
port=${MONGO_SMOKE_PORT:-27019}
"$bin" server "$work/db" --listen 127.0.0.1:0 --mongo-compat "127.0.0.1:$port" 2>"$work/server.log" &
pid=$!
trap 'kill $pid 2>/dev/null; rm -rf "$work"' EXIT
for _ in $(seq 50); do (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && break; sleep 0.2; done

"${PYTHON:-python3}" "$(dirname "$0")/mongo-smoke.py" "mongodb://nucleus:nucleus@127.0.0.1:$port/?directConnection=true"

if [ -z "${MONGOSH:-}" ]; then
  if command -v mongosh >/dev/null; then
    MONGOSH=mongosh
  elif command -v docker >/dev/null && [ "$(uname)" = Linux ]; then
    MONGOSH="docker run --rm -i --network host mongo:8 mongosh"
  else
    echo "mongosh: not installed, skipped"
    exit 0
  fi
fi
url="mongodb://nucleus:nucleus@${MONGOSH_HOST:-127.0.0.1}:$port/shop"
# shellcheck disable=SC2086
got=$($MONGOSH --quiet "$url" --eval '
  db.smoke.drop();
  db.smoke.insertMany([{n: 1, at: new Date("2024-01-02")}, {n: 2, tags: ["a"]}, {n: 3}]);
  db.smoke.updateOne({n: 2}, {$push: {tags: "b"}});
  db.smoke.updateOne({n: 4}, {$set: {new: true}}, {upsert: true});
  db.smoke.deleteOne({n: 3});
  const docs = db.smoke.find({n: {$gte: 1}}, {_id: 0}).sort({n: 1}).toArray();
  print(JSON.stringify(docs), db.smoke.countDocuments({}), db.smoke.findOne({at: {$lt: new Date()}}).n);
' 2>&1)
# Keys come back in nucleus's canonical order (shorter first), §M15 item 3.
want='[{"n":1,"at":"2024-01-02T00:00:00.000Z"},{"n":2,"tags":["a","b"]},{"n":4,"new":true}] 3 1'
[ "$got" = "$want" ] || { echo "mongosh: unexpected output:"; echo "$got"; echo "want:"; echo "$want"; exit 1; }
echo "mongosh: CRUD and queries passed"
