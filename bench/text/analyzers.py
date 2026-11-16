#!/usr/bin/env python3
"""How fast the text analyzers and text queries without an index are
(PLAN.md §M14.21 T1, T2): a BEIR corpus (title and text) loaded into a
table, then each analyzer's terms counted over every document through
SQL — `sum(cardinality(tokenize(body, a)))` — and the documents counted
that match each of a few queries — `count(*) WHERE body @@ q` — best of
three, on the server's workers as any scan.

    bench/text/analyzers.py nucleus BINARY [--data target/text-data/beir-fiqa/fiqa]
    bench/text/analyzers.py postgres

`postgres` runs the same over `to_tsvector` (its Snowball stemmers, its
default parser) in PostgreSQL native (bench/pg.py's cluster), timed by
psql's `\\timing` in one session: `length(to_tsvector(...))`
counts distinct terms a document, a little less work than ours; the
queries are `to_tsvector('simple', body) @@ q::tsquery`, without an index
as ours are.  A time is the median of its runs; every number is printed
as an `@metric` line too, for `bench/method.py run` (PLAN.md §M14.31).
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import method  # noqa: E402

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
            system = "nucleus" if workers == "default" else "nucleus, one thread"
            for an in ANALYZERS:
                ms, n = timed(c, f"SELECT sum(cardinality(tokenize(body, '{an}'))) FROM docs", 3)
                print(f"  {an:10} workers {workers:7}: {ms:7.0f} ms, {n:>9} terms, "
                      f"{mb / ms * 1000:6.1f} MB/s", flush=True)
                method.emit(f"analyzer {an}", ms, system=system)
            for q, _ in QUERIES:
                ms, n = timed(c, "SELECT count(*) FROM docs WHERE body @@ '" + q.replace("'", "''") + "'", 3)
                print(f"  @@ {q:18} workers {workers:7}: {ms:7.0f} ms, {n:>6} documents", flush=True)
                method.emit(f"@@ {q}, no index", ms, system=system)
        c.execute("RESET parallel_workers")
        t = time.perf_counter()
        c.execute("CREATE INDEX docs_body ON docs USING text (body)")
        ms = (time.perf_counter() - t) * 1000
        print(f"  CREATE INDEX USING text: {ms:7.0f} ms", flush=True)
        method.emit("CREATE INDEX", ms)
        for q, _ in QUERIES:
            ms, n = timed(c, "SELECT count(*) FROM docs WHERE body @@ '" + q.replace("'", "''") + "'", 5)
            print(f"  @@ {q:18} indexed        : {ms:7.1f} ms, {n:>6} documents", flush=True)
            method.emit(f"@@ {q}, indexed", ms)
        # The rows read too.
        ms, n = timed(c, "SELECT sum(length(body)) FROM docs WHERE body @@ 'market'", 5)
        print(f"  @@ market, rows read  indexed : {ms:7.1f} ms, sum {n}", flush=True)
        method.emit("@@ market, rows read, indexed", ms)
        c.close()
    finally:
        server.terminate()
        server.wait()
        shutil.rmtree(work)


def timed(c, sql, times):
    """The median of `times` runs of `sql` (ms), and its one value."""
    t, n = [], None
    for _ in range(times):
        s = time.perf_counter()
        n = c.execute(sql).rows[0][0]
        t.append((time.perf_counter() - s) * 1000)
    return method.median(t), n


def postgres_run(a, ids, bodies):
    """The same on PostgreSQL native (bench/pg.py's cluster), timed by
    psql's `\\timing` in one session (the round trip on its socket, as
    the driver's is to nucleus)."""
    import pg
    from common import Session

    with pg.Cluster(pg.DEFAULT) as cluster:
        method.emit_system("postgresql", cluster.describe())
        cluster.sql("DROP TABLE IF EXISTS text_docs")
        cluster.sql("CREATE TABLE text_docs (id int PRIMARY KEY, body text)")
        rows = "".join(f"{i}\t" + b.replace("\\", "\\\\").replace("\t", "\\t").replace("\n", "\\n")
                       .replace("\r", "\\r") + "\n" for i, b in zip(ids, bodies))
        cluster.sql(file="COPY text_docs FROM STDIN;\n" + rows + "\\.\n")
        cluster.sql("VACUUM ANALYZE text_docs")
        s = Session(cluster.tool("psql") + ["-X", "-q", "-At"])
        try:
            def timed(sql, times):
                t, out = [], None
                for _ in range(times):
                    ms, out = s.query(sql)
                    t.append(ms)
                return method.median(t), out[-1] if out else None

            mb = sum(len(b.encode()) for b in bodies) / 1e6
            print(f"postgres: {len(ids)} documents, {mb:.1f} MB", flush=True)
            for workers in ["default", "0"]:
                s.send("RESET max_parallel_workers_per_gather;" if workers == "default"
                       else "SET max_parallel_workers_per_gather = 0;")
                system = "postgresql" if workers == "default" else "postgresql, one thread"
                for an, cfg in PG.items():
                    ms, n = timed(f"SELECT sum(length(to_tsvector('{cfg}', body))) FROM text_docs", 3)
                    print(f"  {an:10} workers {workers:7}: {ms:7.0f} ms, {n:>9} terms, "
                          f"{mb / ms * 1000:6.1f} MB/s", flush=True)
                    method.emit(f"analyzer {an}", ms, system=system)
                for q, tsq in QUERIES:
                    if tsq is None:
                        continue
                    ms, n = timed(f"SELECT count(*) FROM text_docs "
                                  f"WHERE to_tsvector('simple', body) @@ '{tsq}'::tsquery", 3)
                    print(f"  @@ {tsq:18} workers {workers:7}: {ms:7.0f} ms, {n:>6} documents", flush=True)
                    method.emit(f"@@ {q}, no index", ms, system=system)
            s.send("RESET max_parallel_workers_per_gather;")
            ms, _ = s.query("CREATE INDEX text_docs_gin ON text_docs USING gin (to_tsvector('simple', body))")
            print(f"  CREATE INDEX USING gin: {ms:7.0f} ms", flush=True)
            method.emit("CREATE INDEX", ms, system="postgresql")
            s.query("ANALYZE text_docs")
            for q, tsq in QUERIES:
                if tsq is None:
                    continue
                ms, n = timed(f"SELECT count(*) FROM text_docs "
                              f"WHERE to_tsvector('simple', body) @@ '{tsq}'::tsquery", 5)
                print(f"  @@ {tsq:18} indexed        : {ms:7.1f} ms, {n:>6} documents", flush=True)
                method.emit(f"@@ {q}, indexed", ms, system="postgresql")
            ms, n = timed("SELECT sum(length(body)) FROM text_docs "
                          "WHERE to_tsvector('simple', body) @@ 'market'::tsquery", 5)
            print(f"  @@ market, rows read  indexed : {ms:7.1f} ms, sum {n}", flush=True)
            method.emit("@@ market, rows read, indexed", ms, system="postgresql")
        finally:
            s.close()
            cluster.sql("DROP TABLE IF EXISTS text_docs")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("system", choices=["nucleus", "postgres"])
    ap.add_argument("binary", nargs="?")
    ap.add_argument("--data", default="target/text-data/beir-fiqa/fiqa")
    ap.add_argument("--port", type=int, default=21016)
    ap.add_argument("--host", default=os.environ.get("NUCLEUS_BENCH_HOST", "127.0.0.1"))
    a = ap.parse_args()
    ids, bodies = corpus(a.data)
    if a.system == "nucleus":
        nucleus_run(a, ids, bodies)
    else:
        postgres_run(a, ids, bodies)


if __name__ == "__main__":
    main()
