#!/usr/bin/env python3
"""PostgreSQL's text search through pg-compat (PLAN.md §M14.21 T8): what
PostgreSQL clients write for a GIN index, on BEIR's corpora, against
PostgreSQL with that GIN index.

    bench/text/tsearch.py nucleus BINARY [--sets scifact,nfcorpus,fiqa] [--values] [--rank]
    bench/text/tsearch.py postgres [--container nucleus-paradedb] [--sets …] [--rank]

Each BEIR query (its words, lower case) counts the documents it matches,
all its words (`plainto_tsquery`) or any (`websearch_to_tsquery` of the
words joined by `or`):

    SELECT count(*) FROM docs WHERE to_tsvector('english', body) @@ plainto_tsquery('english', q)

nucleus: through PostgreSQL's protocol (psycopg, the query a literal, so
`@@` rewrites itself to the text index's `body @@ q` by `english`); with
`--values`, also over a stored `tsvector` column without an index — the
values computed as PostgreSQL computes them.  PostgreSQL: `CREATE INDEX
… USING gin (to_tsvector('english', body))`, timed by `psql`'s `\\timing`
(the server's time).  The mean and median ms of a query, the second of
two rounds, and the documents matched over all queries (the two systems'
English analyses differ a little: PostgreSQL's parser is its own).

`--rank` (T8b) adds what a PostgreSQL search page asks: the ten best
matches by `ts_rank` (`rank`), and with each one's `ts_headline`
(`headline`), their nDCG@10 in place of the count:

    SELECT id[, length(ts_headline('english', body, q))] FROM docs
    WHERE to_tsvector('english', body) @@ q
    ORDER BY ts_rank(to_tsvector('english', body), q) DESC, id LIMIT 10
"""

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..",
                                "drivers", "python"))
import beir  # noqa: E402

SHAPES = {
    "all": "plainto_tsquery('english', {q})",
    "any": "websearch_to_tsquery('english', {q})",
}


RANKED = {
    "rank": "SELECT id FROM docs WHERE {tv} @@ {m} ORDER BY ts_rank({tv}, {m}) DESC, id LIMIT 10",
    "headline": "SELECT id, length(ts_headline('english', body, {m})) FROM docs WHERE {tv} @@ {m} "
                "ORDER BY ts_rank({tv}, {m}) DESC, id LIMIT 10",
}
TV = "to_tsvector('english', body)"


def words(q):
    return beir.SYNTAX.sub(" ", q).lower().split()


def query_text(shape, q):
    ws = words(q)
    return beir.lit(" ".join(ws) if shape == "all" else " or ".join(ws))


def report(name, mode, times, found):
    times.sort()
    found = f"{found:10.4f}" if isinstance(found, float) else f"{found:10}"
    print(f"{name:10} {mode:12} {sum(times) / len(times):9.2f} {times[len(times) // 2]:9.2f} "
          f"{found}", flush=True)


def ranked_modes(a):
    return [(f"{shape} {mode}", shape, sql) for shape in SHAPES for mode, sql in RANKED.items()
            if a.rank]


def postgres(a):
    def psql(sql=None, stdin=None):
        cmd = ["docker", "exec", "-i", a.container, "psql", "-U", "postgres", "-d", "postgres",
               "-At", "-v", "ON_ERROR_STOP=1"]
        if sql:
            cmd += ["-c", sql]
        return subprocess.run(cmd, input=stdin, capture_output=True, text=True,
                              check=True).stdout

    print("postgres, GIN on to_tsvector('english', body)")
    print(f"{'set':10} {'mode':12} {'mean ms':>9} {'p50 ms':>9} {'found':>10}")
    for name in a.sets.split(","):
        d = os.path.join(a.data, f"beir-{name}", name)
        docs = list(beir.corpus(d))
        queries, qrels = beir.load(d)
        psql("DROP TABLE IF EXISTS docs")
        psql("CREATE TABLE docs (id text PRIMARY KEY, body text)")
        with tempfile.NamedTemporaryFile("w", suffix=".tsv", encoding="utf-8") as f:
            for i, b in docs:
                f.write(i.replace("\\", "\\\\").replace("\t", " ") + "\t"
                        + b.replace("\\", "\\\\").replace("\t", "\\t")
                        .replace("\n", "\\n").replace("\r", "\\r") + "\n")
            f.flush()
            os.chmod(f.name, 0o644)
            subprocess.run(["docker", "cp", f.name, f"{a.container}:/tmp/docs.tsv"], check=True)
        psql("COPY docs FROM '/tmp/docs.tsv'")
        subprocess.run(["docker", "exec", a.container, "rm", "/tmp/docs.tsv"], check=True)
        t = time.perf_counter()
        psql("CREATE INDEX docs_gin ON docs USING gin (to_tsvector('english', body))")
        print(f"{name:10} GIN built in {(time.perf_counter() - t) * 1000:.0f} ms", flush=True)
        psql("ANALYZE docs")
        for shape, maker in SHAPES.items():
            script = ["SET client_min_messages = warning;", "\\timing on"]
            for qid in qrels:
                script.append("\\echo @@")
                script.append("SELECT count(*) FROM docs WHERE to_tsvector('english', body) @@ "
                              + maker.format(q=query_text(shape, queries[qid])) + ";")
            body = script[2:]
            out = psql(stdin="\n".join(script + body) + "\n")
            runs = out.split("@@\n")[1:][len(qrels):]
            times, found = [], 0
            for run in runs:
                lines = run.strip().split("\n")
                found += int(lines[0])
                times.append(float(lines[-1].split("Time: ")[1].split(" ms")[0]))
            report(name, shape, times, found)
        for label, shape, sql in ranked_modes(a):
            script = ["SET client_min_messages = warning;", "\\timing on"]
            for qid in qrels:
                m = SHAPES[shape].format(q=query_text(shape, queries[qid]))
                script.append("\\echo @@")
                script.append(sql.format(tv=TV, m=m) + ";")
            body = script[2:]
            out = psql(stdin="\n".join(script + body) + "\n")
            runs = out.split("@@\n")[1:][len(qrels):]
            times, scores = [], []
            for qid, run in zip(qrels, runs):
                lines = run.strip().split("\n")
                ids = [l.split("|")[0] for l in lines[:-1]]
                scores.append(beir.ndcg10(ids, qrels[qid]))
                times.append(float(lines[-1].split("Time: ")[1].split(" ms")[0]))
            report(name, label, times, sum(scores) / len(scores))
        psql("DROP TABLE docs")


def nucleus_side(a):
    import psycopg
    import nucleus

    work = tempfile.mkdtemp(prefix="nucleus-tsearch-")
    host = a.host
    fmt = (lambda p: f"[{host}]:{p}") if ":" in host else (lambda p: f"{host}:{p}")
    server = subprocess.Popen([a.binary, "server", os.path.join(work, "db"),
                               "--listen", fmt(a.port), "--pg-compat", fmt(a.port + 1),
                               "--no-auth"],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(600):
            try:
                c = nucleus.connect(host, a.port, password=None)
                break
            except (OSError, nucleus.OperationalError):
                time.sleep(0.1)
        pg = psycopg.connect(host=host, port=a.port + 1, user="nucleus", dbname="nucleus",
                             autocommit=True)
        print("nucleus through pg-compat, the text index (english)"
              + (", and stored tsvectors" if a.values else ""))
        print(f"{'set':10} {'mode':12} {'mean ms':>9} {'p50 ms':>9} {'found':>10}")
        for name in a.sets.split(","):
            d = os.path.join(a.data, f"beir-{name}", name)
            docs = list(beir.corpus(d))
            queries, qrels = beir.load(d)
            c.execute("DROP TABLE IF EXISTS docs")
            c.execute("CREATE TABLE docs (id TEXT PRIMARY KEY, body TEXT)")
            c.load("docs", {"id": [i for i, _ in docs], "body": [b for _, b in docs]})
            t = time.perf_counter()
            c.execute("CREATE INDEX docs_body ON docs USING text (body) "
                      "WITH (analyzer = 'english')")
            print(f"{name:10} text index built in {(time.perf_counter() - t) * 1000:.0f} ms",
                  flush=True)
            modes = [("index", "to_tsvector('english', body)")]
            if a.values:
                pg.execute("ALTER TABLE docs ADD COLUMN tv tsvector")
                pg.execute("UPDATE docs SET tv = to_tsvector('english', body)")
                modes.append(("values", "tv"))
            c.execute("ANALYZE docs")
            for shape, maker in SHAPES.items():
                for mode, lhs in modes:
                    for _ in range(2):
                        times, found = [], 0
                        for qid in qrels:
                            sql = (f"SELECT count(*) FROM docs WHERE {lhs} @@ "
                                   + maker.format(q=query_text(shape, queries[qid])))
                            t = time.perf_counter()
                            found += pg.execute(sql).fetchone()[0]
                            times.append((time.perf_counter() - t) * 1000)
                    report(name, f"{shape} {mode}", times, found)
            for label, shape, sql in ranked_modes(a):
                for _ in range(2):
                    times, scores = [], []
                    for qid in qrels:
                        m = SHAPES[shape].format(q=query_text(shape, queries[qid]))
                        q = sql.format(tv=TV, m=m)
                        t = time.perf_counter()
                        ids = [r[0] for r in pg.execute(q).fetchall()]
                        times.append((time.perf_counter() - t) * 1000)
                        scores.append(beir.ndcg10(ids, qrels[qid]))
                report(name, label, times, sum(scores) / len(scores))
        pg.close()
        c.close()
    finally:
        server.terminate()
        server.wait()
        shutil.rmtree(work)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("system", choices=["nucleus", "postgres"])
    ap.add_argument("binary", nargs="?")
    ap.add_argument("--container", default="nucleus-paradedb")
    ap.add_argument("--sets", default="scifact,nfcorpus,fiqa")
    ap.add_argument("--data", default="target/text-data")
    ap.add_argument("--values", action="store_true")
    ap.add_argument("--rank", action="store_true")
    ap.add_argument("--port", type=int, default=21028)
    ap.add_argument("--host", default=os.environ.get("NUCLEUS_BENCH_HOST", "127.0.0.1"))
    a = ap.parse_args()
    return postgres(a) if a.system == "postgres" else nucleus_side(a)


if __name__ == "__main__":
    main()
