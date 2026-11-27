#!/usr/bin/env python3
"""`INSERT … ON CONFLICT` measured against PostgreSQL (PLAN.md §M15 D2).

    python3 bench/upsert.py --nucleus target/release/nucleus [--rows 200000]

A table `t (id int PRIMARY KEY, n int, tag text UNIQUE)` holding --rows
rows; each statement offers --rows rows, half of them there already:
- `insert`: the same number of new rows, a plain `INSERT … SELECT` (the
  floor an upsert is compared with);
- `do update`: `ON CONFLICT (id) DO UPDATE SET n = t.n + excluded.n`;
- `do nothing`: `ON CONFLICT (id) DO NOTHING`;
- `by tag`: `ON CONFLICT (tag) DO UPDATE …` — the arbiter a secondary
  unique index.
Each timed on a table loaded afresh, the median of five; nucleus as a
server of the given binary (its Python client's clock), PostgreSQL 18
native and tuned (`bench/pg.py`, `\\timing`'s clock in psql).  Every time
is printed as an `@metric` line too, for `bench/method.py run`.
"""

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "drivers", "python"))
sys.path.insert(0, HERE)

import method  # noqa: E402
import nucleus  # noqa: E402
from pg import DEFAULT, Cluster  # noqa: E402

RUNS = 5


def statements(rows):
    """Setup, then each case's statement, by name."""
    half = rows // 2
    setup = [
        "DROP TABLE IF EXISTS t",
        "CREATE TABLE t (id int PRIMARY KEY, n int, tag text UNIQUE)",
        f"INSERT INTO t SELECT i, 1, 'tag' || i FROM generate_series(1, {rows}) i",
    ]
    # Offered: ids half there (the upper half), half new.
    offered = f"SELECT i, 1, 'tag' || i FROM generate_series({half + 1}, {rows + half}) i"
    cases = {
        "insert": f"INSERT INTO t SELECT i, 1, 'tag' || i FROM generate_series({rows + 1}, {2 * rows}) i",
        "do update": f"INSERT INTO t {offered} ON CONFLICT (id) DO UPDATE SET n = t.n + excluded.n",
        "do nothing": f"INSERT INTO t {offered} ON CONFLICT (id) DO NOTHING",
        "by tag": f"INSERT INTO t {offered} ON CONFLICT (tag) DO UPDATE SET n = t.n + excluded.n",
    }
    return setup, cases


def run_nucleus(binary, rows):
    setup, cases = statements(rows)
    work = tempfile.mkdtemp(prefix="nucleus-upsert-")
    server = subprocess.Popen([binary, "server", os.path.join(work, "db"), "--listen", "127.0.0.1:20238"],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    out = {}
    try:
        for _ in range(60):
            try:
                db = nucleus.connect("nucleus://nucleus:nucleus@127.0.0.1:20238/nucleus")
                break
            except Exception:
                time.sleep(0.5)
        for name, sql in cases.items():
            times = []
            for _ in range(RUNS + 1):
                for s in setup:
                    db.execute(s)
                db.execute("VACUUM t")
                start = time.perf_counter()
                db.execute(sql)
                times.append(time.perf_counter() - start)
                want = rows + rows // 2 if name in ("do nothing", "do update", "by tag") else 2 * rows
                got = db.execute("SELECT count(*) FROM t").rows[0][0]
                assert got == want, (name, got, want)
            out[name] = times[1:]
    finally:
        server.terminate()
        server.wait()
        shutil.rmtree(work, ignore_errors=True)
    return out


def run_postgres(rows):
    setup, cases = statements(rows)
    out = {}
    with Cluster(DEFAULT) as pg:
        for name, sql in cases.items():
            times = []
            for _ in range(RUNS + 1):
                script = ";\n".join(setup) + ";\nVACUUM ANALYZE t;\n\\timing on\n" + sql + ";\n"
                ms = re.findall(r"Time: ([0-9.]+) ms", subprocess.run(
                    pg.tool("psql") + ["-X", "-q", "-v", "ON_ERROR_STOP=1"], input=script,
                    capture_output=True, text=True, check=True).stdout)
                times.append(float(ms[-1]) / 1000)
            out[name] = times[1:]
    return out


def main():
    a = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    a.add_argument("--nucleus", required=True)
    a.add_argument("--rows", type=int, default=200_000)
    args = a.parse_args()
    nuc = run_nucleus(args.nucleus, args.rows)
    pg = run_postgres(args.rows)
    print(f"rows {args.rows}, half of those offered there already; median of {RUNS}, ms")
    print(f"{'':12} {'nucleus':>9} {'postgres':>9} {'ratio':>6}")
    for name in nuc:
        for system, t in (("nucleus", nuc[name]), ("postgresql", pg[name])):
            method.emit(name, t, unit="s", system=system, phase="write")
        n, p = method.median(nuc[name]), method.median(pg[name])
        print(f"{name:12} {n * 1000:9.1f} {p * 1000:9.1f} {n / p:6.2f}")


if __name__ == "__main__":
    main()
