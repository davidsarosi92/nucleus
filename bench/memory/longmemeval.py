#!/usr/bin/env python3
"""§M14.24 R1's measurement: memory stores' retrieval on LongMemEval
(Wu et al., ICLR 2025), scored as its own code scores retrieval.

    bench/memory/longmemeval.py BINARY [--data target/memory-data/longmemeval_s_cleaned.json]
                                       [--models target/models] [--model bge-small-en-v1.5]
                                       [--questions N]

In a new server (`BINARY server DIR --no-auth`):

1. `CREATE MODEL bge USING local WITH (path = …)` and `CREATE MEMORY
   STORE mem USING bge`.
2. Every question's history loaded into it as memories: each user turn a
   memory of the subject `question_id` (the paper's protocol keeps only
   the user side), its `source` the turn's id as the paper's code names
   it (`<session>_<turn number among all turns>`).  The 30 abstention
   questions are left out, as the paper leaves them out of retrieval.
   The embedding column's queue drained.
3. Each question ranked against its subject's memories — by `recall()`
   (BM25 and vectors fused), by BM25 alone and by vectors alone in SQL,
   by the vectors searched exactly (numpy), and by the paper's own BM25
   (`rank_bm25`'s `BM25Okapi`: tokens split on spaces, an index per
   question) re-implemented here — and scored with the paper's code's
   metrics: turn level, and session level by turns (the first `k`
   distinct sessions of the ranked turns); `recall_all@k` (every
   evidence turn or session in the first `k`) and `ndcg_any@k`.  A
   question with no evidence on the user side is skipped, as there.
4. `REMEMBER`'s and the queries' times: one question's turns (changed,
   so the model's cache does not answer) remembered one at a time into a
   second store; every question's queries as they ran (the table not
   analyzed — as a store is until `ANALYZE`; the question's vector made
   by its first), then again after `ANALYZE mem` with the vector made
   before (the queries alone).  `REMEMBER`s last: their vectors are made
   by the background queue, which would share the cores with queries.

Run it under a memory cap (`target/bench-tools/memcap.sh`).
"""

import argparse
import json
import math
import os
import re
import shutil
import statistics
import sys
import tempfile
import time

import numpy as np

import importlib.util

here = os.path.dirname(os.path.abspath(__file__))
# bench/embed/beir.py's server and queue helpers (by path: it imports
# bench/text/beir.py as `beir` itself).
_spec = importlib.util.spec_from_file_location(
    "embed_beir", os.path.join(here, "..", "embed", "beir.py"))
_embed = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_embed)
Server, jobs, lit = _embed.Server, _embed.jobs, _embed.lit

K_TURN = (5, 10, 50)
K_SESSION = (5, 10)
DEPTH = 200  # turns ranked per question


def turns(q):
    """A question's user turns: (id, text, evidence)."""
    out = []
    for sid, session in zip(q["haystack_session_ids"], q["haystack_sessions"]):
        for i, t in enumerate(session):
            if t["role"] == "user":
                out.append((f"{sid}_{i + 1}", t["content"], bool(t.get("has_answer"))))
    return out


def session_of(turn_id):
    return re.sub(r"_\d+$", "", turn_id)


def recall_all(ranked, gold, k):
    top = set(ranked[:k])
    return float(all(g in top for g in gold))


def ndcg_any(ranked, gold, k):
    dcg = sum(1 / math.log2(i + 2) for i, d in enumerate(ranked[:k]) if d in gold)
    ideal = sum(1 / math.log2(i + 2) for i in range(min(len(gold), k)))
    return dcg / ideal if ideal else 0.0


def sessions(ranked):
    out = []
    for t in ranked:
        s = session_of(t)
        if s not in out:
            out.append(s)
    return out


def bm25_okapi(docs, query, k1=1.5, b=0.75, epsilon=0.25):
    """rank_bm25's BM25Okapi, as LongMemEval's flat-bm25 calls it."""
    corpus = [d.split(" ") for d in docs]
    n = len(corpus)
    lengths = np.array([len(d) for d in corpus], dtype=np.float64)
    avgdl = lengths.mean()
    freqs = []
    df = {}
    for d in corpus:
        f = {}
        for w in d:
            f[w] = f.get(w, 0) + 1
        freqs.append(f)
        for w in f:
            df[w] = df.get(w, 0) + 1
    idf = {}
    negative = []
    total = 0.0
    for w, c in df.items():
        v = math.log(n - c + 0.5) - math.log(c + 0.5)
        idf[w] = v
        total += v
        if v < 0:
            negative.append(w)
    floor = epsilon * total / len(idf)
    for w in negative:
        idf[w] = floor
    scores = np.zeros(n)
    for w in query.split(" "):
        tf = np.array([f.get(w, 0) for f in freqs], dtype=np.float64)
        scores += idf.get(w, 0) * (tf * (k1 + 1) / (tf + k1 * (1 - b + b * lengths / avgdl)))
    return list(np.argsort(scores)[::-1])


def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("binary")
    p.add_argument("--data", default="target/memory-data/longmemeval_s_cleaned.json")
    p.add_argument("--models", default="target/models")
    p.add_argument("--model", default="bge-small-en-v1.5")
    p.add_argument("--questions", type=int, default=0, help="the first N only (0: all)")
    p.add_argument("--port", type=int, default=21024)
    p.add_argument("--db", help="keep the database in this directory, and use it again "
                                "when it is there (else a temporary one)")
    a = p.parse_args()

    data = [q for q in json.load(open(a.data)) if "_abs" not in q["question_id"]]
    if a.questions:
        data = data[:a.questions]
    work = None if a.db else tempfile.mkdtemp(prefix="nucleus-longmemeval-")
    again = bool(a.db) and os.path.isdir(a.db)
    server = Server(a.binary, a.db or os.path.join(work, "db"), a.port)
    try:
        c = server.start()
        if again:
            n = c.execute("SELECT count(*) FROM mem").rows[0][0]
            print(f"{len(data)} questions, {n} memories: the database kept in {a.db}")
        else:
            n = load(c, a, data)
        rank(c, data, n)
    finally:
        server.stop()
        if work:
            shutil.rmtree(work, ignore_errors=True)


def load(c, a, data):
    """The memories loaded and embedded; how many."""
    if True:
        c.execute(f"CREATE MODEL bge USING local WITH (path = "
                  f"{lit(os.path.abspath(os.path.join(a.models, a.model)))})")
        c.execute("CREATE MEMORY STORE mem USING bge")
        t = time.time()
        n = 0
        for i in range(0, len(data), 20):
            subject, content, source = [], [], []
            for q in data[i:i + 20]:
                for tid, text, _ in turns(q):
                    subject.append(q["question_id"])
                    content.append(text)
                    source.append(tid)
            c.load("mem", {"subject": subject, "content": content, "source": source})
            n += len(subject)
        load_s = time.time() - t
        njobs, _ = jobs(c)
        t = time.time()
        while True:
            left, failed = jobs(c)
            if failed:
                raise RuntimeError(f"{failed} jobs failed")
            if left == 0:
                break
            time.sleep(1)
        embed_s = time.time() - t
        ntokens = c.execute("SELECT tokens FROM nc_catalog.models WHERE name = 'bge'").rows[0][0]
        print(f"{len(data)} questions, {n} memories: loaded in {load_s:.1f} s; {njobs} jobs "
              f"embedded in {embed_s:.0f} s ({ntokens} tokens, {ntokens / embed_s:.0f} a second)",
              flush=True)
        return n


def rank(c, data, n):
    """Each question ranked and scored; the times."""
    if True:
        rankers = ["recall", "bm25", "vector", "vector exact", "paper bm25"]
        scores = {r: {} for r in rankers}
        # Each run first for its question: its query's vector is computed
        # by recall's (the model's cache answers the others).
        sql_rankers = [
            ("recall", f"SELECT source FROM recall('mem', $1, $2, {DEPTH})"),
            ("bm25", f"SELECT source FROM mem WHERE subject = $1 AND content @@ $2 "
                     f"ORDER BY score(content @@ $2) DESC LIMIT {DEPTH}"),
            ("vector", f"SELECT source FROM mem WHERE subject = $1 "
                       f"ORDER BY embedding <=> embed($2) LIMIT {DEPTH}"),
        ]
        times = {r: [] for r, _ in sql_rankers}
        skipped = 0
        for qi, q in enumerate(data):
            ts = turns(q)
            gold = {tid for tid, _, ev in ts if ev}
            if not gold:
                skipped += 1
                continue
            qid, question = q["question_id"], q["question"]
            ranked = {}
            for r, sql in sql_rankers:
                t = time.perf_counter()
                ranked[r] = [x[0] for x in c.execute(sql, (qid, question)).rows]
                times[r].append(time.perf_counter() - t)
            rows = c.execute("SELECT source, embedding, embed($2, 'bge', 'query') FROM mem "
                             "WHERE subject = $1", (qid, question)).rows
            ids = [r[0] for r in rows]
            m = np.array([np.frombuffer(r[1], dtype=np.float32) for r in rows])
            qv = np.frombuffer(rows[0][2], dtype=np.float32)
            s = (m / np.linalg.norm(m, axis=1, keepdims=True)) @ (qv / np.linalg.norm(qv))
            ranked["vector exact"] = [ids[i] for i in np.argsort(-s, kind="stable")[:DEPTH]]
            order = bm25_okapi([text for _, text, _ in ts], question)
            ranked["paper bm25"] = [ts[i][0] for i in order[:DEPTH]]
            gold_s = {session_of(g) for g in gold}
            for r in rankers:
                row = scores[r].setdefault(q["question_type"], [])
                rs = sessions(ranked[r])
                row.append([recall_all(ranked[r], gold, k) for k in K_TURN]
                           + [ndcg_any(ranked[r], gold, k) for k in K_TURN]
                           + [recall_all(rs, gold_s, k) for k in K_SESSION]
                           + [ndcg_any(rs, gold_s, k) for k in K_SESSION])
            if qi % 50 == 0:
                print(f"  {qi} ranked", file=sys.stderr, flush=True)

        print(f"\n{len(data) - skipped} questions scored ({skipped} without evidence on the "
              f"user side skipped)")
        head = ([f"T R@{k}" for k in K_TURN] + [f"T N@{k}" for k in K_TURN]
                + [f"S R@{k}" for k in K_SESSION] + [f"S N@{k}" for k in K_SESSION])
        print(f"{'':14}" + "".join(f"{h:>8}" for h in head))
        for r in rankers:
            allrows = [x for rows in scores[r].values() for x in rows]
            mean = np.array(allrows).mean(axis=0)
            print(f"{r:14}" + "".join(f"{v:8.3f}" for v in mean))
        print("\nrecall() by question type (turn R@10, session R@5):")
        for qt in sorted(scores["recall"]):
            m = np.array(scores["recall"][qt]).mean(axis=0)
            b = np.array(scores["paper bm25"][qt]).mean(axis=0)
            print(f"  {qt:28} {len(scores['recall'][qt]):4}  recall {m[1]:.3f} {m[6]:.3f}   "
                  f"paper bm25 {b[1]:.3f} {b[6]:.3f}")
        # Again after ANALYZE, the questions' vectors made first (the
        # model's cache answers): the queries alone, as statistics plan them.
        analyzed = {r: [] for r, _ in sql_rankers}
        analyzed["recall, k=10"] = []
        c.execute("ANALYZE mem")
        for q in data:
            c.execute("SELECT embed($1, 'bge', 'query')", (q["question"],))
            for r, sql in sql_rankers + [("recall, k=10", sql_rankers[0][1].replace(
                    str(DEPTH), "10"))]:
                t = time.perf_counter()
                c.execute(sql, (q["question_id"], q["question"]))
                analyzed[r].append(time.perf_counter() - t)

        # REMEMBER one at a time: a question's turns into a store of their own.
        c.execute("CREATE MEMORY STORE IF NOT EXISTS mem2 USING bge")
        c.execute("DELETE FROM mem2")
        sample = turns(data[0])
        remember = []
        nonce = os.urandom(4).hex()
        for tid, text, _ in sample:
            # A text the model has not seen (its cache would answer).
            text = f"Remembered ({nonce}): {text}"
            t = time.perf_counter()
            c.remember("mem2", "u", text, source=tid)
            remember.append(time.perf_counter() - t)
        remember.sort()

        def show(r, ts):
            ts.sort()
            print(f"  {r:13} median {statistics.median(ts) * 1000:7.2f} ms, "
                  f"p95 {ts[int(len(ts) * 0.95)] * 1000:7.2f} ms")
        print(f"\nOver ~{n // len(data)} memories of a subject among {n} (client to server, "
              f"TCP), k={DEPTH}.  Not analyzed, the query's vector made in recall's:")
        for r, ts in times.items():
            show(r, ts)
        print("Analyzed, the query's vector made before:")
        for r, ts in analyzed.items():
            show(r, ts)
        print(f"REMEMBER ({len(remember)} turns, one at a time, each embedded): median "
              f"{statistics.median(remember) * 1000:.2f} ms, "
              f"p95 {remember[int(len(remember) * 0.95)] * 1000:.2f} ms")


if __name__ == "__main__":
    main()
