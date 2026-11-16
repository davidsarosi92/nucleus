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
- table: `count(*), sum(<a column>)` over `j, JSON_TABLE(j.doc, <rows>
  COLUMNS (…))` — SQL/JSON's table, lateral to each document (a tweet's
  hashtags' lengths, a performance's seat areas' ids, a feature's points
  west of 70°W);
- size: each whole file as one value, `pg_column_size` against nucleus's
  binary form (from `bench/json`'s binary: the `form KB` column).

PostgreSQL runs native on this machine (bench/pg.py's cluster, tuned for
it); nucleus as a server of the given binary.  One at a time.  Every
time is printed as an `@metric` line too, for `bench/method.py run`,
which repeats the whole script (PLAN.md §M14.31).
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
sys.path.insert(0, os.path.join(ROOT, "drivers", "python"))
sys.path.insert(0, os.path.join(ROOT, "bench"))
import method  # noqa: E402
import nucleus  # noqa: E402
import pg  # noqa: E402

DATA = os.path.join(ROOT, "target", "bench-data", "json")
CLUSTER = None  # the PostgreSQL cluster, while main runs


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


def times(f, n=5):
    """n timed runs of f() after one untimed, in seconds."""
    f()
    t = []
    for _ in range(n):
        s = time.perf_counter()
        f()
        t.append(time.perf_counter() - s)
    return t


def psql(sql, *, timing=False):
    script = ("\\timing on\n" if timing else "") + sql + "\n"
    out = CLUSTER.sql(file=script)
    if timing:
        ms = [float(line.split()[1]) for line in out.splitlines() if line.startswith("Time:")]
        return ms[-1] / 1000
    return out.strip()


def copy_in(table, lines):
    CLUSTER.sql(file=f"COPY {table} FROM STDIN;\n" + lines + "\\.\n")


def pg_times(sql, n=5):
    """n runs in one session, after one untimed, by psql's clock."""
    out = CLUSTER.sql(file="\\timing on\n" + (sql + "\n") * (n + 1))
    ms = [float(line.split()[1]) for line in out.splitlines() if line.startswith("Time:")]
    return [m / 1000 for m in ms[1:]]


def main():
    a = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    a.add_argument("--nucleus", required=True)
    a.add_argument("--rows", type=int, default=50_000)
    args = a.parse_args()
    docs = documents()
    global CLUSTER

    CLUSTER = pg.Cluster(pg.DEFAULT).__enter__()
    method.emit_system("postgresql", CLUSTER.describe())
    work = tempfile.mkdtemp(prefix="nucleus-json-bench-")
    server = subprocess.Popen([args.nucleus, "server", os.path.join(work, "db"), "--listen",
                               "127.0.0.1:20235"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(60):
            try:
                db = nucleus.connect("nucleus://nucleus:nucleus@127.0.0.1:20235/nucleus")
                break
            except Exception:
                time.sleep(0.5)
        print(f"{'documents':<14} {'rows':>7} {'MB':>6} | {'parse s':>8} {'pg':>8} | "
              f"{'output s':>8} {'pg':>8} | {'field s':>8} {'pg':>8} | {'path s':>8} {'pg':>8} | "
              f"{'value s':>8} {'pg':>8} | {'table s':>8} {'pg':>8}")
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
        tables = {
            "tweets": "'$.entities.hashtags[*]' COLUMNS (t text PATH '$.text')) t",
            "performances": "'$.seatCategories[*].areas[*]' COLUMNS (t bigint PATH '$.areaId')) t",
            "features": "'$.geometry.coordinates[*][*] ? (@[0] < -70)' COLUMNS (t int PATH '$[0].floor()')) t",
        }
        table_sum = {"tweets": "sum(length(t.t))", "performances": "sum(t.t)", "features": "sum(t.t)"}
        for name, texts in docs.items():
            rows = [texts[i % len(texts)] for i in range(args.rows)] if name != "features" else texts * 4
            mb = sum(len(t.encode()) for t in rows) / 1e6
            # PostgreSQL: COPY the texts in.
            psql(f"DROP TABLE IF EXISTS src; CREATE TABLE src (id int, doc text);")
            copy = "".join(f"{i}\t{t.replace(chr(92), chr(92) * 2)}\n" for i, t in enumerate(rows))
            copy_in("src", copy)
            psql("DROP TABLE IF EXISTS j; CREATE TABLE j AS SELECT id, doc::jsonb AS doc FROM src; "
                 "VACUUM ANALYZE src; VACUUM ANALYZE j;")
            pg_parse = pg_times("SELECT count(*) FROM src WHERE doc::jsonb IS NOT NULL;")
            pg_out = pg_times("SELECT sum(length(doc::text)) FROM j;")
            pg_field = pg_times(f"SELECT sum(length({fields[name]})) FROM j;")
            path = f"SELECT count(*) FROM j WHERE doc @? '{paths[name]}'"
            pg_path = pg_times(path + ";")
            value = f"SELECT sum(JSON_VALUE(doc, '{values[name]}' RETURNING bigint)) FROM j"
            pg_value = pg_times(value + ";")
            table = f"SELECT count(*), {table_sum[name]} FROM j, JSON_TABLE(j.doc, {tables[name]}"
            pg_table = pg_times(table + ";")
            # nucleus: the same rows.
            db.execute("DROP TABLE IF EXISTS src")
            db.execute("DROP TABLE IF EXISTS j")
            db.execute("CREATE TABLE src (id int PRIMARY KEY, doc text)")
            for k in range(0, len(rows), 5000):
                db.execute_many("INSERT INTO src VALUES ($1, $2)",
                                [(k + i, t) for i, t in enumerate(rows[k:k + 5000])])
            db.execute("CREATE TABLE j AS SELECT id, doc::json AS doc FROM src")
            n_parse = times(lambda: db.execute("SELECT count(*) FROM src WHERE doc::json IS NOT NULL"))
            n_out = times(lambda: db.execute("SELECT sum(length(doc::text)) FROM j"))
            field = f"SELECT sum(length({fields[name]})) FROM j"
            assert str(db.execute(field).rows[0][0]) == psql(field + ";"), "the same field sums"
            n_field = times(lambda: db.execute(field))
            assert str(db.execute(path).rows[0][0]) == psql(path + ";"), "the same path counts"
            n_path = times(lambda: db.execute(path))
            assert str(db.execute(value).rows[0][0]) == psql(value + ";"), "the same value sums"
            n_value = times(lambda: db.execute(value))
            got = "|".join(str(x) for x in db.execute(table).rows[0])
            assert got == psql(table + ";"), f"the same table sums: {got}"
            n_table = times(lambda: db.execute(table))
            pairs = {"parse": (n_parse, pg_parse), "output": (n_out, pg_out), "field": (n_field, pg_field),
                     "path": (n_path, pg_path), "value": (n_value, pg_value), "table": (n_table, pg_table)}
            for op, (n_t, pg_t) in pairs.items():
                method.emit(f"{op} ({name})", n_t, unit="s")
                method.emit(f"{op} ({name})", pg_t, unit="s", system="postgresql")
            cells = " | ".join(f"{method.median(n_t):>8.3f} {method.median(pg_t):>8.3f}"
                               for n_t, pg_t in pairs.values())
            print(f"{name:<14} {len(rows):>7} {mb:>6.1f} | {cells}", flush=True)
        print()
        print(f"{'file':<20} {'text KB':>8} {'jsonb KB':>9}")
        for f in ("canada.json", "citm_catalog.json", "twitter.json"):
            text = open(os.path.join(DATA, f), encoding="utf-8").read()
            psql("DROP TABLE IF EXISTS one; CREATE TABLE one (doc text);")
            copy_in("one", text.replace("\\", "\\\\").replace("\n", " ") + "\n")
            size = int(psql("SELECT pg_column_size(doc::jsonb) FROM one;"))
            print(f"{f:<20} {len(text.encode()) / 1024:>8.0f} {size / 1024:>9.0f}")
            method.emit(f"size ({f})", size / 1024, unit="KB", system="postgresql")
    finally:
        server.terminate()
        server.wait()
        psql("DROP TABLE IF EXISTS src; DROP TABLE IF EXISTS j; DROP TABLE IF EXISTS one;")
        CLUSTER.__exit__(None, None, None)
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
