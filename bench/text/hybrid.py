#!/usr/bin/env python3
"""Hybrid search on BEIR (PLAN.md §M14.21 T7): BM25 alone, a dense model's
vectors alone, and the two fused — `rrf(...)` and `convex(..., α)` — in
one query each, scored by nDCG@10 as `beir.py` scores them, and timed.

    bench/text/hybrid.py nucleus BINARY [--sets scifact,nfcorpus,arguana,fiqa]
                                        [--alpha 0.5] [--analyzer english]
    bench/text/hybrid.py paradedb [--container nucleus-paradedb] [--sets …]

The vectors are `bench/text/embed.py`'s (BAAI/bge-small-en-v1.5 by
default, 384 dimensions, cosine), in a `vector(384)` column with an HNSW
index; the text with a text index (`english`, k1 1.2, b 0.75).  Queries:

    text    SELECT id FROM docs WHERE body @@ q ORDER BY score(body @@ q) DESC, id LIMIT 11
    vector  SELECT id FROM docs ORDER BY emb <=> $1, id LIMIT 11
    rrf     SELECT id FROM docs ORDER BY rrf(score(body @@ q), emb <=> $1) DESC, id LIMIT 11
    convex  SELECT id FROM docs ORDER BY convex(score(body @@ q), emb <=> $1, α) DESC, id LIMIT 11

(11: ArguAna's query document is left out, as `beir.py` does.)  The mean
and median ms of a query, the second of two rounds, through the protocol.

ParadeDB (pg_search's BM25, `beir.py`'s tokenizer, and pgvector's HNSW,
cosine, its defaults): text and vector alone, and RRF as ParadeDB's
documentation writes it — each ranking's first 100 rows in a CTE, ranked
by `RANK()`, joined `FULL OUTER` on the id — with `hnsw.ef_search` 100
for it (pgvector's HNSW returns at most `ef_search` rows), timed by
`psql`'s `\\timing` (the server's time, without the client's).
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..",
                                "drivers", "python"))
import beir  # noqa: E402
import nucleus  # noqa: E402


def vec(v):
    return "'[" + ",".join(f"{x:.7g}" for x in v) + "]'"


def sparse_rows(path):
    """`embed.py`'s CSR file as (dims, [(indices, values)] a row)."""
    z = np.load(path)
    ip, ix, vs, dims = z["indptr"], z["indices"], z["values"], int(z["dims"][0])
    return dims, [(ix[ip[r]:ip[r + 1]], vs[ip[r]:ip[r + 1]]) for r in range(len(ip) - 1)]


def sparse_text(at, vs, dims):
    return "{" + ",".join(f"{i + 1}:{v:.7g}" for i, v in zip(at, vs)) + "}/" + str(dims)


def paradedb(a):
    def psql(sql=None, stdin=None):
        cmd = ["docker", "exec", "-i", a.container, "psql", "-U", "postgres", "-d", "paradedb",
               "-qAt", "-v", "ON_ERROR_STOP=1"]
        if sql is not None:
            cmd += ["-c", sql]
        return subprocess.run(cmd, input=stdin, capture_output=True, text=True,
                              check=True).stdout

    print("ParadeDB hybrid: pg_search BM25 (English stemmer), pgvector HNSW cosine, RRF")
    print(f"{'set':10} {'mode':7} {'nDCG@10':>8} {'mean ms':>9} {'p50 ms':>9}")
    psql("CREATE EXTENSION IF NOT EXISTS vector")
    for name in a.sets.split(","):
        d = os.path.join(a.data, f"beir-{name}", name)
        ids = json.load(open(os.path.join(d, "ids.json")))
        emb = np.load(os.path.join(d, "dense-docs.npy"))
        qemb = np.load(os.path.join(d, "dense-queries.npy"))
        queries, qrels = beir.load(d)
        assert list(qrels) == ids["queries"], name
        psql("DROP TABLE IF EXISTS docs")
        sp = a.sparse and os.path.exists(os.path.join(d, "sparse-docs.npz"))
        if sp:
            dims, sdocs = sparse_rows(os.path.join(d, "sparse-docs.npz"))
            _, squeries = sparse_rows(os.path.join(d, "sparse-queries.npz"))
        psql(f"CREATE TABLE docs (id text PRIMARY KEY, body text, emb vector({emb.shape[1]})"
             + (f", sv sparsevec({dims}))" if sp else ")"))
        with tempfile.NamedTemporaryFile("w", suffix=".tsv", encoding="utf-8") as f:
            for n, (i, b) in enumerate(beir.corpus(d)):
                assert i == ids["docs"][n], name
                f.write(i.replace("\\", "\\\\").replace("\t", " ") + "\t" +
                        b.replace("\\", "\\\\").replace("\t", "\\t").replace("\n", "\\n")
                        .replace("\r", "\\r") + "\t" + vec(emb[n])[1:-1]
                        + ("\t" + sparse_text(*sdocs[n], dims) if sp else "") + "\n")
            f.flush()
            os.chmod(f.name, 0o644)
            subprocess.run(["docker", "cp", f.name, f"{a.container}:/tmp/docs.tsv"], check=True)
        psql("COPY docs FROM '/tmp/docs.tsv'")
        subprocess.run(["docker", "exec", a.container, "rm", "/tmp/docs.tsv"], check=True)
        psql("CREATE INDEX docs_bm25 ON docs USING bm25 (id, (body::pdb.simple("
             "'stemmer=english', 'stopwords_language=english')))")
        # One process: a parallel build's shared memory outgrows the
        # container's /dev/shm (64 MB) on ArguAna.
        psql(stdin="SET max_parallel_maintenance_workers = 0;\n"
                   "CREATE INDEX docs_emb ON docs USING hnsw (emb vector_cosine_ops);\n"
                   + ("CREATE INDEX docs_sv ON docs USING hnsw (sv sparsevec_ip_ops);\n"
                      if sp else ""))
        psql("ANALYZE docs")
        order = list(qrels)
        modes = {
            "text": ("", "SELECT id FROM docs WHERE body ||| {q} "
                         "ORDER BY pdb.score(id) DESC LIMIT 11;"),
            "vector": ("", "SELECT id FROM docs ORDER BY emb <=> {v} LIMIT 11;"),
            "rrf": ("SET hnsw.ef_search = 100;",
                    "WITH t AS (SELECT id, pdb.score(id) AS s FROM docs WHERE body ||| {q} "
                    "ORDER BY pdb.score(id) DESC LIMIT 100), "
                    "v AS (SELECT id, emb <=> {v} AS d FROM docs ORDER BY emb <=> {v} LIMIT 100), "
                    "tr AS (SELECT id, RANK() OVER (ORDER BY s DESC) AS r FROM t), "
                    "vr AS (SELECT id, RANK() OVER (ORDER BY d) AS r FROM v) "
                    "SELECT COALESCE(tr.id, vr.id) AS id, "
                    "COALESCE(1.0 / (60 + tr.r), 0) + COALESCE(1.0 / (60 + vr.r), 0) AS f "
                    "FROM tr FULL OUTER JOIN vr ON tr.id = vr.id ORDER BY f DESC, id LIMIT 11;"),
        }
        if sp:
            modes["splade"] = ("SET hnsw.ef_search = 40;",
                               "SELECT id FROM docs ORDER BY sv <#> {s} LIMIT 11;")
        for mode, (setup, sql) in modes.items():
            script = ["\\timing on"]
            for n, qid in enumerate(order):
                q = beir.lit(beir.SYNTAX.sub(" ", queries[qid]).lower())
                script.append(f"\\echo @@ {qid}")
                s = "'" + sparse_text(*squeries[n], dims) + "'" if sp else ""
                script.append(sql.format(q=q, v=vec(qemb[n]), s=s))
            # Twice: the first round warms what the second measures.
            out = psql(stdin=setup + "\n" + "\n".join(script + script[1:]) + "\n")
            runs = out.split("@@ ")[1:][len(order):]
            scores, times = [], []
            for qid, run in zip(order, runs):
                lines = run.strip().split("\n")[1:]
                times.append(float(lines[-1].split("Time: ")[1].split(" ms")[0]))
                ranked = [x.split("|")[0] for x in lines[:-1]]
                ranked = [x for x in ranked if x != qid][:10]
                scores.append(beir.ndcg10(ranked, qrels[qid]))
            times.sort()
            print(f"{name:10} {mode:7} {sum(scores) / len(scores):8.4f} "
                  f"{sum(times) / len(times):9.2f} {times[len(times) // 2]:9.2f}", flush=True)
        psql("DROP TABLE docs")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("system", choices=["nucleus", "paradedb"])
    ap.add_argument("binary", nargs="?")
    ap.add_argument("--container", default="nucleus-paradedb")
    ap.add_argument("--sets", default="scifact,nfcorpus,arguana,fiqa")
    ap.add_argument("--data", default="target/text-data")
    ap.add_argument("--analyzer", default="english")
    ap.add_argument("--alpha", type=float, default=0.5)
    ap.add_argument("--sparse", action="store_true",
                    help="SPLADE's vectors too, where embed.py made them")
    ap.add_argument("--port", type=int, default=21018)
    ap.add_argument("--host", default=os.environ.get("NUCLEUS_BENCH_HOST", "127.0.0.1"))
    a = ap.parse_args()
    if a.system == "paradedb":
        return paradedb(a)

    work = tempfile.mkdtemp(prefix="nucleus-hybrid-")
    listen = f"[{a.host}]:{a.port}" if ":" in a.host else f"{a.host}:{a.port}"
    server = subprocess.Popen([a.binary, "server", os.path.join(work, "db"), "--listen", listen,
                               "--no-auth"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(600):
            try:
                c = nucleus.connect(a.host, a.port, password=None)
                break
            except (OSError, nucleus.OperationalError):
                time.sleep(0.1)
        print(f"nucleus hybrid, {a.analyzer} BM25 1.2/0.75, bge-small-en-v1.5 cosine, α {a.alpha}")
        print(f"{'set':10} {'mode':7} {'nDCG@10':>8} {'mean ms':>9} {'p50 ms':>9}")
        for name in a.sets.split(","):
            d = os.path.join(a.data, f"beir-{name}", name)
            ids = json.load(open(os.path.join(d, "ids.json")))
            docs = list(beir.corpus(d))
            assert [i for i, _ in docs] == ids["docs"], name
            emb = np.load(os.path.join(d, "dense-docs.npy"))
            qemb = np.load(os.path.join(d, "dense-queries.npy"))
            queries, qrels = beir.load(d)
            assert list(qrels) == ids["queries"], name
            dim = emb.shape[1]
            sp = a.sparse and os.path.exists(os.path.join(d, "sparse-docs.npz"))
            c.execute("DROP TABLE IF EXISTS docs")
            cols = {"id": [i for i, _ in docs], "body": [b for _, b in docs], "emb": list(emb)}
            if sp:
                dims, sdocs = sparse_rows(os.path.join(d, "sparse-docs.npz"))
                _, squeries = sparse_rows(os.path.join(d, "sparse-queries.npz"))
                cols["sv"] = [nucleus.SparseVector(dims, tuple(int(x) for x in at),
                                                   tuple(float(x) for x in vs))
                              for at, vs in sdocs]
            c.execute(f"CREATE TABLE docs (id TEXT PRIMARY KEY, body TEXT, emb vector({dim})"
                      + (f", sv sparsevec({dims}))" if sp else ")"))
            c.load("docs", cols)
            c.execute(f"CREATE INDEX docs_body ON docs USING text (body) WITH "
                      f"(analyzer = '{a.analyzer}')")
            c.execute("CREATE INDEX docs_emb ON docs USING hnsw (emb vector_cosine_ops)")
            if sp:
                t = time.perf_counter()
                c.execute("CREATE INDEX docs_sv ON docs USING sparse (sv)")
                print(f"{name:10} sparse index {(time.perf_counter() - t) * 1000:.0f} ms",
                      flush=True)
            c.execute("ANALYZE docs")
            modes = {
                "text": "SELECT id FROM docs WHERE body @@ {q} ORDER BY score(body @@ {q}) "
                        "DESC, id LIMIT 11",
                "vector": "SELECT id FROM docs ORDER BY emb <=> $1, id LIMIT 11",
                "rrf": "SELECT id FROM docs ORDER BY rrf(score(body @@ {q}), emb <=> $1) "
                       "DESC, id LIMIT 11",
                "convex": "SELECT id FROM docs ORDER BY convex(score(body @@ {q}), emb <=> $1, "
                          f"{a.alpha}) DESC, id LIMIT 11",
            }
            if sp:
                modes["splade"] = "SELECT id FROM docs ORDER BY sv <#> $1, id LIMIT 11"
                modes["rrf3"] = ("SELECT id FROM docs ORDER BY rrf(score(body @@ {q}), "
                                 "emb <=> $1, sv <#> $2) DESC, id LIMIT 11")
            for mode, sql in modes.items():
                for _ in range(2):
                    scores, times = [], []
                    for n, (qid, rels) in enumerate(qrels.items()):
                        q = beir.lit(beir.SYNTAX.sub(" ", queries[qid]).lower())
                        s = sql.format(q=q)
                        params, sq = (), None
                        if sp:
                            at, vs = squeries[n]
                            sq = nucleus.SparseVector(dims, tuple(int(x) for x in at),
                                                      tuple(float(x) for x in vs))
                        if mode == "splade":
                            params = (sq,)
                        elif "$1" in s:
                            params = (qemb[n], sq) if "$2" in s else (qemb[n],)
                        t = time.perf_counter()
                        rows = c.execute(s, params).rows
                        times.append((time.perf_counter() - t) * 1000)
                        ranked = [r[0] for r in rows if r[0] != qid][:10]
                        scores.append(beir.ndcg10(ranked, rels))
                times.sort()
                print(f"{name:10} {mode:7} {sum(scores) / len(scores):8.4f} "
                      f"{sum(times) / len(times):9.2f} {times[len(times) // 2]:9.2f}", flush=True)
        c.close()
    finally:
        server.terminate()
        server.wait()
        shutil.rmtree(work)


if __name__ == "__main__":
    main()
