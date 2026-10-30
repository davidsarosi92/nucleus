#!/usr/bin/env python3
"""TPC-H on nucleus and on PostgreSQL (PLAN.md §M14.20 V10): the same
generated data (bench/tpch_data.py), schema and indexes (primary keys,
and the foreign keys' columns, as the specification allows), the 22
queries with their validation parameters (bench/tpch_queries.py), each
system's results checked against the other's.

    bench/tpch.py --nucleus target/release/nucleus [--sf 0.1]
                  [--storage row|column|both] [--psql "psql …"] [--no-pg]
                  [--runs 3] [--timeout 600] [--json out.json]

nucleus runs as `nucleus server` on a fresh directory, driven by
`nucleus sql -h`; PostgreSQL through `psql` (PGHOST, PGUSER … or --psql,
e.g. "docker exec -i pg psql -U postgres").  Times come from each
client's `\\timing` (best of --runs); loads are `COPY … FROM STDIN` of the
same files, then the indexes and `ANALYZE`.  Results are compared with
numbers rounded to cents (as the specification compares them) and rows
as sets where the ordering leaves ties.  The output is a Markdown table;
--json writes the numbers.
"""

import argparse
import json
import os
import shutil
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from common import Nucleus, Postgres, measure, report  # noqa: E402
from tpch_data import SCHEMA, TABLES, generate  # noqa: E402
from tpch_queries import QUERIES as BY_NUMBER  # noqa: E402

QUERIES = {f"Q{n}": sql for n, sql in BY_NUMBER.items()}

# The foreign keys' columns (TPC-H 3.0 §1.4.2.3 allows indexes on them).
INDEXES = [
    "CREATE INDEX lineitem_part_supp ON lineitem (l_partkey, l_suppkey)",
    "CREATE INDEX lineitem_supp ON lineitem (l_suppkey)",
    "CREATE INDEX orders_cust ON orders (o_custkey)",
    "CREATE INDEX partsupp_supp ON partsupp (ps_suppkey)",
    "CREATE INDEX customer_nation ON customer (c_nationkey)",
    "CREATE INDEX supplier_nation ON supplier (s_nationkey)",
    "CREATE INDEX nation_region ON nation (n_regionkey)",
]

# The tables a column storage run stores in columns.
COLUMNAR = {"supplier", "part", "partsupp", "customer", "orders", "lineitem"}

COPY = " WITH (FORMAT csv, DELIMITER '|')"


def load(system, data, column=False):
    """The tables, their rows, the indexes, `ANALYZE`: the seconds the rows
    on took."""
    if isinstance(system, Postgres):
        system.script(["DROP TABLE IF EXISTS " + ", ".join(TABLES)])
    ddl = []
    for stmt in SCHEMA.split(";"):
        if stmt.strip():
            table = stmt.split()[2]
            with_ = " WITH (storage = column)" if column and table in COLUMNAR else ""
            ddl.append(stmt.replace("{with_}", with_))
    system.script(ddl)
    started = time.time()
    for t in TABLES:
        system.copy(t, os.path.join(data, t + ".tbl"), COPY)
    system.script(INDEXES + ["ANALYZE"])
    return time.time() - started


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--nucleus", required=True)
    ap.add_argument("--sf", type=float, default=0.1)
    ap.add_argument("--storage", choices=["row", "column", "both"], default="both")
    ap.add_argument("--psql", default="psql")
    ap.add_argument("--no-pg", action="store_true")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--timeout", type=float, default=600, help="seconds per query run")
    ap.add_argument("--data", default=None, help="a directory of generated files to reuse")
    ap.add_argument("--json", default=None)
    a = ap.parse_args()

    work = tempfile.mkdtemp(prefix="nucleus-tpch-")
    data = a.data or os.path.join(work, "data")
    if not a.data or not os.path.exists(os.path.join(data, "lineitem.tbl")):
        started = time.time()
        generate(a.sf, data)
        print(f"generated SF {a.sf} in {time.time() - started:.1f} s", file=sys.stderr)

    storages = ["row", "column"] if a.storage == "both" else [a.storage]
    systems, loads, times, results = [], {}, {}, {}
    try:
        for storage in storages:
            key = f"nucleus ({storage})"
            n = Nucleus(a.nucleus, key, work)
            try:
                loads[key] = load(n, data, storage == "column")
                print(f"{key}: loaded in {loads[key]:.1f} s", file=sys.stderr)
                times[key] = measure(n, QUERIES, a.runs, a.timeout)
                results[key] = {q: n.results(sql) if times[key][q] is not None else None
                                for q, sql in QUERIES.items()}
            finally:
                n.close()
            systems.append(key)
        if not a.no_pg:
            pg = Postgres(a.psql)
            try:
                loads["postgresql"] = load(pg, data)
                print(f"postgresql: loaded in {loads['postgresql']:.1f} s", file=sys.stderr)
                times["postgresql"] = measure(pg, QUERIES, a.runs, a.timeout)
                results["postgresql"] = {q: pg.results(sql) if times["postgresql"][q] is not None else None
                                         for q, sql in QUERIES.items()}
            finally:
                pg.script(["DROP TABLE IF EXISTS " + ", ".join(TABLES)])
            systems.append("postgresql")
    finally:
        shutil.rmtree(work, ignore_errors=True)

    table, checks = report(f"TPC-H SF {a.sf:g}", QUERIES, systems, times, results, loads,
                           "load (COPY, indexes, ANALYZE; s)")
    print(table)
    if a.json:
        with open(a.json, "w") as f:
            json.dump({"sf": a.sf, "times": times, "loads": loads, "checks": checks}, f, indent=2)
    if any("DIFFERENT" in c for c in checks.values()):
        sys.exit("some results differ")


if __name__ == "__main__":
    main()
