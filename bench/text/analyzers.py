#!/usr/bin/env python3
"""How fast the text analyzers and text queries without an index are
(PLAN.md §M14.21 T1, T2): a BEIR corpus (title and text) loaded into a
table, then each analyzer's terms counted over every document through
SQL — `sum(cardinality(tokenize(body, a)))` — and the documents counted
that match each of a few queries — `count(*) WHERE body @@ q` — best of
three, on the server's workers as any scan.

    bench/text/analyzers.py nucleus BINARY [--data target/text-data/beir-fiqa/fiqa]
    bench/text/analyzers.py postgres [--container ncpg]

`postgres` runs the same over `to_tsvector` (its Snowball stemmers, its
default parser) in a PostgreSQL container: `length(to_tsvector(...))`
counts distinct terms a document, a little less work than ours; the
queries are `to_tsvector('simple', body) @@ q::tsquery`, without an index
as ours are.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

ANALYZERS = ["simple", "porter", "english", "hungarian", "trigram"]
PG = {"simple": "simple", "english": "english", "hungarian": "hungarian"}
# A query in nucleus's syntax and in PostgreSQL's tsquery (no fuzzy terms
# there).
QUERIES = [
    ("market", "market"),
    ("stock market", "stock | market"),
    ("stock AND market", "stock & market"),
    ('"stock market"', "stock <-> market"),
    ("invest*", "invest:*"),
    ("markte~1", None),
]


def corpus(d):
    ids, bodies = [], []
    for i, line in enumerate(open(os.path.join(d, "corpus.jsonl"), encoding="utf-8")):
        doc = json.loads(line)
        ids.append(i)
        bodies.append((doc.get("title") or "") + "\n" + (doc.get("text") or ""))
    return ids, bodies


def nucleus_run(a, ids, bodies):
    sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "drivers", "python"))
    import nucleus

    work = tempfile.mkdtemp(prefix="nucleus-text-")
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
        c.execute("CREATE TABLE docs (id INT PRIMARY KEY, body TEXT)")
        c.load("docs", {"id": ids, "body": bodies})
        mb = sum(len(b.encode()) for b in bodies) / 1e6
        print(f"nucleus ({os.path.basename(a.binary)}): {len(ids)} documents, {mb:.1f} MB", flush=True)
        for workers in ["default", "1"]:
            c.execute("RESET parallel_workers" if workers == "default" else f"SET parallel_workers = {workers}")
            for an in ANALYZERS:
                sql = f"SELECT sum(cardinality(tokenize(body, '{an}'))) FROM docs"
                best, n = 1e9, None
                for _ in range(3):
                    t = time.perf_counter()
                    n = c.execute(sql).rows[0][0]
                    best = min(best, time.perf_counter() - t)
                print(f"  {an:10} workers {workers:7}: {best * 1000:7.0f} ms, {n:>9} terms, "
                      f"{mb / best:6.1f} MB/s", flush=True)
            for q, _ in QUERIES:
                sql = "SELECT count(*) FROM docs WHERE body @@ '" + q.replace("'", "''") + "'"
                best, n = 1e9, None
                for _ in range(3):
                    t = time.perf_counter()
                    n = c.execute(sql).rows[0][0]
                    best = min(best, time.perf_counter() - t)
                print(f"  @@ {q:18} workers {workers:7}: {best * 1000:7.0f} ms, {n:>6} documents",
                      flush=True)
        c.execute("RESET parallel_workers")
        t = time.perf_counter()
        c.execute("CREATE INDEX docs_body ON docs USING text (body)")
        print(f"  CREATE INDEX USING text: {(time.perf_counter() - t) * 1000:7.0f} ms", flush=True)
        for q, _ in QUERIES:
            sql = "SELECT count(*) FROM docs WHERE body @@ '" + q.replace("'", "''") + "'"
            best, n = 1e9, None
            for _ in range(5):
                t = time.perf_counter()
                n = c.execute(sql).rows[0][0]
                best = min(best, time.perf_counter() - t)
            print(f"  @@ {q:18} indexed        : {best * 1000:7.1f} ms, {n:>6} documents",
                  flush=True)
        # The rows read too.
        sql = "SELECT sum(length(body)) FROM docs WHERE body @@ 'market'"
        best = 1e9
        for _ in range(5):
            t = time.perf_counter()
            n = c.execute(sql).rows[0][0]
            best = min(best, time.perf_counter() - t)
        print(f"  @@ market, rows read  indexed : {best * 1000:7.1f} ms, sum {n}", flush=True)
        c.close()
    finally:
        server.terminate()
        server.wait()
        shutil.rmtree(work)


def postgres_run(a, ids, bodies):
    def psql(sql, stdin=None):
        return subprocess.run(["docker", "exec", "-i", a.container, "psql", "-U", "postgres", "-qAt",
                               "-c", sql], input=stdin, capture_output=True, text=True, check=True).stdout

    psql("DROP TABLE IF EXISTS text_docs")
    psql("CREATE TABLE text_docs (id int PRIMARY KEY, body text)")
    rows = "".join(f"{i}\t" + b.replace("\\", "\\\\").replace("\t", "\\t").replace("\n", "\\n")
                   .replace("\r", "\\r") + "\n" for i, b in zip(ids, bodies))
    psql("COPY text_docs FROM STDIN", stdin=rows)
    psql("ANALYZE text_docs")
    mb = sum(len(b.encode()) for b in bodies) / 1e6
    print(f"postgres ({a.container}): {len(ids)} documents, {mb:.1f} MB", flush=True)
    for workers in ["default", "0"]:
        for an, cfg in PG.items():
            setting = "" if workers == "default" else "SET max_parallel_workers_per_gather = 0;"
            q = f"{setting} SELECT sum(length(to_tsvector('{cfg}', body))) FROM text_docs"
            best, n = 1e9, None
            for _ in range(3):
                t = time.perf_counter()
                n = psql(q).strip()
                best = min(best, time.perf_counter() - t)
            print(f"  {an:10} workers {workers:7}: {best * 1000:7.0f} ms, {n:>9} terms, "
                  f"{mb / best:6.1f} MB/s (psql round trip included)", flush=True)
        for q, tsq in QUERIES:
            if tsq is None:
                continue
            setting = "" if workers == "default" else "SET max_parallel_workers_per_gather = 0;"
            sql = (f"{setting} SELECT count(*) FROM text_docs "
                   f"WHERE to_tsvector('simple', body) @@ '{tsq}'::tsquery")
            best, n = 1e9, None
            for _ in range(3):
                t = time.perf_counter()
                n = psql(sql).strip()
                best = min(best, time.perf_counter() - t)
            print(f"  @@ {tsq:18} workers {workers:7}: {best * 1000:7.0f} ms, {n:>6} documents",
                  flush=True)
    t = time.perf_counter()
    psql("CREATE INDEX text_docs_gin ON text_docs USING gin (to_tsvector('simple', body))")
    print(f"  CREATE INDEX USING gin: {(time.perf_counter() - t) * 1000:7.0f} ms "
          "(psql round trip included)", flush=True)
    psql("ANALYZE text_docs")
    for q, tsq in QUERIES:
        if tsq is None:
            continue
        sql = (f"SELECT count(*) FROM text_docs "
               f"WHERE to_tsvector('simple', body) @@ '{tsq}'::tsquery")
        n = psql(sql).strip()
        # The server's own time: `docker exec` and `psql` cost ~50 ms a
        # call, more than the search.
        best = 1e9
        for _ in range(5):
            out = psql("EXPLAIN (ANALYZE, TIMING OFF) " + sql)
            ms = float(out.split("Execution Time: ")[1].split(" ms")[0])
            best = min(best, ms)
        print(f"  @@ {tsq:18} indexed        : {best:7.1f} ms, {n:>6} documents "
              "(the server's execution time)", flush=True)
    sql = ("SELECT sum(length(body)) FROM text_docs "
           "WHERE to_tsvector('simple', body) @@ 'market'::tsquery")
    best = 1e9
    for _ in range(5):
        out = psql("EXPLAIN (ANALYZE, TIMING OFF) " + sql)
        best = min(best, float(out.split("Execution Time: ")[1].split(" ms")[0]))
    print(f"  @@ market, rows read  indexed : {best:7.1f} ms, sum {psql(sql).strip()}", flush=True)
    psql("DROP TABLE text_docs")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("system", choices=["nucleus", "postgres"])
    ap.add_argument("binary", nargs="?")
    ap.add_argument("--data", default="target/text-data/beir-fiqa/fiqa")
    ap.add_argument("--port", type=int, default=21016)
    ap.add_argument("--host", default=os.environ.get("NUCLEUS_BENCH_HOST", "127.0.0.1"))
    ap.add_argument("--container", default="ncpg")
    a = ap.parse_args()
    ids, bodies = corpus(a.data)
    if a.system == "nucleus":
        nucleus_run(a, ids, bodies)
    else:
        postgres_run(a, ids, bodies)


if __name__ == "__main__":
    main()
