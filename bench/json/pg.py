#!/usr/bin/env python3
"""JSON in SQL, nucleus against PostgreSQL's jsonb (PLAN.md §M14.15).

    python3 bench/json/pg.py --nucleus target/release/nucleus [--rows 50000]

The same documents — the tweets of `twitter.json`, the performances of
`citm_catalog.json`, the features of `canada.json` (`bench/json/run.sh`
fetches them into target/bench-data/json) — stored as text in both, then
timed (the median of five runs, the client's clock):

- parse: `count(*) … WHERE doc::json IS NOT NULL` over the text column;
- output: `sum(length(doc::text))` over a json column;
- field: `sum(length(<a field>))` over it — a nested field by `->` and
  `->>` (a tweet's user's screen name, a performance's start, a
  feature's name);
- path: `count(*) … WHERE doc @? <path>` — an SQL/JSON path with a
  filter (a tweet's hashtags starting with "a", a performance's seat
  areas above an id, a feature's points west of 70°W);
- value: `sum(JSON_VALUE(doc, <path> RETURNING bigint))` — SQL/JSON's
  scalar, converted (a tweet's user's followers, a performance's start, a
  feature's number of rings);
- size: each whole file as one value, `pg_column_size` against nucleus's
  binary form (from `bench/json`'s binary: the `form KB` column).

PostgreSQL runs in Docker (postgres:18-alpine, its defaults); nucleus as a
server of the given binary.  Both on this machine, one at a time.
"""

import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
sys.path.insert(0, os.path.join(ROOT, "drivers", "python"))
import nucleus  # noqa: E402

DATA = os.path.join(ROOT, "target", "bench-data", "json")
PG = "nucleus-json-bench-pg"


def documents():
    """name -> list of JSON texts."""
    twitter = json.load(open(os.path.join(DATA, "twitter.json")))["statuses"]
    citm = json.load(open(os.path.join(DATA, "citm_catalog.json")))["performances"]
    canada = json.load(open(os.path.join(DATA, "canada.json")))["features"]
    return {
        "tweets": [json.dumps(d, ensure_ascii=False) for d in twitter],
        "performances": [json.dumps(d) for d in citm],
        "features": [json.dumps(d) for d in canada],
    }


def median(f, n=5):
    f()
    t = []
    for _ in range(n):
        s = time.perf_counter()
        f()
        t.append(time.perf_counter() - s)
    return statistics.median(t)


def psql(sql, *, timing=False):
    cmd = ["docker", "exec", "-i", PG, "psql", "-U", "postgres", "-qAt", "-v", "ON_ERROR_STOP=1"]
    script = ("\\timing on\n" if timing else "") + sql + "\n"
    out = subprocess.run(cmd, input=script, capture_output=True, text=True, check=True).stdout
    if timing:
        ms = [float(line.split()[1]) for line in out.splitlines() if line.startswith("Time:")]
        return ms[-1] / 1000
    return out.strip()


def pg_time(sql, n=5):
    psql(sql, timing=True)
    return statistics.median(psql(sql, timing=True) for _ in range(n))


def main():
    a = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    a.add_argument("--nucleus", required=True)
    a.add_argument("--rows", type=int, default=50_000)
    args = a.parse_args()
    docs = documents()

    subprocess.run(["docker", "rm", "-f", PG], capture_output=True)
    subprocess.run(["docker", "run", "-d", "--name", PG, "-e", "POSTGRES_HOST_AUTH_METHOD=trust",
                    "postgres:18-alpine"], check=True, capture_output=True)
    work = tempfile.mkdtemp(prefix="nucleus-json-bench-")
    server = subprocess.Popen([args.nucleus, "server", os.path.join(work, "db"), "--listen",
                               "127.0.0.1:20235"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(60):
            try:
                psql("SELECT 1")
                db = nucleus.connect("nucleus://nucleus:nucleus@127.0.0.1:20235/nucleus")
                break
            except Exception:
                time.sleep(0.5)
        print(f"{'documents':<14} {'rows':>7} {'MB':>6} | {'parse s':>8} {'pg':>8} | "
              f"{'output s':>8} {'pg':>8} | {'field s':>8} {'pg':>8} | {'path s':>8} {'pg':>8} | "
              f"{'value s':>8} {'pg':>8}")
        fields = {
            "tweets": "doc -> 'user' ->> 'screen_name'",
            "performances": "doc ->> 'start'",
            "features": "doc -> 'properties' ->> 'name'",
        }
        paths = {
            "tweets": '$.entities.hashtags[*] ? (@.text starts with "a")',
            "performances": "$.seatCategories[*].areas[*] ? (@.areaId > 205705999)",
            "features": "$.geometry.coordinates[*][*] ? (@[0] < -70)",
        }
        values = {
            "tweets": "$.user.followers_count",
            "performances": "$.start",
            "features": "$.geometry.coordinates.size()",
        }
        for name, texts in docs.items():
            rows = [texts[i % len(texts)] for i in range(args.rows)] if name != "features" else texts * 4
            mb = sum(len(t.encode()) for t in rows) / 1e6
            # PostgreSQL: COPY the texts in.
            psql(f"DROP TABLE IF EXISTS src; CREATE TABLE src (id int, doc text);")
            copy = "".join(f"{i}\t{t.replace(chr(92), chr(92) * 2)}\n" for i, t in enumerate(rows))
            subprocess.run(["docker", "exec", "-i", PG, "psql", "-U", "postgres", "-q", "-c",
                            "COPY src FROM STDIN"], input=copy, text=True, check=True)
            psql("DROP TABLE IF EXISTS j; CREATE TABLE j AS SELECT id, doc::jsonb AS doc FROM src; "
                 "VACUUM ANALYZE src; VACUUM ANALYZE j;")
            pg_parse = pg_time("SELECT count(*) FROM src WHERE doc::jsonb IS NOT NULL;")
            pg_out = pg_time("SELECT sum(length(doc::text)) FROM j;")
            pg_field = pg_time(f"SELECT sum(length({fields[name]})) FROM j;")
            path = f"SELECT count(*) FROM j WHERE doc @? '{paths[name]}'"
            pg_path = pg_time(path + ";")
            value = f"SELECT sum(JSON_VALUE(doc, '{values[name]}' RETURNING bigint)) FROM j"
            pg_value = pg_time(value + ";")
            # nucleus: the same rows.
            db.execute("DROP TABLE IF EXISTS src")
            db.execute("DROP TABLE IF EXISTS j")
            db.execute("CREATE TABLE src (id int PRIMARY KEY, doc text)")
            for k in range(0, len(rows), 5000):
                db.execute_many("INSERT INTO src VALUES ($1, $2)",
                                [(k + i, t) for i, t in enumerate(rows[k:k + 5000])])
            db.execute("CREATE TABLE j AS SELECT id, doc::json AS doc FROM src")
            n_parse = median(lambda: db.execute("SELECT count(*) FROM src WHERE doc::json IS NOT NULL"))
            n_out = median(lambda: db.execute("SELECT sum(length(doc::text)) FROM j"))
            field = f"SELECT sum(length({fields[name]})) FROM j"
            assert str(db.execute(field).rows[0][0]) == psql(field + ";"), "the same field sums"
            n_field = median(lambda: db.execute(field))
            assert str(db.execute(path).rows[0][0]) == psql(path + ";"), "the same path counts"
            n_path = median(lambda: db.execute(path))
            assert str(db.execute(value).rows[0][0]) == psql(value + ";"), "the same value sums"
            n_value = median(lambda: db.execute(value))
            print(f"{name:<14} {len(rows):>7} {mb:>6.1f} | {n_parse:>8.3f} {pg_parse:>8.3f} | "
                  f"{n_out:>8.3f} {pg_out:>8.3f} | {n_field:>8.3f} {pg_field:>8.3f} | "
                  f"{n_path:>8.3f} {pg_path:>8.3f} | {n_value:>8.3f} {pg_value:>8.3f}", flush=True)
        print()
        print(f"{'file':<20} {'text KB':>8} {'jsonb KB':>9}")
        for f in ("canada.json", "citm_catalog.json", "twitter.json"):
            text = open(os.path.join(DATA, f), encoding="utf-8").read()
            psql("DROP TABLE IF EXISTS one; CREATE TABLE one (doc text);")
            subprocess.run(["docker", "exec", "-i", PG, "psql", "-U", "postgres", "-q", "-c",
                            "COPY one FROM STDIN"], input=text.replace("\\", "\\\\").replace("\n", " ") + "\n",
                           text=True, check=True)
            size = int(psql("SELECT pg_column_size(doc::jsonb) FROM one;"))
            print(f"{f:<20} {len(text.encode()) / 1024:>8.0f} {size / 1024:>9.0f}")
    finally:
        server.terminate()
        server.wait()
        subprocess.run(["docker", "rm", "-f", PG], capture_output=True)
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
