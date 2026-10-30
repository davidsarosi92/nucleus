#!/usr/bin/env python3
"""ClickBench on nucleus and on PostgreSQL (PLAN.md §M14.20 V10): the
benchmark's `hits` table (its PostgreSQL schema) and 43 queries, taken
from the ClickBench repository at a fixed commit, on the first --rows rows
of its data (`hits.tsv.gz`, read as a stream: no need for all 100
million), each system's results checked against the other's.

    bench/clickbench.py --nucleus target/release/nucleus [--rows 1000000]
                        [--storage row|column|both] [--psql "psql …"]
                        [--no-pg] [--runs 3] [--timeout 600]
                        [--cache dir] [--json out.json]

The schema, the queries and the data are downloaded into --cache (kept
between runs).  nucleus runs as `nucleus server`, driven by `nucleus sql
-h`; PostgreSQL through `psql` (or --psql).  Loads are `COPY … FROM
STDIN` of the same file, then `ANALYZE`; each query's best time of
--runs.  A query whose LIMIT may cut ties differently is reported, not
failed, when the rows differ.  ClickBench is Apache-2.0
(https://github.com/ClickHouse/ClickBench); nothing of it is stored in
this repository.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from common import Nucleus, Postgres, measure, report  # noqa: E402

COMMIT = "18e5485b36227b160d11f119477bad16bad754c7"
RAW = f"https://raw.githubusercontent.com/ClickHouse/ClickBench/{COMMIT}/postgresql"
DATA = "https://datasets.clickhouse.com/hits_compatible/hits.tsv.gz"


def fetch(cache, name):
    path = os.path.join(cache, name)
    if not os.path.exists(path):
        with urllib.request.urlopen(f"{RAW}/{name}") as r, open(path + ".part", "wb") as f:
            f.write(r.read())
        os.rename(path + ".part", path)
    with open(path) as f:
        return f.read()


def data(cache, rows):
    """The first `rows` rows of the benchmark's data, as a file."""
    path = os.path.join(cache, f"hits-{rows}.tsv")
    if not os.path.exists(path):
        started = time.time()
        # A stream, cut after the rows wanted (the download stops there).
        cmd = f"curl -sfL {DATA} | gunzip -c | head -n {rows} > {path}.part"
        subprocess.run(["bash", "-o", "pipefail", "-c", cmd], check=False)
        with open(path + ".part", "rb") as f:
            got = sum(1 for _ in f)
        if got != rows:
            sys.exit(f"downloaded {got} rows of {rows}")
        os.rename(path + ".part", path)
        print(f"downloaded {rows} rows in {time.time() - started:.0f} s", file=sys.stderr)
    return path


def load(system, schema, path, column=False):
    if isinstance(system, Postgres):
        system.script(["DROP TABLE IF EXISTS hits"])
    ddl = schema.strip().rstrip(";")
    if column:
        ddl += " WITH (storage = column)"
    system.script([ddl])
    started = time.time()
    system.copy("hits", path)
    system.script(["ANALYZE"])
    return time.time() - started


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--nucleus", required=True)
    ap.add_argument("--rows", type=int, default=1_000_000)
    ap.add_argument("--storage", choices=["row", "column", "both"], default="both")
    ap.add_argument("--psql", default="psql")
    ap.add_argument("--no-pg", action="store_true")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--timeout", type=float, default=600, help="seconds per query run")
    ap.add_argument("--cache", default=os.path.join(tempfile.gettempdir(), "nucleus-clickbench"))
    ap.add_argument("--json", default=None)
    a = ap.parse_args()

    os.makedirs(a.cache, exist_ok=True)
    schema = fetch(a.cache, "create.sql")
    lines = [q.strip() for q in fetch(a.cache, "queries.sql").splitlines() if q.strip()]
    queries = {f"Q{i}": q.rstrip(";") for i, q in enumerate(lines)}
    limited = {q for q, sql in queries.items() if " limit " in sql.lower()}
    path = data(a.cache, a.rows)

    work = tempfile.mkdtemp(prefix="nucleus-clickbench-")
    storages = ["row", "column"] if a.storage == "both" else [a.storage]
    systems, loads, times, results = [], {}, {}, {}
    try:
        for storage in storages:
            key = f"nucleus ({storage})"
            n = Nucleus(a.nucleus, key, work)
            try:
                loads[key] = load(n, schema, path, storage == "column")
                print(f"{key}: loaded in {loads[key]:.1f} s", file=sys.stderr)
                times[key] = measure(n, queries, a.runs, a.timeout)
                results[key] = {q: n.results(sql) if times[key][q] is not None else None
                                for q, sql in queries.items()}
            finally:
                n.close()
            systems.append(key)
        if not a.no_pg:
            pg = Postgres(a.psql)
            try:
                loads["postgresql"] = load(pg, schema, path)
                print(f"postgresql: loaded in {loads['postgresql']:.1f} s", file=sys.stderr)
                times["postgresql"] = measure(pg, queries, a.runs, a.timeout)
                results["postgresql"] = {q: pg.results(sql) if times["postgresql"][q] is not None else None
                                         for q, sql in queries.items()}
            finally:
                pg.script(["DROP TABLE IF EXISTS hits"])
            systems.append("postgresql")
    finally:
        shutil.rmtree(work, ignore_errors=True)

    table, checks = report(f"ClickBench, {a.rows:,} rows", queries, systems, times, results, loads,
                           "load (COPY, ANALYZE; s)", exact=lambda q: q not in limited)
    print(table)
    if a.json:
        with open(a.json, "w") as f:
            json.dump({"rows": a.rows, "times": times, "loads": loads, "checks": checks}, f, indent=2)
    if any("DIFFERENT" in c for c in checks.values()):
        sys.exit("some results differ")


if __name__ == "__main__":
    main()
