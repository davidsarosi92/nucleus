#!/usr/bin/env python3
"""How fast the text analyzers are (PLAN.md §M14.21 T1): a BEIR corpus
(title and text) loaded into a table, then each analyzer's terms counted
over every document through SQL — `sum(cardinality(tokenize(body, a)))`
— best of three, on the server's workers as any scan.

    bench/text/analyzers.py nucleus BINARY [--data target/text-data/beir-fiqa/fiqa]
    bench/text/analyzers.py postgres [--container ncpg]

`postgres` runs the same over `to_tsvector` (its Snowball stemmers, its
default parser) in a PostgreSQL container: `length(to_tsvector(...))`
counts distinct terms a document, a little less work than ours.
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
