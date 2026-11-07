#!/usr/bin/env python3
"""Filtered nearest-neighbour searches (PLAN.md §M14.12 V5): "the ten
nearest among the rows a `WHERE` lets through", on a dataset in
bench/ann/data.py's format with a column `a` (0–999, a hash of the row's
id: independent of its vector) and a B-tree index on it, and `g` (`a`'s
hundreds: ten groups, as tenants), through nucleus's SQL or pgvector's
(image pgvector/pgvector:pg18, in Docker).  `--partition`: nucleus's
HNSW index `PARTITION BY (g)`, a graph per group.

    bench/ann/filtered.py nucleus BINARY DIR [--rows N] [--queries 500]
        [--filters 50,10,1,0.1,0.01] [--ef 40] [--port 21013] [--cache 3GB]
        [--keep DB] [--partition]
    bench/ann/filtered.py pgvector DIR [--rows N] [--queries 500] ...
        [--iterative off,relaxed_order,strict_order]

For each filter (its share of the rows, in %: `a < 500`, `a < 100`,
`a < 10`, `a = 7`, `a = 7 AND id % 10 = 3`): recall@10 against the exact answer among the rows
that pass, the rows returned (a search that finds fewer than 10 counts
them missing), and µs a search on one client — `SELECT id FROM t WHERE …
ORDER BY v <-> q LIMIT 10`, at `hnsw.ef_search` `--ef`.  Needs numpy
(bench/ann/data.py's virtual environment).
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

from pgvector_ref import NAME, IMAGE, copy_binary, exact, psql, sh  # noqa: E402

K = 10


def column(n):
    """`a` of rows 0..n: a multiplicative hash of the id, 0–999."""
    return ((np.arange(n, dtype=np.uint64) * np.uint64(2654435761)) % np.uint64(2**32) % np.uint64(1000)).astype(
        np.int32)


FILTERS = {
    "50": ("a < 500", lambda a, ids: a < 500),
    "10": ("a < 100", lambda a, ids: a < 100),
    "1": ("a < 10", lambda a, ids: a < 10),
    "0.1": ("a = 7", lambda a, ids: a == 7),
    "0.01": ("a = 7 AND id % 10 = 3", lambda a, ids: (a == 7) & (ids % 10 == 3)),
    # A group (a tenant), and a part of one.
    "g10": ("g = 3", lambda a, ids: a // 100 == 3),
    "g1": ("g = 3 AND a BETWEEN 300 AND 309", lambda a, ids: (a >= 300) & (a <= 309)),
}


def truths(train, a, test, names):
    """Each filter's exact answers: the ids of the `K` nearest that pass."""
    out = {}
    for name in names:
        ids = np.nonzero(FILTERS[name][1](a, np.arange(len(a))))[0]
        out[name] = [ids[t] for t in exact(train[ids], test, K)]
    return out


def score(found, truth):
    rec = sum(len(set(f) & set(t.tolist())) / K for f, t in zip(found, truth)) / len(truth)
    returned = sum(len(f) for f in found) / len(found)
    return rec, returned


def run_nucleus(a, train, col, test, truth):
    import nucleus
    work = a.keep or tempfile.mkdtemp(prefix="nucleus-ann-filtered-")
    loaded = a.keep and os.path.exists(os.path.join(work, "db"))
    server = subprocess.Popen([a.binary, "server", os.path.join(work, "db"), "--listen", f"127.0.0.1:{a.port}",
                               "--no-auth", "--cache", a.cache], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(600):
            try:
                c = nucleus.connect("127.0.0.1", a.port, password=None)
                break
            except (OSError, nucleus.OperationalError):
                time.sleep(0.1)
        dim = train.shape[1]
        t = time.perf_counter()
        if not loaded:
            c.execute(f"CREATE TABLE t (id INT PRIMARY KEY, a INT NOT NULL, g INT NOT NULL, v vector({dim}))")
            c.load("t", {"id": np.arange(len(train), dtype=np.int32), "a": col, "g": col // 100,
                         "v": list(train)})
            t = time.perf_counter()
            part = " PARTITION BY (g)" if a.partition else ""
            c.execute(f"CREATE INDEX t_v ON t USING hnsw (v vector_l2_ops){part} "
                      "WITH (m = 16, ef_construction = 64)")
            c.execute("CREATE INDEX t_a ON t (a)")
            c.execute("ANALYZE t")
        print(f"nucleus ({os.path.basename(a.binary)}): {len(train)} rows, indexed in "
              f"{time.perf_counter() - t:.1f} s; ef_search {a.ef}"
              f"{'; PARTITION BY (g)' if a.partition else ''}", flush=True)
        c.execute(f"SET hnsw.ef_search = {a.ef}")
        # Warm: the graph read into memory.
        for q in test[:50]:
            c.execute("SELECT id FROM t ORDER BY v <-> $1 LIMIT 10", (q,))
        print("filter  recall@10  returned  µs/search  plan")
        for name in a.filters:
            cond = FILTERS[name][0]
            sql = f"SELECT id FROM t WHERE {cond} ORDER BY v <-> $1 LIMIT {K}"
            plan = " / ".join(r[0].strip() for r in c.execute("EXPLAIN " + sql, (test[0],)).rows)
            # A database kept from before is read from disk on the first
            # round: the last round counts.
            for _ in range(2 if loaded else 1):
                found = []
                t = time.perf_counter()
                for q in test:
                    found.append([r[0] for r in c.execute(sql, (q,)).rows])
                lat = (time.perf_counter() - t) / len(test) * 1e6
            rec, ret = score(found, truth[name])
            print(f"{name + '%':>6}  {rec:>9.4f}  {ret:>8.2f}  {lat:>9.1f}  {plan}", flush=True)
        c.close()
    finally:
        server.terminate()
        server.wait()
        if not a.keep:
            shutil.rmtree(work)


def copy_rows(path, col, vecs):
    """(id, a, g, v) as PostgreSQL's binary COPY."""
    n, dim = vecs.shape
    rec = np.dtype([("nf", ">i2"), ("l1", ">i4"), ("id", ">i4"), ("l2", ">i4"), ("a", ">i4"), ("lg", ">i4"),
                    ("g", ">i4"), ("l3", ">i4"), ("dim", ">i2"), ("unused", ">i2"), ("v", ">f4", (dim,))])
    r = np.zeros(n, dtype=rec)
    r["nf"], r["l1"], r["id"], r["l2"], r["a"] = 4, 4, np.arange(n), 4, col
    r["lg"], r["g"] = 4, col // 100
    r["l3"], r["dim"], r["v"] = 4 + 4 * dim, dim, vecs
    with open(path, "wb") as f:
        f.write(b"PGCOPY\n\xff\r\n\x00" + (0).to_bytes(4, "big") + (0).to_bytes(4, "big"))
        f.write(r.tobytes())
        f.write((-1).to_bytes(2, "big", signed=True))


def run_pgvector(a, train, col, test, truth):
    work = tempfile.mkdtemp(prefix="pgvector-ann-filtered-")
    copy_rows(os.path.join(work, "rows.copy"), col, train)
    copy_binary(os.path.join(work, "queries.copy"), np.arange(len(test)), test)
    for name in a.filters:
        with open(os.path.join(work, f"search-{name}.sql"), "w") as f:
            f.write(f"\\set qid random(0, {len(test) - 1})\n"
                    f"SELECT id FROM t WHERE {FILTERS[name][0]} "
                    f"ORDER BY v <-> (SELECT v FROM q WHERE id = :qid) LIMIT {K};\n")
    os.chmod(work, 0o755)
    subprocess.run(["docker", "rm", "-f", NAME], capture_output=True)
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
        dim = train.shape[1]
        psql("CREATE EXTENSION vector;\n"
             f"CREATE TABLE t (id int PRIMARY KEY, a int NOT NULL, g int NOT NULL, v vector({dim}));\n"
             f"CREATE TABLE q (id int PRIMARY KEY, v vector({dim}));\n"
             "COPY t FROM '/data/rows.copy' (FORMAT binary);\n"
             "COPY q FROM '/data/queries.copy' (FORMAT binary);\n")
        t = time.perf_counter()
        psql("CREATE INDEX t_v ON t USING hnsw (v vector_l2_ops) WITH (m = 16, ef_construction = 64);\n"
             "CREATE INDEX t_a ON t (a);\nVACUUM ANALYZE t; VACUUM ANALYZE q;\n")
        print(f"pgvector ({IMAGE}): {len(train)} rows, indexed in {time.perf_counter() - t:.1f} s; "
              f"ef_search {a.ef}", flush=True)
        for mode in a.iterative:
            opts = f"-c hnsw.ef_search={a.ef} -c hnsw.iterative_scan={mode}"
            print(f"hnsw.iterative_scan = {mode}")
            print("filter  recall@10  returned  µs/search  plan")
            for name in a.filters:
                cond = FILTERS[name][0]
                plan = psql(f"SET hnsw.ef_search = {a.ef}; SET hnsw.iterative_scan = {mode};\n"
                            f"EXPLAIN (COSTS OFF) SELECT id FROM t WHERE {cond} ORDER BY v <-> "
                            f"(SELECT v FROM q WHERE id = 0) LIMIT {K};\n")
                plan = " / ".join(l.strip() for l in plan.splitlines() if l.strip())
                rows = psql(f"SET hnsw.ef_search = {a.ef}; SET hnsw.iterative_scan = {mode};\n"
                            f"SELECT q.id, array_to_string(array(SELECT t.id FROM t WHERE t.{cond} "
                            f"ORDER BY t.v <-> q.v LIMIT {K}), ',') FROM q ORDER BY q.id;\n").split()
                found = []
                for line in rows:
                    _, ids = line.split("|")
                    found.append([int(x) for x in ids.split(",") if x])
                out = subprocess.run(["docker", "exec", "-e", f"PGOPTIONS={opts}", NAME, "pgbench", "-n", "-U",
                                      "postgres", "-f", f"/data/search-{name}.sql", "-c", "1", "-j", "1", "-t",
                                      str(len(test)), "postgres"], check=True, capture_output=True, text=True).stdout
                lat = float(re.search(r"latency average = ([\d.]+) ms", out).group(1)) * 1000
                rec, ret = score(found, truth[name])
                print(f"{name + '%':>6}  {rec:>9.4f}  {ret:>8.2f}  {lat:>9.1f}  {plan[:160]}", flush=True)
    finally:
        subprocess.run(["docker", "rm", "-f", NAME], capture_output=True)
        shutil.rmtree(work)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("system", choices=["nucleus", "pgvector"])
    ap.add_argument("rest", nargs="+", help="nucleus: BINARY DIR; pgvector: DIR")
    ap.add_argument("--rows", type=int, default=None)
    ap.add_argument("--queries", type=int, default=500)
    ap.add_argument("--filters", default="50,10,1,0.1,0.01")
    ap.add_argument("--ef", type=int, default=40)
    ap.add_argument("--port", type=int, default=21013)
    ap.add_argument("--cache", default="3GB")
    ap.add_argument("--iterative", default="off,relaxed_order,strict_order")
    ap.add_argument("--keep", default=None, help="nucleus: a directory the database is kept in (and reused)")
    ap.add_argument("--partition", action="store_true", help="nucleus: the HNSW index PARTITION BY (g)")
    a = ap.parse_args()
    if a.system == "nucleus":
        a.binary, a.dir = a.rest
    else:
        (a.dir,) = a.rest
    a.filters = a.filters.split(",")
    a.iterative = a.iterative.split(",")
    meta = dict(kv.split("=") for kv in open(os.path.join(a.dir, "meta.txt")).read().split())
    dim = int(meta["dim"])
    assert meta["metric"] == "l2", "l2 only"
    train = np.fromfile(os.path.join(a.dir, "train.f32"), dtype="<f4").reshape(-1, dim)
    train = train[: a.rows or len(train)]
    test = np.fromfile(os.path.join(a.dir, "test.f32"), dtype="<f4").reshape(-1, dim)[: a.queries]
    col = column(len(train))
    truth = truths(train, col, test, a.filters)
    (run_nucleus if a.system == "nucleus" else run_pgvector)(a, train, col, test, truth)


if __name__ == "__main__":
    main()
