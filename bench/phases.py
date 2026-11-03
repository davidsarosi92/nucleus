#!/usr/bin/env python3
"""TPC-H and ClickBench step by step, for comparing two builds of nucleus
(PLAN.md §M14.20 V14): how each step of a change is measured.

    bench/phases.py load  --nucleus BIN --data DIR [--storage row|column]
    bench/phases.py reads --nucleus BIN --data DIR [--storage row|column]
                          [--runs 3] [--queries Q1,Q6] [--vacuum]
    bench/phases.py clickbench --nucleus BIN --data DIR [--storage row|column]
                          [--runs 3] [--queries Q4,Q32]

`load` times each table's `COPY`, each index and `ANALYZE` (the table in
docs/benchmarks.md, "The load since").  `reads` loads, then times each of
the 22 queries right after the load (`first`) and as the best of --runs
(`best`) — and with --vacuum the best again after `VACUUM` (reads before
and after a load's intents become versions).  DIR holds generated
`*.tbl` files (`bench/tpch.py --data DIR` makes and keeps them; SF 1 is
what the docs report).  `clickbench` loads `hits` and times its 43
queries the same way (`first`, `best`); DIR is `bench/clickbench.py`'s
--cache, which holds `create.sql`, `queries.sql` and `hits-N.tsv` (the
largest N there; 1,000,000 is what the docs report).

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
    table(qs, first, best, after)


def clickbench(a, n):
    import glob

    with open(os.path.join(a.data, "create.sql")) as f:
        ddl = f.read().strip().rstrip(";")
    with open(os.path.join(a.data, "queries.sql")) as f:
        lines = [q.strip() for q in f if q.strip()]
    data = max(glob.glob(os.path.join(a.data, "hits-*.tsv")), key=lambda p: int(p.rsplit("-", 1)[1][:-4]))
    n.script([ddl + (" WITH (storage = column)" if a.storage == "column" else "")])
    st = time.time()
    n.copy("hits", data)
    n.script(["ANALYZE"])
    print(f"load {os.path.basename(data)} {time.time() - st:.1f} s", flush=True)
    qs = {f"Q{i}": q.rstrip(";") for i, q in enumerate(lines)}
    if a.queries:
        qs = {q: qs[q] for q in a.queries.split(",")}
    first = measure(n, qs, 1, a.timeout)
    best = measure(n, qs, a.runs, a.timeout)
    table(qs, first, best)


def table(qs, first, best, after=None):
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
    ap.add_argument("what", choices=["load", "reads", "clickbench"])
    ap.add_argument("--nucleus", required=True)
    ap.add_argument("--data", required=True, help="TPC-H: a directory of generated *.tbl files; clickbench: bench/clickbench.py's --cache")
    ap.add_argument("--storage", choices=["row", "column"], default="row")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--queries", default=None, help="e.g. Q1,Q6 (default: all)")
    ap.add_argument("--vacuum", action="store_true", help="reads: again after VACUUM")
    ap.add_argument("--timeout", type=float, default=600)
    a = ap.parse_args()
    work = tempfile.mkdtemp(prefix="nucleus-phases-")
    n = Nucleus(a.nucleus, a.storage, work)
    print("server pid", n.server.pid, flush=True)
    try:
        {"load": load, "reads": reads, "clickbench": clickbench}[a.what](a, n)
    finally:
        n.close()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
