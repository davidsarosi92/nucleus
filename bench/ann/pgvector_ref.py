#!/usr/bin/env python3
"""pgvector's HNSW index (image pgvector/pgvector:pg18, in Docker) on a
dataset in bench/ann/data.py's format, run as `ann model` runs the
prototypes (PLAN.md §M14.12 V4): the first rows loaded and indexed, a
recall curve, rows inserted by concurrent writers while a searcher asks,
the curve again.

    bench/ann/pgvector_ref.py DIR [--rows N] [--bulk N] [--queries 2000]
        [--m 16] [--efc 64] [--threads 10] [--efs 10,40,200]
        [--quantize half|binary] [--rescore 5] [--no-inserts]

The clients run inside the container (psql and pgbench on its socket), so
Docker's network on macOS does not add to pgvector's latency.  µs/search
is one client asking the queries one after another; QPS is `--threads`
clients at once; recall@10 as `ann recall` counts it.  Writers insert one
row a transaction (autocommit) from a staging table, a sequence handing
out the rows.  A cosine dataset (stored normalized) is indexed with
vector_cosine_ops and searched by `<=>`.  `--quantize` is pgvector's way
to the codes nucleus's `WITH (quantize = …)` keeps (PLAN.md §M14.12 V6a),
as its documentation gives it: `half` an expression index on
`v::halfvec(n)`, searched and ordered by the halves' distance; `binary`
one on `binary_quantize(v)::bit(n)` (`bit_hamming_ops`), the `k ·
--rescore` nearest by the bits re-ranked by the vectors' distance in a
subquery.  Needs numpy (bench/ann/data.py's virtual environment).
"""

import argparse
import os
import re
import subprocess
import tempfile
import time

import numpy as np

NAME = "nucleus-ann-pg"
IMAGE = "pgvector/pgvector:pg18"


def sh(*args, **kw):
    return subprocess.run(list(args), check=True, capture_output=True, text=True, **kw)


def psql(sql, env=None):
    cmd = ["docker", "exec", "-i"]
    for k, v in (env or {}).items():
        cmd += ["-e", f"{k}={v}"]
    cmd += [NAME, "psql", "-q", "-At", "-U", "postgres", "-v", "ON_ERROR_STOP=1"]
    return subprocess.run(cmd, input=sql, check=True, capture_output=True, text=True).stdout


def pgbench(script, *args, ef=None):
    """pgbench on `script` (a path in the container); its output."""
    cmd = ["docker", "exec"]
    if ef is not None:
        cmd += ["-e", f"PGOPTIONS=-c hnsw.ef_search={ef}"]
    cmd += [NAME, "pgbench", "-n", "-U", "postgres", "-f", script, *args, "postgres"]
    return subprocess.run(cmd, check=True, capture_output=True, text=True)


def copy_binary(path, ids, vecs):
    """`ids` and `vecs` as PostgreSQL's binary COPY of (int, vector)."""
    n, dim = vecs.shape
    rec = np.dtype([("nf", ">i2"), ("l1", ">i4"), ("id", ">i4"), ("l2", ">i4"),
                    ("dim", ">i2"), ("unused", ">i2"), ("v", ">f4", (dim,))])
    a = np.zeros(n, dtype=rec)
    a["nf"], a["l1"], a["id"], a["l2"] = 2, 4, ids, 4 + 4 * dim
    a["dim"], a["v"] = dim, vecs
    with open(path, "wb") as f:
        f.write(b"PGCOPY\n\xff\r\n\x00" + (0).to_bytes(4, "big") + (0).to_bytes(4, "big"))
        f.write(a.tobytes())
        f.write((-1).to_bytes(2, "big", signed=True))


def exact(train, queries, k):
    """Each query's `k` nearest rows (squared L2, ties by row): candidates
    by a float32 product, then exact in float64."""
    out = []
    norms = (train * train).sum(axis=1)
    for b in range(0, len(queries), 200):
        qs = queries[b:b + 200]
        d = norms[None, :] - 2.0 * (qs @ train.T)
        cand = np.argpartition(d, k + 32, axis=1)[:, : k + 32]
        for q, c in zip(qs, cand):
            e = ((train[c].astype(np.float64) - q.astype(np.float64)) ** 2).sum(axis=1)
            out.append(c[np.lexsort((c, e))][:k])
    return out


# The search, `{q}` the query vector's expression (main sets it).
SEARCH = "SELECT t.id FROM t ORDER BY t.v <-> {q} LIMIT {k}"


def curve(queries, truth, k, threads, efs, n):
    print("ef_search  recall@10  µs/search  QPS(%d)" % threads)
    for ef in efs:
        rows = psql(f"SET hnsw.ef_search = {ef};\n"
                    f"SELECT q.id, array_to_string(array({SEARCH.format(q='q.v', k=k)}), ',') "
                    f"FROM q ORDER BY q.id;\n").split()
        total = 0.0
        for line in rows:
            qid, ids = line.split("|")
            got = set(int(x) for x in ids.split(",") if x)
            total += len(got & set(truth[int(qid)][:k].tolist())) / k
        one = pgbench("/data/search.sql", "-c", "1", "-j", "1", "-t", str(n), ef=ef).stdout
        lat = float(re.search(r"latency average = ([\d.]+) ms", one).group(1)) * 1000
        many = pgbench("/data/search.sql", "-c", str(threads), "-j", str(threads), "-t", str(n // threads), ef=ef).stdout
        tps = float(re.search(r"tps = ([\d.]+)", many).group(1))
        print(f"{ef:>9}  {total / len(rows):>9.4f}  {lat:>9.1f}  {tps:>8.0f}", flush=True)


def server_time(n, k, ef=40):
    """The server's own time a search (EXPLAIN ANALYZE's execution time,
    the plan's timing off) over the first `n` queries, at `ef`: what a
    client's round trip and its sending of the vector leave out."""
    script = f"SET hnsw.ef_search = {ef};\n" + "".join(
        f"EXPLAIN (ANALYZE, TIMING OFF, COSTS OFF) {SEARCH.format(q=f'(SELECT v FROM q WHERE id = {i})', k=k)};\n"
        for i in range(n))
    times = [float(m.group(1)) for m in re.finditer(r"Execution Time: ([\d.]+) ms", psql(script))]
    print(f"server-side (EXPLAIN ANALYZE, ef {ef}): {sum(times) / len(times) * 1000:.0f} µs a search", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dir")
    ap.add_argument("--rows", type=int, default=None)
    ap.add_argument("--bulk", type=int, default=None)
    ap.add_argument("--queries", type=int, default=2000)
    ap.add_argument("--m", type=int, default=16)
    ap.add_argument("--efc", type=int, default=64)
    ap.add_argument("--threads", type=int, default=10)
    ap.add_argument("--k", type=int, default=10)
    ap.add_argument("--efs", default="10,40,200")
    ap.add_argument("--shared-buffers", default="3GB")
    ap.add_argument("--work", default=None, help="the directory for the COPY files (a new temporary one)")
    ap.add_argument("--quantize", default=None, choices=["half", "binary"])
    ap.add_argument("--rescore", type=int, default=5, help="binary: rows re-ranked for each returned")
    ap.add_argument("--no-inserts", action="store_true", help="the bulk rows only: no writers, one curve")
    ap.add_argument("--server-efs", default="40", help="the ef_search values the server's own time is taken at")
    a = ap.parse_args()
    meta = dict(kv.split("=") for kv in open(os.path.join(a.dir, "meta.txt")).read().split())
    dim, nq = int(meta["dim"]), int(meta["test"])
    # Angular datasets are stored normalized: the Euclidean order is the
    # cosine one, so the ground truth is found the same way.
    cosine = meta["metric"] == "cosine"
    op, ops, half_ops = ("<=>", "vector_cosine_ops", "halfvec_cosine_ops") if cosine else \
        ("<->", "vector_l2_ops", "halfvec_l2_ops")
    global SEARCH
    if a.quantize == "half":
        index = f"(v::halfvec({dim})) {half_ops}"
        SEARCH = f"SELECT t.id FROM t ORDER BY t.v::halfvec({dim}) {op} {{q}}::halfvec({dim}) LIMIT {{k}}"
    elif a.quantize == "binary":
        index = f"(binary_quantize(v)::bit({dim})) bit_hamming_ops"
        SEARCH = (f"SELECT s.id FROM (SELECT t.id, t.v FROM t ORDER BY binary_quantize(t.v)::bit({dim}) <~> "
                  f"binary_quantize({{q}}) LIMIT {{k}} * {a.rescore}) s ORDER BY s.v {op} {{q}} LIMIT {{k}}")
    else:
        index = f"v {ops}"
        SEARCH = f"SELECT t.id FROM t ORDER BY t.v {op} {{q}} LIMIT {{k}}"
    train = np.fromfile(os.path.join(a.dir, "train.f32"), dtype="<f4").reshape(-1, dim)
    test = np.fromfile(os.path.join(a.dir, "test.f32"), dtype="<f4").reshape(-1, dim)[: a.queries]
    rows = min(a.rows or len(train), len(train))
    bulk = min(a.bulk or rows * 9 // 10, rows)
    efs = [int(x) for x in a.efs.split(",")]
    k = a.k

    work = a.work or tempfile.mkdtemp(prefix="pgvector-ann-")
    copy_binary(os.path.join(work, "bulk.copy"), np.arange(bulk), train[:bulk])
    copy_binary(os.path.join(work, "staged.copy"), np.arange(bulk, rows), train[bulk:rows])
    copy_binary(os.path.join(work, "queries.copy"), np.arange(len(test)), test)
    with open(os.path.join(work, "search.sql"), "w") as f:
        f.write(f"\\set qid random(0, {len(test) - 1})\n"
                + SEARCH.format(q="(SELECT v FROM q WHERE id = :qid)", k=k) + ";\n")
    with open(os.path.join(work, "insert.sql"), "w") as f:
        f.write("WITH n AS MATERIALIZED (SELECT nextval('staged_seq')::int AS id) "
                "INSERT INTO t SELECT s.id, s.v FROM n JOIN s ON s.id = n.id;\n")
    os.chmod(work, 0o755)

    subprocess.run(["docker", "rm", "-f", NAME], capture_output=True)
    sh("docker", "run", "-d", "--name", NAME, "-e", "POSTGRES_HOST_AUTH_METHOD=trust",
       "--shm-size=6g", "-v", f"{work}:/data", IMAGE,
       "-c", f"shared_buffers={a.shared_buffers}", "-c", "maintenance_work_mem=4GB",
       "-c", f"max_parallel_maintenance_workers={a.threads - 1}", "-c", "max_connections=50",
       "-c", "jit=off")
    try:
        for _ in range(300):
            if subprocess.run(["docker", "exec", NAME, "pg_isready", "-U", "postgres"],
                              capture_output=True).returncode == 0:
                break
            time.sleep(0.2)
        time.sleep(1)
        print(f"pgvector ({IMAGE}, hnsw m {a.m}, ef_construction {a.efc}, shared_buffers {a.shared_buffers}): "
              f"{rows} rows ({bulk} bulk, {rows - bulk} inserted by {a.threads} writers), dim {dim}", flush=True)
        psql("CREATE EXTENSION vector;\n"
             f"CREATE TABLE t (id int PRIMARY KEY, v vector({dim}));\n"
             f"CREATE TABLE s (id int PRIMARY KEY, v vector({dim}));\n"
             f"CREATE TABLE q (id int PRIMARY KEY, v vector({dim}));\n"
             f"CREATE SEQUENCE staged_seq START {bulk};\n"
             "COPY s FROM '/data/staged.copy' (FORMAT binary);\n"
             "COPY q FROM '/data/queries.copy' (FORMAT binary);\n"
             "VACUUM ANALYZE s; VACUUM ANALYZE q;\n")
        t = time.perf_counter()
        psql("COPY t FROM '/data/bulk.copy' (FORMAT binary);\n")
        load = time.perf_counter() - t
        t = time.perf_counter()
        psql(f"CREATE INDEX t_v_idx ON t USING hnsw ({index}) WITH (m = {a.m}, ef_construction = {a.efc});\n"
             "VACUUM ANALYZE t;\n")
        build = time.perf_counter() - t
        size = psql("SELECT pg_relation_size('t_v_idx') / 1048576, pg_total_relation_size('t') / 1048576;\n").strip()
        print(f"bulk: {bulk} rows loaded in {load:.1f} s, indexed in {build:.1f} s "
              f"({build * 1e6 / bulk:.1f} µs/row); index {size.split('|')[0]} MB, table with indexes "
              f"{size.split('|')[1]} MB", flush=True)
        truth = exact(train[:bulk], test, k)
        curve(test, truth, k, a.threads, efs, len(test))
        for ef in a.server_efs.split(","):
            server_time(min(200, len(test)), k, int(ef))
        if a.no_inserts:
            return

        # Writers and a searcher at once.
        n = rows - bulk
        per = n // a.threads
        searcher = subprocess.Popen(
            ["docker", "exec", "-e", "PGOPTIONS=-c hnsw.ef_search=40", NAME, "pgbench", "-n", "-U", "postgres",
             "-f", "/data/search.sql", "-c", "1", "-j", "1", "-T", "3600", "-P", "1", "postgres"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        time.sleep(1)
        t = time.perf_counter()
        pgbench("/data/insert.sql", "-c", str(a.threads), "-j", str(a.threads), "-t", str(per))
        secs = time.perf_counter() - t
        # The searcher stopped (the image has no pkill).
        subprocess.run(["docker", "exec", NAME, "sh", "-c",
                        "for p in /proc/[0-9]*; do grep -qx pgbench $p/comm 2>/dev/null && kill -INT ${p#/proc/}; done"],
                       capture_output=True)
        _, err = searcher.communicate(timeout=60)
        prog = [(float(m.group(1)), float(m.group(2)))
                for m in re.finditer(r"progress: [\d.]+ s, ([\d.]+) tps, lat ([\d.]+) ms", err)]
        prog = prog[1:-1] or prog
        searches = sum(tp for tp, _ in prog)
        lat = sum(tp * l for tp, l in prog) / max(searches, 1) * 1000
        inserted = int(psql("SELECT count(*) FROM t;\n")) - bulk
        print(f"inserts: {inserted} rows in {secs:.1f} s ({inserted / secs:.0f} rows/s); "
              f"searches meanwhile (ef 40): {searches:.0f}, {lat:.0f} µs each", flush=True)
        psql("VACUUM ANALYZE t;\n")
        truth = exact(train[:rows], test, k)
        print("after the inserts:")
        curve(test, truth, k, a.threads, efs, len(test))
    finally:
        subprocess.run(["docker", "rm", "-f", NAME], capture_output=True)


if __name__ == "__main__":
    main()
