#!/usr/bin/env python3
"""JSON Schema checks in SQL measured (PLAN.md §M14.15 J5).

    python3 bench/json/schema.py --nucleus target/release/nucleus [--rows 100000]

Made-up tool calls (as `bench/json/src/bin/schema.rs` makes them, one in
ten not fitting): loaded into a table without a check and into one with
`CHECK (json_matches_schema(args, '<schema>'))` (the fitting ones; the
others refused one by one), and `count(*) … WHERE
json_matches_schema(args, '<schema>')` against `count(*)` over the
table.  The client's clock, the median of five; nucleus as a server of
the given binary.
"""

import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
sys.path.insert(0, os.path.join(ROOT, "drivers", "python"))
import nucleus  # noqa: E402

SCHEMA = {
    "$defs": {"place": {"type": "object", "properties": {
        "city": {"type": "string", "minLength": 1, "maxLength": 80},
        "country": {"type": "string", "pattern": "^[A-Z]{2}$"}},
        "required": ["city"], "additionalProperties": False}},
    "type": "object",
    "properties": {
        "place": {"$ref": "#/$defs/place"},
        "unit": {"enum": ["celsius", "fahrenheit"]},
        "days": {"type": "integer", "minimum": 1, "maximum": 14},
        "hours": {"type": "array", "items": {"type": "integer", "minimum": 0, "maximum": 23},
                  "uniqueItems": True, "maxItems": 24},
        "detail": {"anyOf": [{"const": "brief"}, {"type": "object", "properties": {
            "wind": {"type": "boolean"}, "rain": {"type": "boolean"}}}]},
    },
    "required": ["place", "unit"],
    "additionalProperties": False,
}


def document(i):
    unit = "kelvin" if i % 10 == 7 else ("celsius" if i % 2 == 0 else "fahrenheit")
    detail = "brief" if i % 3 == 0 else {"wind": i % 2 == 0, "rain": True}
    return {"place": {"city": f"city{i % 5000}", "country": "HU"}, "unit": unit,
            "days": i % 14 + 1, "hours": [(h * 5 + i) % 24 for h in range(i % 6)], "detail": detail}


def median(f, n=5):
    f()
    t = []
    for _ in range(n):
        s = time.perf_counter()
        f()
        t.append(time.perf_counter() - s)
    return statistics.median(t)


def main():
    a = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    a.add_argument("--nucleus", required=True)
    a.add_argument("--rows", type=int, default=100_000)
    args = a.parse_args()
    docs = [document(i) for i in range(args.rows)]
    fit = [(i, d) for i, d in enumerate(docs) if i % 10 != 7]
    schema = json.dumps(SCHEMA).replace("'", "''")
    work = tempfile.mkdtemp(prefix="nucleus-json-schema-")
    server = subprocess.Popen([args.nucleus, "server", os.path.join(work, "db"), "--listen",
                               "127.0.0.1:20237"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(60):
            try:
                db = nucleus.connect("nucleus://nucleus:nucleus@127.0.0.1:20237/nucleus")
                break
            except Exception:
                time.sleep(0.5)

        def load(check):
            db.execute("DROP TABLE IF EXISTS c")
            db.execute("CREATE TABLE c (id int PRIMARY KEY, args json"
                       + (f" CHECK (json_matches_schema(args, '{schema}'))" if check else "") + ")")
            for k in range(0, len(fit), 5000):
                db.execute_many("INSERT INTO c VALUES ($1, $2)", fit[k:k + 5000])

        plain = median(lambda: load(False), 3)
        checked = median(lambda: load(True), 3)
        refused = 0
        for i, d in enumerate(docs[:2000]):
            if i % 10 == 7:
                try:
                    db.execute("INSERT INTO c VALUES ($1, $2)", [args.rows + i, d])
                except nucleus.Error as e:
                    assert e.code == "23514", e
                    refused += 1
        assert refused == 200, refused
        n = db.execute("SELECT count(*) FROM c").rows[0][0]
        assert n == len(fit), n
        count = median(lambda: db.execute("SELECT count(*) FROM c"))
        q = f"SELECT count(*) FROM c WHERE json_matches_schema(args, '{schema}')"
        assert db.execute(q).rows[0][0] == len(fit)
        matches = median(lambda: db.execute(q))
        q = f"SELECT sum(cardinality(json_schema_errors(args, '{schema}'))) FROM c"
        errors = median(lambda: db.execute(q))
        print(f"rows {len(fit)}")
        print(f"load without a check   {plain:8.3f} s")
        print(f"load with the CHECK    {checked:8.3f} s  (+{(checked - plain) / len(fit) * 1e9:.0f} ns a row)")
        print(f"count(*)               {count * 1000:8.1f} ms")
        print(f"count matching         {matches * 1000:8.1f} ms  ({(matches - count) / len(fit) * 1e9:.0f} ns a row)")
        print(f"sum of errors          {errors * 1000:8.1f} ms")
    finally:
        server.terminate()
        server.wait()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
