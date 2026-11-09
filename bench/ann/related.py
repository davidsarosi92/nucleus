#!/usr/bin/env python3
"""Searches whose query vector is a row's (PLAN.md §M14.12 V7): "more like
this row" — `ORDER BY v <=> (SELECT v FROM t WHERE id = $1) LIMIT 10`, the
client sending an id — and each of many rows' nearest at once through
`LATERAL`, in nucleus and in pgvector (image pgvector/pgvector:pg18, in
Docker), on a dataset in bench/ann/data.py's format.

    bench/ann/related.py nucleus BINARY DIR [--bulk N] [--ids 200]
        [--lateral 1000] [--ef 40] [--port 21015] [--host 127.0.0.1]
        [--cache 3GB] [--quantize int8]
    bench/ann/related.py pgvector DIR [--bulk N] [--ids 200]
        [--lateral 1000] [--ef 40]

The first `--bulk` rows are loaded and indexed (m 16, ef_construction
64).  "more like": `--ids` rows' ten nearest others, one after another
from one client — µs a search at the client (the id sent, so the two
clients do the same work) and the server's own (`EXPLAIN ANALYZE`) —,
recall@10 against the exact answer.  "lateral": the first `--lateral`
rows' ten nearest others in one statement, its time and recall; nucleus
with its workers (`parallel_workers` as set) and on one thread.  Needs
numpy (bench/ann/data.py's virtual environment).
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
sys.path.insert(0, os.path.join(HERE, "..", "..", "drivers", "python"))
sys.path.insert(0, HERE)

from pgvector_ref import IMAGE, NAME, copy_binary, psql, sh  # noqa: E402

K = 10


def like_sql(op, q):
    return f"SELECT id FROM t WHERE id <> {q} ORDER BY v {op} (SELECT v FROM t WHERE id = {q}) LIMIT {K}"


def lateral_sql(op, n):
    return (f"SELECT a.id, n.id FROM t a, LATERAL (SELECT b.id FROM t b WHERE b.id <> a.id "
            f"ORDER BY b.v {op} a.v LIMIT {K}) n WHERE a.id < {n}")


def truth(train, ids, cosine):
    """Each row of `ids`' ten nearest other rows of `train`, exactly."""
    out = []
    for b in range(0, len(ids), 100):
        q = train[ids[b:b + 100]]
        if cosine:
            d = -(q @ train.T)
        else:
            d = (q * q).sum(1)[:, None] - 2 * q @ train.T + (train * train).sum(1)[None, :]
        for j, i in enumerate(ids[b:b + 100]):
            d[j, i] = np.inf
        out.extend(np.argsort(d, axis=1, kind="stable")[:, :K].tolist())
    return [set(t) for t in out]


def recall(found, exact):
    return sum(len(set(f) & t) / K for f, t in zip(found, exact)) / len(exact)


def by_row(pairs, n):
    found = [[] for _ in range(n)]
    for a, b in pairs:
        found[a].append(b)
    return found


def nucleus_run(a, train, cosine, dim, op, ops):
    import nucleus

    work = tempfile.mkdtemp(prefix="nucleus-related-")
    listen = f"[{a.host}]:{a.port}" if ":" in a.host else f"{a.host}:{a.port}"
    server = subprocess.Popen([a.binary, "server", os.path.join(work, "db"), "--listen", listen,
                               "--no-auth", "--cache", a.cache], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(600):
            try:
                c = nucleus.connect(a.host, a.port, password=None)
                break
            except (OSError, nucleus.OperationalError):
                time.sleep(0.1)
        c.execute(f"CREATE TABLE t (id INT PRIMARY KEY, v vector({dim}))")
        c.load("t", {"id": np.arange(a.bulk, dtype=np.int32), "v": list(train)})
        with_ = "m = 16, ef_construction = 64" + (f", quantize = '{a.quantize}'" if a.quantize else "")
        t = time.perf_counter()
        c.execute(f"CREATE INDEX t_v ON t USING hnsw (v {ops}) WITH ({with_})")
        print(f"nucleus ({os.path.basename(a.binary)}): {a.bulk} rows, dim {dim}, indexed in "
              f"{time.perf_counter() - t:.1f} s (WITH ({with_}))", flush=True)
        c.execute("ANALYZE")
        c.execute(f"SET hnsw.ef_search = {a.ef}")
        ids = list(range(0, a.bulk, a.bulk // a.ids))[: a.ids]
        exact = truth(train, ids, cosine)
        sql = like_sql(op, "$1")
        plan = " / ".join(r[0].strip() for r in c.execute("EXPLAIN " + sql, (ids[0],)).rows)
        for i in ids[:20]:
            c.execute(sql, (i,))
        t = time.perf_counter()
        found = [[r[0] for r in c.execute(sql, (i,)).rows] for i in ids]
        lat = (time.perf_counter() - t) / len(ids) * 1e6
        server_us = []
        for i in ids:
            out = "\n".join(r[0] for r in c.execute("EXPLAIN ANALYZE " + sql, (i,)).rows)
            server_us.append(float(re.search(r"Execution: \d+ rows in ([\d.]+) ms", out).group(1)) * 1000)
        print(f"more like (ef {a.ef}): recall@10 {recall(found, exact):.4f}, {lat:.0f} µs a search, "
              f"server {sum(server_us) / len(server_us):.0f} µs — {plan}", flush=True)

        sql = lateral_sql(op, a.lateral)
        exact = truth(train, list(range(a.lateral)), cosine)
        try:
            plan = " / ".join(r[0].strip() for r in c.execute("EXPLAIN " + sql).rows)
        except nucleus.Error as e:
            print(f"lateral: {e}", flush=True)
            return
        for workers in ["default", "1"]:
            c.execute("RESET parallel_workers" if workers == "default" else f"SET parallel_workers = {workers}")
            c.execute(sql)
            t = time.perf_counter()
            pairs = c.execute(sql).rows
            secs = time.perf_counter() - t
            found = by_row(pairs, a.lateral)
            print(f"lateral, {a.lateral} rows' nearest (parallel_workers {workers}): {secs * 1000:.0f} ms, "
                  f"{secs / a.lateral * 1e6:.0f} µs a row, recall@10 {recall(found, exact):.4f}", flush=True)
        print(f"  plan: {plan}", flush=True)
        c.close()
    finally:
        server.terminate()
        server.wait()
        shutil.rmtree(work)


def pgvector_run(a, train, cosine, dim, op, ops):
    work = tempfile.mkdtemp(prefix="pgvector-related-")
    copy_binary(os.path.join(work, "bulk.copy"), np.arange(a.bulk), train)
    ids = list(range(0, a.bulk, a.bulk // a.ids))[: a.ids]
    with open(os.path.join(work, "ids.txt"), "w") as f:
        f.write("\n".join(str(i) for i in ids) + "\n")
    with open(os.path.join(work, "like.sql"), "w") as f:
        f.write(f"\\set i random(0, {len(ids) - 1})\n\\set q :i * {a.bulk // a.ids}\n" + like_sql(op, ":q") + ";\n")
    os.chmod(work, 0o755)
    subprocess.run(["docker", "rm", "-f", "-v", NAME], capture_output=True)
    sh("docker", "run", "-d", "--name", NAME, "-e", "POSTGRES_HOST_AUTH_METHOD=trust", "--shm-size=6g",
       "-v", f"{work}:/data", IMAGE, "-c", f"shared_buffers={a.cache}", "-c", "maintenance_work_mem=4GB",
       "-c", "max_parallel_maintenance_workers=9", "-c", "jit=off")
    try:
        for _ in range(300):
            if subprocess.run(["docker", "exec", NAME, "pg_isready", "-U", "postgres"],
                              capture_output=True).returncode == 0:
                break
            time.sleep(0.2)
        time.sleep(1)
        psql("CREATE EXTENSION vector;\n"
             f"CREATE TABLE t (id int PRIMARY KEY, v vector({dim}));\n"
             "COPY t FROM '/data/bulk.copy' (FORMAT binary);\n")
        t = time.perf_counter()
        psql(f"CREATE INDEX t_v_idx ON t USING hnsw (v {ops}) WITH (m = 16, ef_construction = 64);\n"
             "VACUUM ANALYZE t;\n")
        print(f"pgvector ({IMAGE}): {a.bulk} rows, dim {dim}, indexed in {time.perf_counter() - t:.1f} s",
              flush=True)
        ef = f"SET hnsw.ef_search = {a.ef};\n"
        exact = truth(train, ids, cosine)
        out = psql(ef + "".join(f"SELECT {i}, array_to_string(array({like_sql(op, i)}), ',');\n" for i in ids))
        found = [[int(x) for x in line.split("|")[1].split(",") if x] for line in out.split()]
        plan = psql("EXPLAIN (COSTS OFF) " + like_sql(op, ids[0]) + ";\n")
        plan = " / ".join(x.strip() for x in plan.splitlines() if x.strip())
        res = subprocess.run(["docker", "exec", "-e", f"PGOPTIONS=-c hnsw.ef_search={a.ef}", NAME, "pgbench", "-n",
                              "-U", "postgres", "-f", "/data/like.sql", "-c", "1", "-j", "1", "-t", "1000",
                              "postgres"], check=True, capture_output=True, text=True).stdout
        lat = float(re.search(r"latency average = ([\d.]+) ms", res).group(1)) * 1000
        times = [float(m.group(1)) for m in re.finditer(r"Execution Time: ([\d.]+) ms", psql(ef + "".join(
            f"EXPLAIN (ANALYZE, TIMING OFF, COSTS OFF) {like_sql(op, i)};\n" for i in ids)))]
        print(f"more like (ef {a.ef}): recall@10 {recall(found, exact):.4f}, {lat:.0f} µs a search, "
              f"server {sum(times) / len(times) * 1000:.0f} µs — {plan}", flush=True)

        sql = lateral_sql(op, a.lateral)
        exact = truth(train, list(range(a.lateral)), cosine)
        psql(ef + sql + ";\n")
        t = time.perf_counter()
        out = psql(ef + sql + ";\n")
        secs = time.perf_counter() - t
        pairs = [tuple(int(x) for x in line.split("|")) for line in out.split()]
        found = by_row(pairs, a.lateral)
        plan = psql("EXPLAIN (COSTS OFF) " + sql + ";\n")
        plan = " / ".join(x.strip() for x in plan.splitlines() if x.strip())
        print(f"lateral, {a.lateral} rows' nearest: {secs * 1000:.0f} ms, {secs / a.lateral * 1e6:.0f} µs a row, "
              f"recall@10 {recall(found, exact):.4f}", flush=True)
        print(f"  plan: {plan}", flush=True)
    finally:
        subprocess.run(["docker", "rm", "-f", "-v", NAME], capture_output=True)
        shutil.rmtree(work)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("system", choices=["nucleus", "pgvector"])
    ap.add_argument("args", nargs="+", help="nucleus: BINARY DIR; pgvector: DIR")
    ap.add_argument("--bulk", type=int, default=None)
    ap.add_argument("--ids", type=int, default=200)
    ap.add_argument("--lateral", type=int, default=1000)
    ap.add_argument("--ef", type=int, default=40)
    ap.add_argument("--port", type=int, default=21015)
    ap.add_argument("--host", default=os.environ.get("NUCLEUS_BENCH_HOST", "127.0.0.1"), help="the server's address (::1 where IPv4's ports run out)")
    ap.add_argument("--cache", default="3GB")
    ap.add_argument("--quantize", default=None)
    a = ap.parse_args()
    if a.system == "nucleus":
        a.binary, d = a.args
    else:
        (d,) = a.args
    meta = dict(kv.split("=") for kv in open(os.path.join(d, "meta.txt")).read().split())
    dim = int(meta["dim"])
    cosine = meta["metric"] == "cosine"
    op, ops = ("<=>", "vector_cosine_ops") if cosine else ("<->", "vector_l2_ops")
    train = np.fromfile(os.path.join(d, "train.f32"), dtype="<f4").reshape(-1, dim)
    a.bulk = min(a.bulk or len(train), len(train))
    train = train[: a.bulk]
    if a.system == "nucleus":
        nucleus_run(a, train, cosine, dim, op, ops)
    else:
        pgvector_run(a, train, cosine, dim, op, ops)


if __name__ == "__main__":
    main()
