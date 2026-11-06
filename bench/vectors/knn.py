"""Exact nearest neighbours (PLAN.md §M14.12): `ORDER BY v <-> $1 LIMIT k`
over a table of random float32 vectors, against numpy's brute force on the
same data — the answers compared, the time and the bandwidth (the bytes of
vectors read per second) reported.  With `--pg`, the same through
pgvector in Docker (image pgvector/pgvector:pg18), exact (no index).

    python3 -I bench/vectors/knn.py BINARY [--rows 100000] [--dims 768]
        [--queries 20] [--k 10] [--port 21010] [--pg]

The nucleus server is started on a fresh database in a temporary
directory and stopped at the end."""

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "..", "drivers", "python"))

import numpy as np  # noqa: E402

import nucleus  # noqa: E402


def best(f, times=5):
    """The best of `times` runs of f(), in ms, and its last result."""
    out, t = None, []
    for _ in range(times):
        s = time.perf_counter()
        out = f()
        t.append((time.perf_counter() - s) * 1000)
    return min(t), out


def numpy_knn(x, q, k):
    d = ((x - q) ** 2).sum(axis=1, dtype=np.float64)
    idx = np.argpartition(d, k)[:k]
    return idx[np.lexsort((idx, d[idx]))]


def start_nucleus(binary, port):
    dir = tempfile.mkdtemp(prefix="nucleus-knn-")
    s = subprocess.Popen([binary, "server", os.path.join(dir, "db"), "--listen", f"127.0.0.1:{port}", "--no-auth"],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(600):
        try:
            return s, dir, nucleus.connect("127.0.0.1", port, password=None)
        except (OSError, nucleus.OperationalError):
            time.sleep(0.1)
    s.kill()
    raise RuntimeError("the server did not start")


def pg(sql, file=None):
    cmd = ["docker", "exec", "-i", "nucleus-knn-pg", "psql", "-q", "-At", "-U", "postgres", "-v", "ON_ERROR_STOP=1"]
    if file is None:
        cmd += ["-c", sql]
    return subprocess.run(cmd, check=True, capture_output=True, text=True, input=file).stdout


def main():
    a = argparse.ArgumentParser()
    a.add_argument("binary")
    a.add_argument("--rows", type=int, default=100_000)
    a.add_argument("--dims", type=int, default=768)
    a.add_argument("--queries", type=int, default=20)
    a.add_argument("--k", type=int, default=10)
    a.add_argument("--port", type=int, default=21010)
    a.add_argument("--pg", action="store_true")
    o = a.parse_args()

    rng = np.random.default_rng(7)
    x = rng.standard_normal((o.rows, o.dims), dtype=np.float32)
    qs = rng.standard_normal((o.queries, o.dims), dtype=np.float32)
    mib = x.nbytes / 2**20
    print(f"{o.rows} vectors of {o.dims} dimensions ({mib:.0f} MiB), k = {o.k}, {o.queries} queries")

    # numpy: the reference, and its time.
    want = [numpy_knn(x, q, o.k) for q in qs]
    np_ms = min(best(lambda q=q: numpy_knn(x, q, o.k), 3)[0] for q in qs[:5])
    print(f"numpy brute force          {np_ms:8.1f} ms  {mib / np_ms * 1000 / 1024:5.2f} GiB/s")

    s, dir, c = start_nucleus(o.binary, o.port)
    try:
        c.execute("CREATE TABLE items (id INT PRIMARY KEY, v vector(%d))" % o.dims)
        t0 = time.perf_counter()
        c.load("items", {"id": np.arange(o.rows, dtype=np.int32), "v": list(x)})
        load_s = time.perf_counter() - t0
        c.execute("VACUUM ANALYZE items")
        sql = f"SELECT id FROM items ORDER BY v <-> $1, id LIMIT {o.k}"
        wrong, times = 0, []
        for i, q in enumerate(qs):
            ms, rows = best(lambda q=q: c.execute(sql, (q,)).rows, 3)
            times.append(ms)
            got = [r[0] for r in rows]
            if got != list(want[i]):
                wrong += 1
        ms = sorted(times)[len(times) // 2]
        print(f"nucleus ORDER BY <-> LIMIT {ms:8.1f} ms  {mib / ms * 1000 / 1024:5.2f} GiB/s"
              f"  (median of {o.queries}; load {load_s:.1f} s; answers differing from numpy's: {wrong})")
    finally:
        c.close()
        s.terminate()
        s.wait()
        shutil.rmtree(dir)

    if o.pg:
        subprocess.run(["docker", "rm", "-f", "nucleus-knn-pg"], capture_output=True)
        subprocess.run(["docker", "run", "-d", "--name", "nucleus-knn-pg", "-e", "POSTGRES_HOST_AUTH_METHOD=trust",
                        "--shm-size=1g", "pgvector/pgvector:pg18", "-c", "shared_buffers=1GB",
                        "-c", "max_parallel_workers_per_gather=4"], check=True, capture_output=True)
        try:
            for _ in range(600):
                if subprocess.run(["docker", "exec", "nucleus-knn-pg", "pg_isready", "-U", "postgres"],
                                  capture_output=True).returncode == 0:
                    break
                time.sleep(0.1)
            time.sleep(1)
            pg("CREATE EXTENSION vector")
            pg(f"CREATE TABLE items (id INT PRIMARY KEY, v vector({o.dims}))")
            t0 = time.perf_counter()
            step = 5000
            for start in range(0, o.rows, step):
                lines = "".join(f"{i}\t[{','.join(map(repr, map(float, x[i])))}]\n"
                                for i in range(start, min(o.rows, start + step)))
                pg(None, file="COPY items FROM STDIN;\n" + lines + "\\.\n")
            load_s = time.perf_counter() - t0
            pg("VACUUM ANALYZE items")
            wrong, times = 0, []
            for i, q in enumerate(qs):
                lit = "[" + ",".join(map(repr, map(float, q))) + "]"
                stmt = f"SELECT id FROM items ORDER BY v <-> '{lit}', id LIMIT {o.k}"
                script = "\\timing on\n" + (stmt + ";\n") * 3
                out = pg(None, file=script)
                ms = min(float(m.split()[1]) for m in out.splitlines() if m.startswith("Time:"))
                times.append(ms)
                got = [int(r) for r in out.splitlines() if r and not r.startswith("Time:")][: o.k]
                if got != list(want[i]):
                    wrong += 1
            ms = sorted(times)[len(times) // 2]
            print(f"pgvector exact (no index)  {ms:8.1f} ms  {mib / ms * 1000 / 1024:5.2f} GiB/s"
                  f"  (median of {o.queries}; load {load_s:.1f} s; answers differing from numpy's: {wrong})")
        finally:
            subprocess.run(["docker", "rm", "-f", "nucleus-knn-pg"], capture_output=True)


if __name__ == "__main__":
    main()
