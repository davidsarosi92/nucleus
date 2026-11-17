#!/usr/bin/env python3
"""ClickBench on nucleus and on PostgreSQL (PLAN.md §M14.20 V10, by
§M14.31 P1's method): the
benchmark's `hits` table (its PostgreSQL schema) and 43 queries, taken
from the ClickBench repository at a fixed commit, on the first --rows rows
of its data (`hits.tsv.gz`, read as a stream: no need for all 100
million), each system's results checked against the other's.

    bench/clickbench.py --nucleus target/release/nucleus [--rows 1000000]
                        [--storage row|column|both] [--runs 5] [--timeout 600]
                        [--pg-data target/bench-pg | --psql "psql …" | --no-pg]
                        [--cache dir] [--out bench/results/clickbench.json]

The schema, the queries and the data are downloaded into --cache (kept
between runs).  nucleus runs as `nucleus server`, driven by `nucleus sql
-h`; PostgreSQL native in a cluster of its own tuned for the machine
(bench/pg.py), or one already running through --psql.  Loads are `COPY …
FROM STDIN` of the same file, then `ANALYZE` (PostgreSQL: `VACUUM ANALYZE`, as its documentation
has it after a bulk load), --runs times with the
systems in turn; each query --runs rounds, the systems in turn,
restarted, the query cold and then warm (bench/method.py: medians,
intervals, spread).  A query whose LIMIT may cut ties differently is
reported, not failed, when the rows differ.  ClickBench is Apache-2.0
(https://github.com/ClickHouse/ClickBench); nothing of it is stored in
this repository.
"""

import argparse
import os
import subprocess
import sys
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import method  # noqa: E402
from common import (COLD, NucleusSystem, PostgresSystem, check_answers, loads,  # noqa: E402
                    queries_by_method, report)

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
    system.fresh(["hits"])
    ddl = schema.strip().rstrip(";")
    if column:
        ddl += " WITH (storage = column)"
    system.script([ddl])
    system.copy("hits", path)
    system.script([system.after_load])


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--nucleus", required=True)
    ap.add_argument("--rows", type=int, default=1_000_000)
    ap.add_argument("--storage", choices=["row", "column", "both"], default="both")
    ap.add_argument("--pg-data", default=os.path.join(method.ROOT, "target", "bench-pg"))
    ap.add_argument("--psql", default=None, help="a running PostgreSQL's psql command instead")
    ap.add_argument("--no-pg", action="store_true")
    ap.add_argument("--runs", type=int, default=method.RUNS)
    ap.add_argument("--retries", type=int, default=method.RETRIES)
    ap.add_argument("--timeout", type=float, default=600, help="seconds per query run")
    ap.add_argument("--cache", default=os.path.join(method.ROOT, "target", "clickbench"))
    ap.add_argument("--queries", default=None, help="some of the queries (Q1,Q6,…)")
    ap.add_argument("--out", "--json", dest="out", default=None)
    a = ap.parse_args()

    os.makedirs(a.cache, exist_ok=True)
    schema = fetch(a.cache, "create.sql")
    lines = [q.strip() for q in fetch(a.cache, "queries.sql").splitlines() if q.strip()]
    queries = {f"Q{i}": q.rstrip(";") for i, q in enumerate(lines)}
    queries = {q: sql for q, sql in queries.items() if not a.queries or q in a.queries.split(",")}
    limited = {q for q, sql in queries.items() if " limit " in sql.lower()}
    path = data(a.cache, a.rows)

    storages = ["row", "column"] if a.storage == "both" else [a.storage]
    systems = []
    try:
        for storage in storages:
            systems.append(NucleusSystem(a.nucleus, f"nucleus ({storage})"))
        if not a.no_pg:
            systems.append(PostgresSystem(a.pg_data, a.psql))
        env = method.environment({s.name: s.describe() for s in systems}, data=[path], where=systems[0].work)
        env["cold"] = COLD
        env["clickbench"] = COMMIT
        load_samples = loads(systems, lambda s: load(s, schema, path, s.name == "nucleus (column)"), a.runs)
        plans = {}
        samples, answers = queries_by_method(systems, queries, a.runs, a.timeout, a.retries, plans)
    finally:
        for s in systems:
            s.close()

    checks = check_answers(answers, queries, exact=lambda q: q not in limited)
    baseline = None if a.no_pg else "postgresql"
    report(f"ClickBench, {a.rows:,} rows", samples, load_samples, checks, env, baseline, "load (COPY, ANALYZE; PostgreSQL VACUUM ANALYZE)")
    if a.out:
        method.write(a.out, "clickbench", env, samples + load_samples,
                     {"baseline": baseline, "rows": a.rows, "checks": checks, "plans": plans})
    if any("DIFFERENT" in c for c in checks.values()):
        sys.exit("some results differ")


if __name__ == "__main__":
    main()
