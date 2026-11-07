#!/usr/bin/env python3
"""nucleus's HNSW index through SQL (PLAN.md §M14.12 V4b) on a dataset in
bench/ann/data.py's format, run as bench/ann/pgvector_ref.py runs
pgvector: the first rows loaded and indexed, a recall curve, rows
inserted by concurrent writers while a searcher asks, the curve again.

    bench/ann/sql.py BINARY DIR [--rows N] [--bulk N] [--queries 2000]
        [--m 16] [--efc 64] [--threads 10] [--efs 10,40,200]
        [--port 21011] [--cache 3GB] [--quantize half|int8|binary]
        [--rescore N] [--no-inserts]

The server is started on a fresh database in a temporary directory and
stopped at the end; the clients are the Python driver, each in its own
process.  µs/search is one client asking `SELECT id FROM t ORDER BY v <->
$1 LIMIT 10` for the queries one after another (the query vector a
parameter, sent in binary); QPS is `--threads` clients at once; recall@10
as pgvector_ref.py counts it.  A cosine dataset (stored normalized) is
indexed with vector_cosine_ops and searched by `<=>`.  `--quantize` and
`--rescore` are the index's options (PLAN.md §M14.12 V6a); what the build
added to the server's resident memory and to its files is printed.  Writers insert one row a transaction
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


def rss(pid):
    """A process's resident bytes."""
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True).stdout
    return int(out.strip() or 0) * 1024


def du(path):
    """The bytes of the files under `path`."""
    return sum(os.path.getsize(os.path.join(d, f)) for d, _, fs in os.walk(path) for f in fs)


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
    port, ef, sql, qs = args
    c = connect(port, ef)
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
    port, ef, sql, qs, stop = args
    c = connect(port, ef)
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


def curve(pool, port, test, truth, k, threads, efs, sql):
    print("ef_search  recall@10  µs/search  QPS(%d)" % threads)
    for ef in efs:
        found, secs = searches((port, ef, sql, list(test)))
        rec = sum(len(set(f) & set(t[:k].tolist())) / k for f, t in zip(found, truth)) / len(test)
        lat = secs / len(test) * 1e6
        parts = [(port, ef, sql, list(test[j::threads])) for j in range(threads)]
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
    ap.add_argument("--quantize", default=None, help="the index's codes: half, int8 or binary (§M14.12 V6)")
    ap.add_argument("--rescore", type=int, default=None, help="rows read for each returned (a quantized index)")
    ap.add_argument("--no-inserts", action="store_true", help="the bulk rows only: no writers, one curve")
    a = ap.parse_args()
    meta = dict(kv.split("=") for kv in open(os.path.join(a.dir, "meta.txt")).read().split())
    dim = int(meta["dim"])
    # Angular datasets are stored normalized: the Euclidean order is the
    # cosine one, so the ground truth is found the same way.
    cosine = meta["metric"] == "cosine"
    ops, op = ("vector_cosine_ops", "<=>") if cosine else ("vector_l2_ops", "<->")
    sql = SEARCH.replace("<->", op).format(k=a.k)
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
        with_ = f"m = {a.m}, ef_construction = {a.efc}"
        if a.quantize:
            with_ += f", quantize = '{a.quantize}'"
        if a.rescore:
            with_ += f", rescore = {a.rescore}"
        before = (rss(server.pid), du(os.path.join(work, "db")))
        t = time.perf_counter()
        c.execute(f"CREATE INDEX t_v ON t USING hnsw (v {ops}) WITH ({with_})")
        build = time.perf_counter() - t
        after = (rss(server.pid), du(os.path.join(work, "db")))
        print(f"bulk: {bulk} rows loaded in {load:.1f} s, indexed in {build:.1f} s "
              f"({build * 1e6 / bulk:.1f} µs/row; WITH ({with_})); the index: "
              f"+{(after[0] - before[0]) / 2**20:.0f} MB resident, +{(after[1] - before[1]) / 2**20:.0f} MB on disk",
              flush=True)
        plan = "\n".join(r[0] for r in c.execute("EXPLAIN " + sql, (test[0],)).rows)
        assert "nearest" in plan, plan
        truth = exact(train[:bulk], test, k)
        curve(pool, a.port, test, truth, k, a.threads, efs, sql)
        if a.no_inserts:
            c.close()
            return

        # Writers and a searcher at once.
        stop = mp.get_context("spawn").Manager().Event()
        busy = pool.apply_async(searcher, ((a.port, 40, sql, list(test), stop),))
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
        curve(pool, a.port, test, truth, k, a.threads, efs, sql)
        c.close()
    finally:
        pool.terminate()
        server.terminate()
        server.wait()
        shutil.rmtree(work)


if __name__ == "__main__":
    main()
