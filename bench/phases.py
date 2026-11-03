#!/usr/bin/env python3
"""TPC-H step by step, for comparing two builds of nucleus (PLAN.md
§M14.20 V14): how each step of a change is measured.

    bench/phases.py load  --nucleus BIN --data DIR [--storage row|column]
    bench/phases.py reads --nucleus BIN --data DIR [--storage row|column]
                          [--runs 3] [--queries Q1,Q6] [--vacuum]

`load` times each table's `COPY`, each index and `ANALYZE` (the table in
docs/benchmarks.md, "The load since").  `reads` loads, then times each of
the 22 queries right after the load (`first`) and as the best of --runs
(`best`) — and with --vacuum the best again after `VACUUM` (reads before
and after a load's intents become versions).  DIR holds generated
`*.tbl` files (`bench/tpch.py --data DIR` makes and keeps them; SF 1 is
what the docs report).

To compare a change with what it changes, build the commit before it in a
worktree with its own target directory and run the two binaries one after
the other, alternating, two rounds or more — the machine's load moves the
numbers by 10 % and more between runs:

    git worktree add ../base HEAD~1
    (cd ../base && CARGO_TARGET_DIR=$PWD/../nucleus/target/base cargo build --release)
    for i in 1 2; do
      bench/phases.py reads --nucleus target/base/release/nucleus --data DIR
      bench/phases.py reads --nucleus target/release/nucleus --data DIR
    done

Profiles: macOS `sample <server pid> <seconds> -file out.txt` while a
query repeats; the server's pid is printed first.
"""

import argparse
import os
import shutil
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import tpch  # noqa: E402
from common import Nucleus, measure  # noqa: E402
from tpch_queries import QUERIES as BY_NUMBER  # noqa: E402


def schema(n, column):
    ddl = []
    for stmt in tpch.SCHEMA.split(";"):
        if stmt.strip():
            table = stmt.split()[2]
            with_ = " WITH (storage = column)" if column and table in tpch.COLUMNAR else ""
            ddl.append(stmt.replace("{with_}", with_))
    n.script(ddl)


def load(a, n):
    column = a.storage == "column"
    schema(n, column)
    total = time.time()
    for t in tpch.TABLES:
        st = time.time()
        n.copy(t, os.path.join(a.data, t + ".tbl"), tpch.COPY)
        print(f"copy {t:10s} {time.time() - st:7.2f} s", flush=True)
    for i in tpch.INDEXES + ["ANALYZE"]:
        st = time.time()
        n.script([i])
        print(f"{i[:60]:60s} {time.time() - st:7.2f} s", flush=True)
    print(f"total {time.time() - total:.2f} s", flush=True)


def reads(a, n):
    st = time.time()
    tpch.load(n, a.data, column=a.storage == "column")
    print(f"load {time.time() - st:.1f} s", flush=True)
    qs = {f"Q{k}": sql for k, sql in BY_NUMBER.items()}
    if a.queries:
        qs = {q: qs[q] for q in a.queries.split(",")}
    first = measure(n, qs, 1, a.timeout)
    best = measure(n, qs, a.runs, a.timeout)
    after = None
    if a.vacuum:
        st = time.time()
        n.script(["VACUUM"])
        print(f"vacuum {time.time() - st:.1f} s", flush=True)
        after = measure(n, qs, a.runs, a.timeout)

    def f(v):
        return f"{v:9.1f}" if v else "        —"

    print(f"{'q':4s} {'first':>9s} {'best':>9s}" + (f" {'vacuumed':>9s}" if after else ""))
    for q in qs:
        print(f"{q:4s} {f(first[q])} {f(best[q])}" + (f" {f(after[q])}" if after else ""))

    def tot(d):
        return sum(v for v in d.values() if v)

    print(f"sum  {tot(first):9.1f} {tot(best):9.1f}" + (f" {tot(after):9.1f}" if after else ""))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("what", choices=["load", "reads"])
    ap.add_argument("--nucleus", required=True)
    ap.add_argument("--data", required=True, help="a directory of generated *.tbl files")
    ap.add_argument("--storage", choices=["row", "column"], default="row")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--queries", default=None, help="e.g. Q1,Q6 (default: all 22)")
    ap.add_argument("--vacuum", action="store_true", help="reads: again after VACUUM")
    ap.add_argument("--timeout", type=float, default=600)
    a = ap.parse_args()
    work = tempfile.mkdtemp(prefix="nucleus-phases-")
    n = Nucleus(a.nucleus, a.storage, work)
    print("server pid", n.server.pid, flush=True)
    try:
        (load if a.what == "load" else reads)(a, n)
    finally:
        n.close()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
