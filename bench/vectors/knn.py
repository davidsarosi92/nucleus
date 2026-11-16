"""Exact nearest neighbours (PLAN.md §M14.12): `ORDER BY v <-> $1 LIMIT k`
over a table of random float32 vectors, against numpy's brute force on the
same data — the answers compared, the time and the bandwidth (the bytes of
vectors read per second) reported.  With `--pg`, the same through
pgvector, exact (no index), in PostgreSQL native on this machine
(bench/pg.py's cluster, pgvector installed in it).

    python3 -I bench/vectors/knn.py BINARY [--rows 100000] [--dims 768]
        [--queries 20] [--k 10] [--port 21010] [--cache 1GB] [--pg]

The nucleus server is started on a fresh database in a temporary
directory and stopped at the end.  Each query runs 3 times, its median
kept; a system's time is the median over the queries.  Its numbers are
printed as `@metric` lines too, for `bench/method.py run`, which repeats
the whole script (PLAN.md §M14.31)."""

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
# NUCLEUS_BENCH_HOST=::1 where IPv4 loopback's ports run out (bench/common.py).
HOST = os.environ.get("NUCLEUS_BENCH_HOST", "127.0.0.1")
LISTEN = f"[{HOST}]" if ":" in HOST else HOST
sys.path.insert(0, os.path.join(HERE, "..", "..", "drivers", "python"))
sys.path.insert(0, os.path.join(HERE, ".."))

import numpy as np  # noqa: E402

import method  # noqa: E402
import nucleus  # noqa: E402


def timed(f, times=3):
    """The median of `times` runs of f(), in ms, and its last result."""
    out, t = None, []
    for _ in range(times):
        s = time.perf_counter()
        out = f()
        t.append((time.perf_counter() - s) * 1000)
    return method.median(t), out


def numpy_knn(x, q, k):
    d = ((x - q) ** 2).sum(axis=1, dtype=np.float64)
    idx = np.argpartition(d, k)[:k]
    return idx[np.lexsort((idx, d[idx]))]


def start_nucleus(binary, port, cache):
    dir = tempfile.mkdtemp(prefix="nucleus-knn-")
    args = [binary, "server", os.path.join(dir, "db"), "--listen", f"{LISTEN}:{port}", "--no-auth"]
    if cache:
        args += ["--cache", cache]
    s = subprocess.Popen(args,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(600):
        try:
            return s, dir, nucleus.connect(HOST, port, password=None)
        except (OSError, nucleus.OperationalError):
            time.sleep(0.1)
    s.kill()
    raise RuntimeError("the server did not start")


def main():
    a = argparse.ArgumentParser()
    a.add_argument("binary")
    a.add_argument("--rows", type=int, default=100_000)
    a.add_argument("--dims", type=int, default=768)
    a.add_argument("--queries", type=int, default=20)
    a.add_argument("--k", type=int, default=10)
    a.add_argument("--port", type=int, default=21010)
    a.add_argument("--pg", action="store_true")
    a.add_argument("--cache", default="1GB",
                   help="nucleus's page cache (pgvector gets shared_buffers=1GB); '' for the server's default")
    o = a.parse_args()

    rng = np.random.default_rng(7)
    x = rng.standard_normal((o.rows, o.dims), dtype=np.float32)
    qs = rng.standard_normal((o.queries, o.dims), dtype=np.float32)
    mib = x.nbytes / 2**20
    print(f"{o.rows} vectors of {o.dims} dimensions ({mib:.0f} MiB), k = {o.k}, {o.queries} queries")

    # numpy: the reference, and its time.
    want = [numpy_knn(x, q, o.k) for q in qs]
    np_ms = method.median([timed(lambda q=q: numpy_knn(x, q, o.k))[0] for q in qs])
    print(f"numpy brute force          {np_ms:8.1f} ms  {mib / np_ms * 1000 / 1024:5.2f} GiB/s")
    method.emit("search", np_ms, system="numpy")
    method.emit_system("numpy", {"version": np.__version__, "settings": "one core, float64 sums"})

    s, dir, c = start_nucleus(o.binary, o.port, o.cache)
    try:
        c.execute("CREATE TABLE items (id INT PRIMARY KEY, v vector(%d))" % o.dims)
        t0 = time.perf_counter()
        c.load("items", {"id": np.arange(o.rows, dtype=np.int32), "v": list(x)})
        load_s = time.perf_counter() - t0
        c.execute("VACUUM ANALYZE items")
        sql = f"SELECT id FROM items ORDER BY v <-> $1, id LIMIT {o.k}"
        wrong, times = 0, []
        for i, q in enumerate(qs):
            ms, rows = timed(lambda q=q: c.execute(sql, (q,)).rows)
            times.append(ms)
            got = [r[0] for r in rows]
            if got != list(want[i]):
                wrong += 1
        ms = method.median(times)
        print(f"nucleus ORDER BY <-> LIMIT {ms:8.1f} ms  {mib / ms * 1000 / 1024:5.2f} GiB/s"
              f"  (median of {o.queries}; load {load_s:.1f} s; answers differing from numpy's: {wrong})")
        method.emit("search", ms)
        method.emit("load", load_s, unit="s")
        method.emit("answers differing from numpy's", wrong, unit="count")
        method.emit_system("nucleus", {"settings": {"cache": o.cache or "default"}})
    finally:
        c.close()
        s.terminate()
        s.wait()
        shutil.rmtree(dir)

    if o.pg:
        import pg
        with pg.Cluster(pg.DEFAULT) as c:
            c.sql("DROP TABLE IF EXISTS items")
            c.sql("CREATE EXTENSION IF NOT EXISTS vector")
            c.sql(f"CREATE TABLE items (id INT PRIMARY KEY, v vector({o.dims}))")
            try:
                t0 = time.perf_counter()
                step = 5000
                for start in range(0, o.rows, step):
                    lines = "".join(f"{i}\t[{','.join(map(repr, map(float, x[i])))}]\n"
                                    for i in range(start, min(o.rows, start + step)))
                    c.sql(file="COPY items FROM STDIN;\n" + lines + "\\.\n")
                load_s = time.perf_counter() - t0
                c.sql("VACUUM ANALYZE items")
                wrong, times = 0, []
                for i, q in enumerate(qs):
                    lit = "[" + ",".join(map(repr, map(float, q))) + "]"
                    stmt = f"SELECT id FROM items ORDER BY v <-> '{lit}', id LIMIT {o.k}"
                    out = c.sql(file="\\timing on\n" + (stmt + ";\n") * 3)
                    times.append(method.median([float(m.split()[1]) for m in out.splitlines()
                                                if m.startswith("Time:")]))
                    got = [int(r) for r in out.splitlines() if r and not r.startswith("Time:")][: o.k]
                    if got != list(want[i]):
                        wrong += 1
                ms = method.median(times)
                print(f"pgvector exact (no index)  {ms:8.1f} ms  {mib / ms * 1000 / 1024:5.2f} GiB/s"
                      f"  (median of {o.queries}; load {load_s:.1f} s; answers differing from numpy's: {wrong})")
                version = c.sql("SELECT extversion FROM pg_extension WHERE extname = 'vector'").strip()
                method.emit("search", ms, system="pgvector")
                method.emit("load", load_s, unit="s", system="pgvector")
                method.emit("answers differing from numpy's", wrong, unit="count", system="pgvector")
                method.emit_system("pgvector", dict(c.describe(), pgvector=version))
            finally:
                c.sql("DROP TABLE IF EXISTS items")


if __name__ == "__main__":
    main()
