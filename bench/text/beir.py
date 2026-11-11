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

`--snippets` times the same queries with each row's `snippet(body @@ q)`
(§M14.21 T6; ParadeDB: `pdb.snippet(body)` in its own query).

`--layout column` keeps the documents in a `column` table, `--layout
partitioned` in a table partitioned by hash of `id` into 4 partitions,
the index on each (§M14.21 T8): the same scores, so the same nDCG@10.

`--filters 1,10,50` times the same queries with a second condition
passing that percentage of the rows (`n % 100 < p`, `n` a row's number),
ranked by the index (§M14.21 T5a) and, with `--unranked`, ordered by
`score(…) + 0`, which the index does not rank by: every match read and
scored on its text.  The first round checks both give the same rows.

MS MARCO passage (`--sets msmarco`, bench/text/data.py msmarco; §M14.21
T5b): 8.8 million passages, loaded in parts, with `--split dev` (BEIR's
own split for it; `test` is TREC DL 2019's 43 queries) and `--queries N`
(the first N, timed as the rest are).  Its index's segments, 1.09 GB,
must fit the server's index cache (`--index-cache`; by default a quarter
of the machine's memory), else they are decoded again on every search.

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


def corpus(d):
    """A set's documents, (id, title and text), in order."""
    for line in open(os.path.join(d, "corpus.jsonl"), encoding="utf-8"):
        doc = json.loads(line)
        yield doc["_id"], (doc.get("title") or "") + "\n" + (doc.get("text") or "")


def parts(d, size=200_000):
    """A set's documents in lists of `size`."""
    part = []
    for doc in corpus(d):
        part.append(doc)
        if len(part) == size:
            yield part
            part = []
    if part:
        yield part


def load(d, split="test", limit=None):
    """A set's queries and judgements (the first `limit` judged)."""
    queries = {}
    for line in open(os.path.join(d, "queries.jsonl"), encoding="utf-8"):
        q = json.loads(line)
        queries[q["_id"]] = q["text"]
    qrels = {}
    with open(os.path.join(d, "qrels", f"{split}.tsv"), encoding="utf-8") as f:
        next(f)
        for line in f:
            qid, did, rel = line.rstrip("\n").split("\t")
            if limit is not None and qid not in qrels and len(qrels) == limit:
                continue
            qrels.setdefault(qid, {})[did] = int(rel)
    return queries, qrels


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
        d = os.path.join(a.data, f"beir-{name}", name)
        queries, qrels = load(d, a.split, a.queries)
        psql("DROP TABLE IF EXISTS docs")
        psql("CREATE TABLE docs (id text PRIMARY KEY, body text)")
        # Through a file in the container: MS MARCO's 3.5 GB too.
        ndocs = 0
        with tempfile.NamedTemporaryFile("w", suffix=".tsv", encoding="utf-8") as f:
            for i, b in corpus(d):
                f.write(i.replace("\\", "\\\\").replace("\t", " ") + "\t" +
                        b.replace("\\", "\\\\").replace("\t", "\\t").replace("\n", "\\n")
                        .replace("\r", "\\r") + "\n")
                ndocs += 1
            f.flush()
            os.chmod(f.name, 0o644)  # the server reads it: not the temporary file's 600
            subprocess.run(["docker", "cp", f.name, f"{a.container}:/tmp/docs.tsv"], check=True)
        psql("COPY docs FROM '/tmp/docs.tsv'")
        subprocess.run(["docker", "exec", a.container, "rm", "/tmp/docs.tsv"], check=True)
        out = psql(stdin="\\timing on\n"
                   "CREATE INDEX docs_bm25 ON docs USING bm25 (id, (body::pdb.simple("
                   "'stemmer=english', 'stopwords_language=english')));\n")
        index_ms = float(out.split("Time: ")[1].split(" ms")[0])
        script = ["\\timing on"]
        order = list(qrels)
        for qid in order:
            q = SYNTAX.sub(" ", queries[qid]).lower()
            script.append(f"\\echo @@ {qid}")
            snippet = r", replace(pdb.snippet(body), E'\n', ' ')" if a.snippets else ""
            script.append(f"SELECT id{snippet} FROM docs WHERE body ||| {lit(q)} "
                          f"ORDER BY pdb.score(id) DESC LIMIT 11;")
        # Twice: the first round warms what the second measures.
        out = psql(stdin="\n".join(script + script[1:]) + "\n")
        runs = out.split("@@ ")[1:]
        runs = runs[len(order):]
        scores, times = [], []
        for qid, run in zip(order, runs):
            lines = run.strip().split("\n")[1:]
            times.append(float(lines[-1].split("Time: ")[1].split(" ms")[0]))
            ranked = [x.split("|")[0] for x in lines[:-1]]
            ranked = [x for x in ranked if x != qid][:10]
            scores.append(ndcg10(ranked, qrels[qid]))
        report(name, ndocs, scores, index_ms, times)
        psql("DROP TABLE docs")


def snippets(c, name, qrels, queries):
    """The queries with each row's snippet (§M14.21 T6): mean and median
    ms, the second of two rounds."""
    for _ in range(2):
        times = []
        for qid in qrels:
            q = lit(SYNTAX.sub(" ", queries[qid]).lower())
            t = time.perf_counter()
            c.execute(f"SELECT id, snippet(body @@ {q}) FROM docs WHERE body @@ {q} "
                      f"ORDER BY score(body @@ {q}) DESC, id LIMIT 11")
            times.append((time.perf_counter() - t) * 1000)
    times.sort()
    print(f"{name:10} snippets        {sum(times) / len(times):8.2f} mean "
          f"{times[len(times) // 2]:8.2f} p50", flush=True)


def filtered(c, name, qrels, queries, p, unranked):
    """The queries with `n % 100 < p` too: mean and median ms, ranked by
    the index and (`unranked`) by `score + 0`, checked alike."""
    def sql(q, by):
        return (f"SELECT id FROM docs WHERE body @@ {lit(q)} AND n % 100 < {p} "
                f"ORDER BY score(body @@ {lit(q)}){by} DESC, id LIMIT 11")

    def run(by, check=None):
        times = []
        for rnd in range(2):
            times = []
            for n, qid in enumerate(qrels):
                q = SYNTAX.sub(" ", queries[qid]).lower()
                t = time.perf_counter()
                rows = c.execute(sql(q, by)).rows
                times.append((time.perf_counter() - t) * 1000)
                if rnd == 0 and check is not None:
                    assert [r[0] for r in rows] == check[n], (name, p, qid)
                if rnd == 0 and check is None and unranked:
                    got.append([r[0] for r in rows])
        times.sort()
        return sum(times) / len(times), times[len(times) // 2]

    got = []
    mean, p50 = run("")
    line = f"{name:10} filter {p:3}%  ranked {mean:8.2f} mean {p50:8.2f} p50"
    if unranked:
        mean, p50 = run(" + 0", check=got)
        line += f"   unranked {mean:8.2f} mean {p50:8.2f} p50"
    print(line, flush=True)


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
    ap.add_argument("--filters", default="")
    ap.add_argument("--unranked", action="store_true")
    ap.add_argument("--snippets", action="store_true")
    ap.add_argument("--split", default="test")
    ap.add_argument("--queries", type=int, default=None)
    ap.add_argument("--layout", choices=["row", "column", "partitioned"], default="row")
    ap.add_argument("--index-cache", default=None,
                    help="the server's --index-cache (its default: a quarter of the machine's memory)")
    a = ap.parse_args()
    if a.system == "paradedb":
        paradedb(a)
        return
    sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "drivers", "python"))
    import nucleus

    work = tempfile.mkdtemp(prefix="nucleus-beir-")
    listen = f"[{a.host}]:{a.port}" if ":" in a.host else f"{a.host}:{a.port}"
    extra = ["--index-cache", a.index_cache] if a.index_cache else []
    server = subprocess.Popen([a.binary, "server", os.path.join(work, "db"), "--listen", listen,
                               "--no-auth", *extra], stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL)
    try:
        for _ in range(600):
            try:
                c = nucleus.connect(a.host, a.port, password=None)
                break
            except (OSError, nucleus.OperationalError):
                time.sleep(0.1)
        print(f"nucleus, BM25 k1 {a.k1}, b {a.b}, analyzer {a.analyzer}, {a.layout} table")
        print(HEADER)
        for name in a.sets.split(","):
            d = os.path.join(a.data, f"beir-{name}", name)
            queries, qrels = load(d, a.split, a.queries)
            c.execute("DROP TABLE IF EXISTS docs")
            if a.layout == "partitioned":
                c.execute("CREATE TABLE docs (id TEXT PRIMARY KEY, body TEXT, n INT) "
                          "PARTITION BY HASH (id)")
                for r in range(4):
                    c.execute(f"CREATE TABLE docs_{r} PARTITION OF docs "
                              f"FOR VALUES WITH (MODULUS 4, REMAINDER {r})")
            else:
                c.execute("CREATE TABLE docs (id TEXT PRIMARY KEY, body TEXT, n INT)"
                          + (" WITH (storage = column)" if a.layout == "column" else ""))
            ndocs = 0
            for part in parts(d):
                c.load("docs", {"id": [i for i, _ in part], "body": [b for _, b in part],
                                "n": list(range(ndocs, ndocs + len(part)))})
                ndocs += len(part)
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
            report(name, ndocs, scores, index_ms, times)
            if a.snippets:
                snippets(c, name, qrels, queries)
            for p in [int(x) for x in a.filters.split(",") if x]:
                filtered(c, name, qrels, queries, p, a.unranked)
        c.close()
    finally:
        server.terminate()
        server.wait()
        shutil.rmtree(work)


if __name__ == "__main__":
    main()
