#!/usr/bin/env python3
"""TPC-H and ClickBench step by step, for comparing two builds of nucleus
(PLAN.md §M14.20 V14), by §M14.31 P1's method: how each step of a change
is measured.

    bench/phases.py load  --nucleus BIN [--base BIN] [--pg] --data DIR
                          [--storage row|column] [--runs 5] [--out F]
    bench/phases.py reads --nucleus BIN [--base BIN] [--pg] --data DIR
                          [--storage row|column] [--runs 5] [--queries Q1,Q6] [--vacuum]
    bench/phases.py clickbench --nucleus BIN [--base BIN] [--pg] --data DIR
                          [--storage row|column] [--runs 5] [--queries Q4,Q32]

The systems — the build, the build before it (--base), PostgreSQL
(--pg: native, bench/pg.py) — run in turn, round by round
(bench/method.py: medians, intervals, spread, `≈` where intervals
overlap).  `load` times each round's `COPY` of every table, the indexes
and `ANALYZE` on an empty database.  `reads` loads once, then times each
of the 22 queries cold (its server restarted) and warm — and with
--vacuum again after `VACUUM` (reads before and after a load's intents
become versions).  DIR holds generated `*.tbl` files (`bench/tpch.py
--data DIR` makes and keeps them; SF 1 is what the docs report).
`clickbench` loads `hits` and times its 43 queries the same way; DIR is
`bench/clickbench.py`'s --cache, which holds `create.sql`, `queries.sql`
and `hits-N.tsv` (the largest N there; 1,000,000 is what the docs
report).

To compare a change with what it changes, build the commit before it in a
worktree with its own target directory:

    git worktree add ../base HEAD~1
    (cd ../base && CARGO_TARGET_DIR=$PWD/../nucleus/target/base cargo build --release)
    bench/phases.py reads --nucleus target/release/nucleus --base target/base/release/nucleus --data DIR

Profiles: macOS `sample <server pid> <seconds> -file out.txt` while a
query repeats.
"""

import argparse
import glob
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import method  # noqa: E402
import tpch  # noqa: E402
from common import COLD, NucleusSystem, PostgresSystem, queries_by_method  # noqa: E402
from tpch_queries import QUERIES as BY_NUMBER  # noqa: E402


def schema(s, column):
    ddl = []
    for stmt in tpch.SCHEMA.split(";"):
        if stmt.strip():
            table = stmt.split()[2]
            with_ = " WITH (storage = column)" if column and table in tpch.COLUMNAR else ""
            ddl.append(stmt.replace("{with_}", with_))
    s.script(ddl)


def load(a, systems):
    """Each round: an empty database, then COPY, the indexes, ANALYZE,
    timed apart."""
    times = {}
    for r in range(a.runs):
        for s in method.turn(systems, r):
            s.fresh(tpch.TABLES)
            schema(s, a.storage == "column" and s.name != "postgresql")
            st = time.time()
            for t in tpch.TABLES:
                s.copy(t, os.path.join(a.data, t + ".tbl"), tpch.COPY)
            copy = time.time() - st
            s.script(tpch.INDEXES)
            index = time.time() - st - copy
            s.script([s.after_load])
            analyze = time.time() - st - copy - index
            for name, t in (("COPY", copy), ("CREATE INDEX", index), (s.after_load, analyze),
                            ("total", copy + index + analyze)):
                times.setdefault((name if name != "VACUUM ANALYZE" else "ANALYZE", s.name), []).append(t)
            print(f"  {s.name} round {r + 1}: {copy:.1f} + {index:.1f} + {analyze:.1f} s", file=sys.stderr, flush=True)
    return [method.Sample(name, system, t, "write", flagged=method.cv(t) > method.CV_BOUND, unit="s")
            for (name, system), t in times.items()]


def reads(a, systems):
    for s in systems:
        tpch.load(s, a.data, column=a.storage == "column" and s.name != "postgresql")
    qs = {f"Q{k}": sql for k, sql in BY_NUMBER.items()}
    return queried(a, systems, qs)


def clickbench(a, systems):
    with open(os.path.join(a.data, "create.sql")) as f:
        ddl = f.read().strip().rstrip(";")
    with open(os.path.join(a.data, "queries.sql")) as f:
        lines = [q.strip() for q in f if q.strip()]
    data = max(glob.glob(os.path.join(a.data, "hits-*.tsv")), key=lambda p: int(p.rsplit("-", 1)[1][:-4]))
    for s in systems:
        s.fresh(["hits"])
        s.script([ddl + (" WITH (storage = column)" if a.storage == "column" and s.name != "postgresql" else "")])
        s.copy("hits", data)
        s.script([s.after_load])
    qs = {f"Q{i}": q.rstrip(";") for i, q in enumerate(lines)}
    return queried(a, systems, qs)


def queried(a, systems, qs):
    if a.queries:
        qs = {q: qs[q] for q in a.queries.split(",")}
    samples, _ = queries_by_method(systems, qs, a.runs, a.timeout, a.retries)
    if a.vacuum:
        for s in systems:
            s.script(["VACUUM"])
        after, _ = queries_by_method(systems, {f"{q} vacuumed": sql for q, sql in qs.items()}, a.runs,
                                     a.timeout, a.retries)
        samples += after
    return samples


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("what", choices=["load", "reads", "clickbench"])
    ap.add_argument("--nucleus", required=True)
    ap.add_argument("--base", default=None, help="the build to compare with")
    ap.add_argument("--pg", action="store_true", help="PostgreSQL beside them")
    ap.add_argument("--pg-data", default=os.path.join(method.ROOT, "target", "bench-pg"))
    ap.add_argument("--data", required=True,
                    help="TPC-H: a directory of generated *.tbl files; clickbench: bench/clickbench.py's --cache")
    ap.add_argument("--storage", choices=["row", "column"], default="row")
    ap.add_argument("--runs", type=int, default=method.RUNS)
    ap.add_argument("--retries", type=int, default=method.RETRIES)
    ap.add_argument("--queries", default=None, help="e.g. Q1,Q6 (default: all)")
    ap.add_argument("--vacuum", action="store_true", help="reads: again after VACUUM")
    ap.add_argument("--timeout", type=float, default=600)
    ap.add_argument("--out", default=None, help="the results file")
    a = ap.parse_args()
    systems = []
    try:
        systems.append(NucleusSystem(a.nucleus, "nucleus"))
        if a.base:
            systems.append(NucleusSystem(a.base, "base"))
        if a.pg:
            systems.append(PostgresSystem(a.pg_data))
        env = method.environment({s.name: dict(s.describe(), binary=getattr(s, "binary", None))
                                  for s in systems}, data=[a.data], where=systems[0].work)
        env["cold"] = COLD
        samples = {"load": load, "reads": reads, "clickbench": clickbench}[a.what](a, systems)
    finally:
        for s in systems:
            s.close()
    baseline = "base" if a.base else ("postgresql" if a.pg else None)
    print(method.table(samples, baseline=baseline, title=f"{a.what} ({a.storage})"))
    print()
    print(method.describe(env))
    if a.out:
        method.write(a.out, f"phases {a.what}", env, samples, {"baseline": baseline, "storage": a.storage})


if __name__ == "__main__":
    main()
