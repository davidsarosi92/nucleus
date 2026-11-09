#!/usr/bin/env python3
"""Sparse vectors searched exactly (PLAN.md §M14.12 V6b): SPLADE-like
term weights — a vocabulary of 30,522 (BERT's), a document ~120 terms
and a query ~20 drawn by a Zipf law, positive log-normal weights — as
`sparsevec(30522)` rows, `SELECT id FROM t ORDER BY v <#> q LIMIT 10` (the
largest inner products), through nucleus's SQL or pgvector's (image
pgvector/pgvector:pg18, in Docker).  Synthetic and seeded: the same rows
every run.

    bench/ann/sparse.py nucleus BINARY [--rows 100000] [--queries 200]
        [--port 21014] [--cache 3GB]
    bench/ann/sparse.py pgvector [--rows 100000] [--queries 200]

Prints the load, recall@10 against the exact answer (numpy, through
posting lists) — an exact search's is 1 — and µs a search on one client.
Needs numpy (bench/ann/data.py's virtual environment).
"""

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
# NUCLEUS_BENCH_HOST=::1 where IPv4 loopback's ports run out (bench/common.py).
HOST = os.environ.get("NUCLEUS_BENCH_HOST", "127.0.0.1")
LISTEN = f"[{HOST}]" if ":" in HOST else HOST
sys.path.insert(0, os.path.join(HERE, "..", "..", "drivers", "python"))
sys.path.insert(0, HERE)

from pgvector_ref import NAME, IMAGE, psql, sh  # noqa: E402

VOCAB = 30522
K = 10


def draw(rng, n, terms):
    """`n` sparse vectors of about `terms` non-zero components each: (positions, weights) pairs."""
    p = 1.0 / (np.arange(VOCAB) + 10.0)
    p /= p.sum()
    out = []
    for c in rng.poisson(terms, n):
        at = np.unique(rng.choice(VOCAB, size=max(int(c), 1), p=p))
        out.append((at, rng.lognormal(0.0, 0.6, len(at)).astype(np.float32)))
    return out


def text(v):
    at, w = v
    return "{" + ",".join(f"{a + 1}:{x:.6g}" for a, x in zip(at, w)) + "}/" + str(VOCAB)


def exact(rows, queries):
    """Each query's ten largest inner products, through posting lists."""
    postings = {}
    for i, (at, w) in enumerate(rows):
        for a, x in zip(at, w):
            postings.setdefault(int(a), ([], []))
            postings[int(a)][0].append(i)
            postings[int(a)][1].append(float(np.float32(x)))
    postings = {a: (np.array(d), np.array(x)) for a, (d, x) in postings.items()}
    truth = []
    for at, w in queries:
        s = np.zeros(len(rows))
        for a, x in zip(at, w):
            if int(a) in postings:
                d, y = postings[int(a)]
                s[d] += float(np.float32(x)) * y
        top = np.argsort(-s, kind="stable")[:K]
        truth.append(set(top.tolist()))
    return truth


def recall(found, truth):
    return sum(len(set(f) & t) / K for f, t in zip(found, truth)) / len(truth)


def nucleus_run(a, rows, queries, truth):
    import nucleus

    work = tempfile.mkdtemp(prefix="nucleus-sparse-")
    server = subprocess.Popen([a.binary, "server", os.path.join(work, "db"), "--listen", f"{LISTEN}:{a.port}",
                               "--no-auth", "--cache", a.cache], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(600):
            try:
                c = nucleus.connect(HOST, a.port, password=None)
                break
            except (OSError, nucleus.OperationalError):
                time.sleep(0.1)
        c.execute(f"CREATE TABLE t (id INT PRIMARY KEY, v sparsevec({VOCAB}))")
        t = time.perf_counter()
        for b in range(0, len(rows), 1000):
            values = ", ".join(f"({i}, '{text(rows[i])}')" for i in range(b, min(b + 1000, len(rows))))
            c.execute(f"INSERT INTO t VALUES {values}")
        load = time.perf_counter() - t
        print(f"nucleus ({os.path.basename(a.binary)}): {len(rows)} rows loaded in {load:.1f} s", flush=True)
        sql = f"SELECT id FROM t ORDER BY v <#> $1::sparsevec LIMIT {K}"
        qs = [text(q) for q in queries]
        for q in qs[:20]:
            c.execute(sql, (q,))
        found = []
        t = time.perf_counter()
        for q in qs:
            found.append([r[0] for r in c.execute(sql, (q,)).rows])
        lat = (time.perf_counter() - t) / len(qs) * 1e6
        plan = " / ".join(r[0].strip() for r in c.execute("EXPLAIN " + sql, (qs[0],)).rows)
        print(f"recall@10 {recall(found, truth):.4f}  {lat:.0f} µs/search  {plan}", flush=True)
        c.close()
    finally:
        server.terminate()
        server.wait()
        shutil.rmtree(work)


def pgvector_run(a, rows, queries, truth):
    work = tempfile.mkdtemp(prefix="pgvector-sparse-")
    with open(os.path.join(work, "rows.csv"), "w") as f:
        for i, r in enumerate(rows):
            f.write(f'{i},"{text(r)}"\n')
    with open(os.path.join(work, "queries.csv"), "w") as f:
        for i, q in enumerate(queries):
            f.write(f'{i},"{text(q)}"\n')
    with open(os.path.join(work, "search.sql"), "w") as f:
        f.write(f"\\set qid random(0, {len(queries) - 1})\n"
                f"SELECT id FROM t ORDER BY v <#> (SELECT v FROM q WHERE id = :qid) LIMIT {K};\n")
    os.chmod(work, 0o755)
    subprocess.run(["docker", "rm", "-f", "-v", NAME], capture_output=True)
    sh("docker", "run", "-d", "--name", NAME, "-e", "POSTGRES_HOST_AUTH_METHOD=trust", "--shm-size=6g",
       "-v", f"{work}:/data", IMAGE, "-c", f"shared_buffers={a.cache}", "-c", "jit=off")
    try:
        for _ in range(300):
            if subprocess.run(["docker", "exec", NAME, "pg_isready", "-U", "postgres"],
                              capture_output=True).returncode == 0:
                break
            time.sleep(0.2)
        time.sleep(1)
        t = time.perf_counter()
        psql("CREATE EXTENSION vector;\n"
             f"CREATE TABLE t (id int PRIMARY KEY, v sparsevec({VOCAB}));\n"
             f"CREATE TABLE q (id int PRIMARY KEY, v sparsevec({VOCAB}));\n"
             "COPY t FROM '/data/rows.csv' (FORMAT csv);\n"
             "COPY q FROM '/data/queries.csv' (FORMAT csv);\n"
             "VACUUM ANALYZE t; VACUUM ANALYZE q;\n")
        print(f"pgvector ({IMAGE}): {len(rows)} rows loaded in {time.perf_counter() - t:.1f} s", flush=True)
        out = psql(f"SELECT q.id, array_to_string(array(SELECT t.id FROM t ORDER BY t.v <#> q.v LIMIT {K}), ',') "
                   "FROM q ORDER BY q.id;\n").split()
        found = [[int(x) for x in line.split("|")[1].split(",") if x] for line in out]
        plan = psql("EXPLAIN (COSTS OFF) SELECT id FROM t ORDER BY v <#> (SELECT v FROM q WHERE id = 0) "
                    f"LIMIT {K};\n")
        plan = " / ".join(l.strip() for l in plan.splitlines() if l.strip())
        res = subprocess.run(["docker", "exec", NAME, "pgbench", "-n", "-U", "postgres", "-f", "/data/search.sql",
                              "-c", "1", "-j", "1", "-t", str(len(queries)), "postgres"],
                             check=True, capture_output=True, text=True).stdout
        lat = float(re.search(r"latency average = ([\d.]+) ms", res).group(1)) * 1000
        print(f"recall@10 {recall(found, truth):.4f}  {lat:.0f} µs/search  {plan}", flush=True)
    finally:
        subprocess.run(["docker", "rm", "-f", "-v", NAME], capture_output=True)
        shutil.rmtree(work)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("system", choices=["nucleus", "pgvector"])
    ap.add_argument("binary", nargs="?")
    ap.add_argument("--rows", type=int, default=100_000)
    ap.add_argument("--queries", type=int, default=200)
    ap.add_argument("--port", type=int, default=21014)
    ap.add_argument("--cache", default="3GB")
    a = ap.parse_args()
    rng = np.random.default_rng(7)
    rows = draw(rng, a.rows, 120)
    queries = draw(rng, a.queries, 20)
    truth = exact(rows, queries)
    if a.system == "nucleus":
        nucleus_run(a, rows, queries, truth)
    else:
        pgvector_run(a, rows, queries, truth)


if __name__ == "__main__":
    main()
