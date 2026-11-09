#!/usr/bin/env python3
"""How well BM25 ranks (PLAN.md §M14.21 T5): BEIR's SciFact, NFCorpus,
ArguAna and FiQA (bench/text/data.py beir) searched through SQL, scored
by nDCG@10 against their relevance judgements, beside Anserini's
published BM25 "flat" baseline — Lucene with title and text as one
field, its English analyzer (`porter` here), k1 0.9, b 0.4, each query
a bag of its words.

    bench/text/beir.py nucleus BINARY [--sets scifact,nfcorpus,arguana,fiqa]
                                      [--k1 0.9] [--b 0.4] [--analyzer porter]
    bench/text/beir.py paradedb [--container nucleus-paradedb]

`paradedb` runs the same in ParadeDB's pg_search (Tantivy's BM25 in
PostgreSQL; `docker run -d --name nucleus-paradedb -e POSTGRES_PASSWORD=x
paradedb/paradedb`): its English tokenizer (Snowball's stemmer, its stop
words) and Tantivy's k1 1.2, b 0.75 — nucleus's `english` analyzer and
defaults — and `body ||| q ORDER BY pdb.score(id) DESC LIMIT 11`, timed by
`\timing` in one psql session in the container.  nucleus's time is the
client's, over TCP: a round trip more.

A query is `SELECT id FROM docs WHERE body @@ q ORDER BY score(body @@ q)
DESC, id LIMIT 10`, with the query syntax's characters taken out of q (and
its words in lower case, so no AND / OR / NOT): bare words, OR'ed, as
Anserini's bag of words — a word twice counts twice there and here.
nDCG@10 is trec_eval's as Anserini runs it (`-c -m ndcg_cut.10`): linear
gains, every judged query (0 for one that retrieves nothing); ArguAna's query is a document of the corpus, which BEIR
leaves out of its own ranking (`ignore_identical_ids`), and so does this.
"""

import argparse
import json
import math
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

# Anserini's BM25 flat nDCG@10 (docs/regressions/regressions-beir-v1.0.0-*.md).
ANSERINI = {"scifact": 0.6789, "nfcorpus": 0.3218, "arguana": 0.3970, "fiqa": 0.2361}
SYNTAX = re.compile(r'[+\-&|!(){}\[\]^"~*?:\\/]')


def load(d):
    docs = []
    for line in open(os.path.join(d, "corpus.jsonl"), encoding="utf-8"):
        doc = json.loads(line)
        docs.append((doc["_id"], (doc.get("title") or "") + "\n" + (doc.get("text") or "")))
    queries = {}
    for line in open(os.path.join(d, "queries.jsonl"), encoding="utf-8"):
        q = json.loads(line)
        queries[q["_id"]] = q["text"]
    qrels = {}
    with open(os.path.join(d, "qrels", "test.tsv"), encoding="utf-8") as f:
        next(f)
        for line in f:
            qid, did, rel = line.rstrip("\n").split("\t")
            qrels.setdefault(qid, {})[did] = int(rel)
    return docs, queries, qrels


def ndcg10(ranked, rels):
    dcg = sum(rels.get(d, 0) / math.log2(i + 2) for i, d in enumerate(ranked[:10]))
    ideal = sorted((r for r in rels.values() if r > 0), reverse=True)[:10]
    idcg = sum(r / math.log2(i + 2) for i, r in enumerate(ideal))
    return dcg / idcg if idcg > 0 else 0.0


def lit(s):
    return "'" + s.replace("'", "''") + "'"


def report(name, docs, scores, index_ms, times):
    times.sort()
    print(f"{name:10} {docs:7} {len(scores):7} {sum(scores) / len(scores):8.4f} "
          f"{ANSERINI.get(name, float('nan')):8.4f} {index_ms:9.0f} "
          f"{sum(times) / len(times):9.2f} {times[len(times) // 2]:9.2f}", flush=True)


HEADER = (f"{'set':10} {'docs':>7} {'queries':>7} {'nDCG@10':>8} {'Anserini':>8} "
          f"{'index ms':>9} {'mean ms':>9} {'p50 ms':>9}")


def paradedb(a):
    def psql(sql=None, stdin=None):
        cmd = ["docker", "exec", "-i", a.container, "psql", "-U", "postgres", "-d", "paradedb",
               "-qAt", "-v", "ON_ERROR_STOP=1"]
        if sql is not None:
            cmd += ["-c", sql]
        return subprocess.run(cmd, input=stdin, capture_output=True, text=True,
                              check=True).stdout

    print("ParadeDB pg_search, English tokenizer, k1 1.2, b 0.75")
    print(HEADER)
    for name in a.sets.split(","):
        docs, queries, qrels = load(os.path.join(a.data, f"beir-{name}", name))
        psql("DROP TABLE IF EXISTS docs")
        psql("CREATE TABLE docs (id text PRIMARY KEY, body text)")
        rows = "".join(d.replace("\\", "\\\\").replace("\t", " ") + "\t" +
                       b.replace("\\", "\\\\").replace("\t", "\\t").replace("\n", "\\n")
                       .replace("\r", "\\r") + "\n" for d, b in docs)
        psql("COPY docs FROM STDIN", stdin=rows)
        out = psql(stdin="\\timing on\n"
                   "CREATE INDEX docs_bm25 ON docs USING bm25 (id, (body::pdb.simple("
                   "'stemmer=english', 'stopwords_language=english')));\n")
        index_ms = float(out.split("Time: ")[1].split(" ms")[0])
        script = ["\\timing on"]
        order = list(qrels)
        for qid in order:
            q = SYNTAX.sub(" ", queries[qid]).lower()
            script.append(f"\\echo @@ {qid}")
            script.append(f"SELECT id FROM docs WHERE body ||| {lit(q)} "
                          f"ORDER BY pdb.score(id) DESC LIMIT 11;")
        # Twice: the first round warms what the second measures.
        out = psql(stdin="\n".join(script + script[1:]) + "\n")
        runs = out.split("@@ ")[1:]
        runs = runs[len(order):]
        scores, times = [], []
        for qid, run in zip(order, runs):
            lines = run.strip().split("\n")[1:]
            times.append(float(lines[-1].split("Time: ")[1].split(" ms")[0]))
            ranked = [x for x in lines[:-1] if x != qid][:10]
            scores.append(ndcg10(ranked, qrels[qid]))
        report(name, len(docs), scores, index_ms, times)
        psql("DROP TABLE docs")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("system", choices=["nucleus", "paradedb"])
    ap.add_argument("binary", nargs="?")
    ap.add_argument("--container", default="nucleus-paradedb")
    ap.add_argument("--sets", default="scifact,nfcorpus,arguana,fiqa")
    ap.add_argument("--data", default="target/text-data")
    ap.add_argument("--k1", type=float, default=0.9)
    ap.add_argument("--b", type=float, default=0.4)
    ap.add_argument("--analyzer", default="porter")
    ap.add_argument("--port", type=int, default=21017)
    ap.add_argument("--host", default=os.environ.get("NUCLEUS_BENCH_HOST", "127.0.0.1"))
    a = ap.parse_args()
    if a.system == "paradedb":
        paradedb(a)
        return
    sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "drivers", "python"))
    import nucleus

    work = tempfile.mkdtemp(prefix="nucleus-beir-")
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
        print(f"nucleus, BM25 k1 {a.k1}, b {a.b}, analyzer {a.analyzer}")
        print(HEADER)
        for name in a.sets.split(","):
            docs, queries, qrels = load(os.path.join(a.data, f"beir-{name}", name))
            c.execute("DROP TABLE IF EXISTS docs")
            c.execute("CREATE TABLE docs (id TEXT PRIMARY KEY, body TEXT)")
            c.load("docs", {"id": [d for d, _ in docs], "body": [b for _, b in docs]})
            t = time.perf_counter()
            c.execute(f"CREATE INDEX docs_body ON docs USING text (body) WITH "
                      f"(analyzer = '{a.analyzer}', k1 = {a.k1}, b = {a.b})")
            index_ms = (time.perf_counter() - t) * 1000
            # Twice: the first round warms what the second measures.
            for _ in range(2):
                scores, times = [], []
                for qid, rels in qrels.items():
                    q = SYNTAX.sub(" ", queries[qid]).lower()
                    sql = (f"SELECT id FROM docs WHERE body @@ {lit(q)} "
                           f"ORDER BY score(body @@ {lit(q)}) DESC, id LIMIT 11")
                    t = time.perf_counter()
                    rows = c.execute(sql).rows
                    times.append((time.perf_counter() - t) * 1000)
                    ranked = [r[0] for r in rows if r[0] != qid][:10]
                    scores.append(ndcg10(ranked, rels))
            report(name, len(docs), scores, index_ms, times)
        c.close()
    finally:
        server.terminate()
        server.wait()
        shutil.rmtree(work)


if __name__ == "__main__":
    main()
