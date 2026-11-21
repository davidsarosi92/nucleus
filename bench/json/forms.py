#!/usr/bin/env python3
"""The document form in SQL (PLAN.md §M15 D1): nucleus's `json` against
PostgreSQL's `jsonb` on the same documents.

    python3 bench/json/forms.py --nucleus target/release/nucleus \\
        --forms target/json/release/forms [--small 100000] [--large 10000]

The documents are `forms --emit`'s (bench/json/src/bin/forms.rs): users
as applications keep them, ~1 KB and ~16 KB, fields in the order written.
Each kind in a table of both, then timed (the median of five runs):

- field: `sum(length(doc ->> k))` — the second field written, the middle
  one and the last (jsonb, like nucleus, sorts an object's keys);
- path: `sum(length(doc -> 'address' -> 'geo' ->> 'lat'))`;
- output: `sum(length(doc::text))`, the whole document as text;
- size: `sum(pg_column_size(doc))` for jsonb (TOAST-compressed past
  ~2 KB); nucleus's form's size is `forms`'s.

PostgreSQL runs native (bench/pg.py's cluster); nucleus as a server of
the given binary.  One at a time.  Every time is an `@metric` line too.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
sys.path.insert(0, os.path.join(ROOT, "drivers", "python"))
sys.path.insert(0, os.path.join(ROOT, "bench"))
import method  # noqa: E402
import nucleus  # noqa: E402
import pg  # noqa: E402

CLUSTER = None


def times(f, n=5):
    f()
    t = []
    for _ in range(n):
        s = time.perf_counter()
        f()
        t.append(time.perf_counter() - s)
    return t


def psql(sql):
    return CLUSTER.sql(file=sql + "\n").strip()


def pg_times(sql, n=5):
    out = CLUSTER.sql(file="\\timing on\n" + (sql + "\n") * (n + 1))
    ms = [float(line.split()[1]) for line in out.splitlines() if line.startswith("Time:")]
    return [m / 1000 for m in ms[1:]]


def main():
    a = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    a.add_argument("--nucleus", required=True)
    a.add_argument("--forms", required=True)
    a.add_argument("--small", type=int, default=100_000)
    a.add_argument("--large", type=int, default=10_000)
    args = a.parse_args()
    global CLUSTER
    CLUSTER = pg.Cluster(pg.DEFAULT).__enter__()
    method.emit_system("postgresql", CLUSTER.describe())
    work = tempfile.mkdtemp(prefix="nucleus-forms-bench-")
    server = subprocess.Popen([args.nucleus, "server", os.path.join(work, "db"), "--listen",
                               "127.0.0.1:20236"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(60):
            try:
                db = nucleus.connect("nucleus://nucleus:nucleus@127.0.0.1:20236/nucleus")
                break
            except Exception:
                time.sleep(0.5)
        print(f"{'kind':<6} {'rows':>7} {'MB':>6} | {'read':<26} {'nucleus ms':>10} {'pg ms':>9}")
        for kind, n in (("small", args.small), ("large", args.large)):
            label = "1 KB" if kind == "small" else "16 KB"
            lines = subprocess.run([args.forms, "--emit", kind, str(n)], check=True,
                                   capture_output=True, text=True).stdout.splitlines()
            keys, texts = json.loads(lines[0]), lines[1:]  # the keys as written
            mb = sum(len(t.encode()) for t in texts) / 1e6
            psql("DROP TABLE IF EXISTS src; CREATE TABLE src (id int, doc text);")
            copy = "".join(f"{i}\t{t.replace(chr(92), chr(92) * 2)}\n" for i, t in enumerate(texts))
            CLUSTER.sql(file="COPY src FROM STDIN;\n" + copy + "\\.\n")
            psql("DROP TABLE IF EXISTS j; CREATE TABLE j AS SELECT id, doc::jsonb AS doc FROM src; "
                 "DROP TABLE src; VACUUM ANALYZE j;")
            db.execute("DROP TABLE IF EXISTS src")
            db.execute("DROP TABLE IF EXISTS j")
            db.execute("CREATE TABLE src (id int PRIMARY KEY, doc text)")
            for k in range(0, len(texts), 2000):
                db.execute_many("INSERT INTO src VALUES ($1, $2)",
                                [(k + i, t) for i, t in enumerate(texts[k:k + 2000])])
            db.execute("CREATE TABLE j AS SELECT id, doc::json AS doc FROM src")
            db.execute("DROP TABLE src")
            reads = [(f"second field ({keys[1]})", f"doc ->> '{keys[1]}'"),
                     (f"middle field ({keys[len(keys) // 2]})", f"doc ->> '{keys[len(keys) // 2]}'"),
                     (f"last field ({keys[-1]})", f"doc ->> '{keys[-1]}'"),
                     ("path address.geo.lat", "doc -> 'address' -> 'geo' ->> 'lat'"),
                     ("whole document as text", "doc::text")]
            for what, expr in reads:
                sql = f"SELECT sum(length({expr})) FROM j"
                if what != "whole document as text":  # the texts differ in spacing and numbers' forms
                    assert str(db.execute(sql).rows[0][0]) == psql(sql + ";"), f"the same sums: {what}"
                n_t = times(lambda: db.execute(sql))
                pg_t = pg_times(sql + ";")
                name = f"{what.split(' (')[0]} ({label})"
                n_t, pg_t = [x * 1e3 for x in n_t], [x * 1e3 for x in pg_t]
                method.emit(name, n_t, unit="ms")
                method.emit(name, pg_t, unit="ms", system="postgresql")
                print(f"{label:<6} {n:>7} {mb:>6.1f} | {what:<26} {method.median(n_t):>9.2f} "
                      f"{method.median(pg_t):>9.2f}", flush=True)
            size = int(psql("SELECT sum(pg_column_size(doc)) FROM j;"))
            method.emit(f"size a document ({label})", size / n, unit="B", system="postgresql")
            print(f"{label:<6} jsonb {size / n:.0f} B a document (pg_column_size)", flush=True)
    finally:
        server.terminate()
        server.wait()
        psql("DROP TABLE IF EXISTS src; DROP TABLE IF EXISTS j;")
        CLUSTER.__exit__(None, None, None)
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
