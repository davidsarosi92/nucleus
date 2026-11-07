#!/usr/bin/env python3
"""nucleus's HNSW index through SQL (PLAN.md §M14.12 V4b) on a dataset in
bench/ann/data.py's format, run as bench/ann/pgvector_ref.py runs
pgvector: the first rows loaded and indexed, a recall curve, rows
inserted by concurrent writers while a searcher asks, the curve again.

    bench/ann/sql.py BINARY DIR [--rows N] [--bulk N] [--queries 2000]
        [--m 16] [--efc 64] [--threads 10] [--efs 10,40,200]
        [--port 21011] [--cache 3GB]

The server is started on a fresh database in a temporary directory and
stopped at the end; the clients are the Python driver, each in its own
process.  µs/search is one client asking `SELECT id FROM t ORDER BY v <->
$1 LIMIT 10` for the queries one after another (the query vector a
parameter, sent in binary); QPS is `--threads` clients at once; recall@10
as pgvector_ref.py counts it.  Writers insert one row a transaction
(autocommit), each its share of the rows.  Needs numpy (bench/ann/data.py's
virtual environment).
"""

import argparse
import multiprocessing as mp
import os
import shutil
import subprocess
import sys
import tempfile
import time

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "..", "drivers", "python"))
sys.path.insert(0, HERE)

import nucleus  # noqa: E402
from pgvector_ref import exact  # noqa: E402

SEARCH = "SELECT id FROM t ORDER BY v <-> $1 LIMIT {k}"


def connect(port, ef=None):
    for _ in range(600):
        try:
            c = nucleus.connect("127.0.0.1", port, password=None)
            break
        except (OSError, nucleus.OperationalError):
            time.sleep(0.1)
    else:
        raise RuntimeError("the server did not start")
    if ef is not None:
        c.execute(f"SET hnsw.ef_search = {ef}")
    return c


def searches(args):
    """Worker: the queries `qs` asked one after another; their ids, and
    the seconds they took."""
    port, ef, k, qs = args
    c = connect(port, ef)
    sql = SEARCH.format(k=k)
    out = []
    t = time.perf_counter()
    for q in qs:
        out.append([r[0] for r in c.execute(sql, (q,)).rows])
    secs = time.perf_counter() - t
    c.close()
    return out, secs


def searcher(args):
    """Worker: searches at `ef` until `stop` is set; how many, and their
    seconds."""
    port, ef, k, qs, stop = args
    c = connect(port, ef)
    sql = SEARCH.format(k=k)
    n, secs, i = 0, 0.0, 0
    while not stop.is_set():
        t = time.perf_counter()
        c.execute(sql, (qs[i % len(qs)],))
        secs += time.perf_counter() - t
        n += 1
        i += 1
    c.close()
    return n, secs


def writer(args):
    """Worker: rows `ids`, `vecs` inserted one a transaction."""
    port, ids, vecs = args
    c = connect(port)
    for i, v in zip(ids, vecs):
        c.execute("INSERT INTO t VALUES ($1, $2)", (int(i), v))
    c.close()
    return len(ids)


def curve(pool, port, test, truth, k, threads, efs):
    print("ef_search  recall@10  µs/search  QPS(%d)" % threads)
    for ef in efs:
        found, secs = searches((port, ef, k, list(test)))
        rec = sum(len(set(f) & set(t[:k].tolist())) / k for f, t in zip(found, truth)) / len(test)
        lat = secs / len(test) * 1e6
        parts = [(port, ef, k, list(test[j::threads])) for j in range(threads)]
        t = time.perf_counter()
        pool.map(searches, parts)
        qps = len(test) / (time.perf_counter() - t)
        print(f"{ef:>9}  {rec:>9.4f}  {lat:>9.1f}  {qps:>8.0f}", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("binary")
    ap.add_argument("dir")
    ap.add_argument("--rows", type=int, default=None)
    ap.add_argument("--bulk", type=int, default=None)
    ap.add_argument("--queries", type=int, default=2000)
    ap.add_argument("--m", type=int, default=16)
    ap.add_argument("--efc", type=int, default=64)
    ap.add_argument("--threads", type=int, default=10)
    ap.add_argument("--k", type=int, default=10)
    ap.add_argument("--efs", default="10,40,200")
    ap.add_argument("--port", type=int, default=21011)
    ap.add_argument("--cache", default="3GB", help="the server's page cache (pgvector_ref.py: shared_buffers 3GB)")
    a = ap.parse_args()
    meta = dict(kv.split("=") for kv in open(os.path.join(a.dir, "meta.txt")).read().split())
    dim = int(meta["dim"])
    assert meta["metric"] == "l2", "l2 only"
    train = np.fromfile(os.path.join(a.dir, "train.f32"), dtype="<f4").reshape(-1, dim)
    test = np.fromfile(os.path.join(a.dir, "test.f32"), dtype="<f4").reshape(-1, dim)[: a.queries]
    rows = min(a.rows or len(train), len(train))
    bulk = min(a.bulk or rows * 9 // 10, rows)
    efs = [int(x) for x in a.efs.split(",")]
    k = a.k

    work = tempfile.mkdtemp(prefix="nucleus-ann-sql-")
    args = [a.binary, "server", os.path.join(work, "db"), "--listen", f"127.0.0.1:{a.port}", "--no-auth"]
    if a.cache:
        args += ["--cache", a.cache]
    server = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    pool = mp.get_context("spawn").Pool(a.threads + 1)
    try:
        c = connect(a.port)
        print(f"nucleus ({os.path.basename(a.binary)}, hnsw m {a.m}, ef_construction {a.efc}, cache {a.cache}): "
              f"{rows} rows ({bulk} bulk, {rows - bulk} inserted by {a.threads} writers), dim {dim}", flush=True)
        c.execute(f"CREATE TABLE t (id INT PRIMARY KEY, v vector({dim}))")
        t = time.perf_counter()
        c.load("t", {"id": np.arange(bulk, dtype=np.int32), "v": list(train[:bulk])})
        load = time.perf_counter() - t
        t = time.perf_counter()
        c.execute(f"CREATE INDEX t_v ON t USING hnsw (v vector_l2_ops) WITH (m = {a.m}, ef_construction = {a.efc})")
        build = time.perf_counter() - t
        print(f"bulk: {bulk} rows loaded in {load:.1f} s, indexed in {build:.1f} s "
              f"({build * 1e6 / bulk:.1f} µs/row)", flush=True)
        plan = "\n".join(r[0] for r in c.execute("EXPLAIN " + SEARCH.format(k=k), (test[0],)).rows)
        assert "nearest" in plan, plan
        truth = exact(train[:bulk], test, k)
        curve(pool, a.port, test, truth, k, a.threads, efs)

        # Writers and a searcher at once.
        stop = mp.get_context("spawn").Manager().Event()
        busy = pool.apply_async(searcher, ((a.port, 40, k, list(test), stop),))
        time.sleep(1)
        ids = np.arange(bulk, rows)
        parts = [(a.port, ids[j::a.threads], list(train[bulk:rows][j::a.threads])) for j in range(a.threads)]
        t = time.perf_counter()
        inserted = sum(pool.map(writer, parts))
        secs = time.perf_counter() - t
        stop.set()
        n, ssecs = busy.get()
        print(f"inserts: {inserted} rows in {secs:.1f} s ({inserted / secs:.0f} rows/s); "
              f"searches meanwhile (ef 40): {n}, {ssecs / max(n, 1) * 1e6:.0f} µs each", flush=True)
        truth = exact(train[:rows], test, k)
        print("after the inserts:")
        curve(pool, a.port, test, truth, k, a.threads, efs)
        c.close()
    finally:
        pool.terminate()
        server.terminate()
        server.wait()
        shutil.rmtree(work)


if __name__ == "__main__":
    main()
