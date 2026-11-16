#!/usr/bin/env python3
"""TPC-H on nucleus and on PostgreSQL (PLAN.md §M14.20 V10, by §M14.31
P1's method): the same generated data (bench/tpch_data.py), schema and
indexes (primary keys, and the foreign keys' columns, as the
specification allows), the 22 queries with their validation parameters
(bench/tpch_queries.py), each system's results checked against the
other's.

    bench/tpch.py --nucleus target/release/nucleus [--sf 0.1]
                  [--storage row|column|both] [--runs 5] [--timeout 600]
                  [--pg-data target/bench-pg | --psql "psql …" | --no-pg]
                  [--data DIR] [--out bench/results/tpch.json]

nucleus runs as `nucleus server` on a directory of its own, driven by
`nucleus sql -h`; PostgreSQL native on this machine in a cluster of its
own tuned for it (bench/pg.py), or one already running through --psql.
Loads are `COPY … FROM STDIN` of the same files, then the indexes and
`ANALYZE` (PostgreSQL: `VACUUM ANALYZE`, as its documentation has it
after a bulk load), --runs times each with the systems in turn.  Then each query
--runs rounds, the systems in turn: restarted, the query cold, then warm,
each time from the client's own `\\timing` in a session kept open
(bench/method.py: medians, intervals, spread).  Results are compared with
numbers rounded to cents (as the specification compares them) and rows
as sets where the ordering leaves ties.  The output is a Markdown table;
--out writes every time with the environment.
"""

import argparse
import os
import shutil
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import method  # noqa: E402
from common import (COLD, NucleusSystem, PostgresSystem, check_answers, loads,  # noqa: E402
                    queries_by_method, report)
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
    """The tables, their rows, the indexes, `ANALYZE`, on an empty
    database."""
    system.fresh(TABLES)
    ddl = []
    for stmt in SCHEMA.split(";"):
        if stmt.strip():
            table = stmt.split()[2]
            with_ = " WITH (storage = column)" if column and table in COLUMNAR else ""
            ddl.append(stmt.replace("{with_}", with_))
    system.script(ddl)
    for t in TABLES:
        system.copy(t, os.path.join(data, t + ".tbl"), COPY)
    system.script(INDEXES + [system.after_load])


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--nucleus", required=True)
    ap.add_argument("--sf", type=float, default=0.1)
    ap.add_argument("--storage", choices=["row", "column", "both"], default="both")
    ap.add_argument("--pg-data", default=os.path.join(method.ROOT, "target", "bench-pg"))
    ap.add_argument("--psql", default=None, help="a running PostgreSQL's psql command instead")
    ap.add_argument("--no-pg", action="store_true")
    ap.add_argument("--runs", type=int, default=method.RUNS)
    ap.add_argument("--retries", type=int, default=method.RETRIES)
    ap.add_argument("--timeout", type=float, default=600, help="seconds per query run")
    ap.add_argument("--data", default=None, help="a directory of generated files to reuse")
    ap.add_argument("--queries", default=None, help="some of the queries (Q1,Q6,…)")
    ap.add_argument("--out", "--json", dest="out", default=None)
    a = ap.parse_args()

    work = tempfile.mkdtemp(prefix="nucleus-tpch-")
    data = a.data or os.path.join(work, "data")
    if not a.data or not os.path.exists(os.path.join(data, "lineitem.tbl")):
        started = time.time()
        generate(a.sf, data)
        print(f"generated SF {a.sf} in {time.time() - started:.1f} s", file=sys.stderr)

    queries = {q: sql for q, sql in QUERIES.items() if not a.queries or q in a.queries.split(",")}
    storages = ["row", "column"] if a.storage == "both" else [a.storage]
    systems = []
    try:
        for storage in storages:
            systems.append(NucleusSystem(a.nucleus, f"nucleus ({storage})"))
        if not a.no_pg:
            systems.append(PostgresSystem(a.pg_data, a.psql))
        env = method.environment({s.name: s.describe() for s in systems}, data=[data], where=systems[0].work)
        env["cold"] = COLD
        load_samples = loads(systems, lambda s: load(s, data, s.name == "nucleus (column)"), a.runs)
        samples, answers = queries_by_method(systems, queries, a.runs, a.timeout, a.retries)
    finally:
        for s in systems:
            s.close()
        shutil.rmtree(work, ignore_errors=True)

    checks = check_answers(answers, queries)
    report(f"TPC-H SF {a.sf:g}", samples, load_samples, checks, env, None if a.no_pg else "postgresql")
    if a.out:
        method.write(a.out, "tpch", env, samples + load_samples,
                     {"baseline": None if a.no_pg else "postgresql", "sf": a.sf, "checks": checks})
    if any("DIFFERENT" in c for c in checks.values()):
        sys.exit("some results differ")


if __name__ == "__main__":
    main()
