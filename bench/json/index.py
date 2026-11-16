#!/usr/bin/env python3
"""Indexes on JSON, nucleus against PostgreSQL's (PLAN.md §M14.15 J4c).

    python3 bench/json/index.py --nucleus target/release/nucleus [--rows 200000] [--no-pg]

The same documents in both — events made up the same way each run:

    {"id": i, "user": {"name": "u<i % 5000>", "tier": "free|pro|team"},
     "kind": "k<i % 20>", "tags": ["t<i % 50>", "t<i * 7 % 50>"],
     "score": i % 1000[, "rare": true where i % 1000 = 0]}

each query timed (the median of five runs, the client's clock) without
an index, then with nucleus's and PostgreSQL's:

- id: `count(*) … WHERE doc->>'id' = '<n>'` — one row; a B-tree
  expression index on `(doc->>'id')` in both;
- score: `count(*) … WHERE (doc->>'score')::int BETWEEN 10 AND 12` — 0.3 %
  of the rows; a B-tree expression index on `((doc->>'score')::int)`;
- user: `count(*) … WHERE doc @> '{"user": {"name": "u42"}}'` — 0.02 %;
  nucleus's `USING json`, PostgreSQL's `USING gin (doc jsonb_path_ops)`;
- kind+tag: `doc @> '{"kind": "k3", "tags": ["t3"]}'` — 1 %;
- rare: `count(*) … WHERE doc ? 'rare'` — 0.1 %; `jsonb_path_ops`
  cannot serve `?` (PostgreSQL scans), nucleus's index can.

Also each index's build, and each count asserted equal to PostgreSQL's
(and, in nucleus, to the count without an index).  PostgreSQL runs
native on this machine (bench/pg.py's cluster, tuned for it); nucleus as
a server of the given binary.  One at a time.  Every time is printed as
an `@metric` line too, for `bench/method.py run`, which repeats the
whole script (PLAN.md §M14.31).
"""

import argparse
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

CLUSTER = None  # the PostgreSQL cluster, while main runs

QUERIES = [
    ("id", "doc->>'id' = '123457'"),
    ("score", "(doc->>'score')::int BETWEEN 10 AND 12"),
    ("user", """doc @> '{"user": {"name": "u42"}}'"""),
    ("kind+tag", """doc @> '{"kind": "k3", "tags": ["t3"]}'"""),
    ("rare", "doc ? 'rare'"),
]
INDEXES = {
    "nucleus": [
        ("id", "CREATE INDEX j_id ON j ((doc->>'id'))"),
        ("score", "CREATE INDEX j_score ON j (((doc->>'score')::int))"),
        ("containment", "CREATE INDEX j_c ON j USING json (doc)"),
    ],
    "pg": [
        ("id", "CREATE INDEX j_id ON j ((doc->>'id'))"),
        ("score", "CREATE INDEX j_score ON j (((doc->>'score')::int))"),
        ("containment", "CREATE INDEX j_c ON j USING gin (doc jsonb_path_ops)"),
    ],
}


def documents(n):
    tiers = ["free", "pro", "team"]
    for i in range(n):
        rare = ', "rare": true' if i % 1000 == 0 else ""
        yield (
            f'{{"id": {i}, "user": {{"name": "u{i % 5000}", "tier": "{tiers[i % 3]}"}}, '
            f'"kind": "k{i % 20}", "tags": ["t{i % 50}", "t{i * 7 % 50}"], '
            f'"score": {i % 1000}{rare}}}'
        )


def times(f, n=5):
    """n timed runs of f() after one untimed, in seconds."""
    f()
    t = []
    for _ in range(n):
        s = time.perf_counter()
        f()
        t.append(time.perf_counter() - s)
    return t


def psql(sql):
    return CLUSTER.sql(file=sql + "\n").strip()


def pg_times(sql, n=5, warm=True):
    """n runs in one session, after one untimed (with `warm`), by psql's
    clock."""
    first = 1 if warm else 0
    out = CLUSTER.sql(file="\\timing on\n" + (sql + "\n") * (n + first))
    ms = [float(line.split()[1]) for line in out.splitlines() if line.startswith("Time:")]
    return [m / 1000 for m in ms[first:]]


def main():
    a = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    a.add_argument("--nucleus", required=True)
    a.add_argument("--rows", type=int, default=200_000)
    a.add_argument("--no-pg", action="store_true", help="nucleus alone")
    args = a.parse_args()
    docs = list(documents(args.rows))
    use_pg = not args.no_pg
    global CLUSTER

    if use_pg:
        CLUSTER = pg.Cluster(pg.DEFAULT).__enter__()
        method.emit_system("postgresql", CLUSTER.describe())
        psql("DROP TABLE IF EXISTS j;")
    work = tempfile.mkdtemp(prefix="nucleus-json-index-")
    server = subprocess.Popen([args.nucleus, "server", os.path.join(work, "db"), "--listen",
                               "127.0.0.1:20236"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(60):
            try:
                db = nucleus.connect("nucleus://nucleus:nucleus@127.0.0.1:20236/nucleus")
                break
            except Exception:
                time.sleep(0.5)
        # Loaded.
        db.execute("CREATE TABLE j (id int PRIMARY KEY, doc json)")
        for k in range(0, len(docs), 5000):
            db.execute_many("INSERT INTO j VALUES ($1, $2::json)",
                            [(k + i, t) for i, t in enumerate(docs[k:k + 5000])])
        db.execute("ANALYZE j")
        if use_pg:
            psql("CREATE TABLE j (id int PRIMARY KEY, doc jsonb);")
            copy = "".join(f"{i}\t{t}\n" for i, t in enumerate(docs))
            CLUSTER.sql(file="COPY j FROM STDIN;\n" + copy + "\\.\n")
            psql("VACUUM ANALYZE j;")
        count = {name: f"SELECT count(*) FROM j WHERE {cond}" for name, cond in QUERIES}
        # Without an index.
        scan_n = {name: times(lambda q=q: db.execute(q)) for name, q in count.items()}
        expect = {name: db.execute(q).rows[0][0] for name, q in count.items()}
        scan_pg = {name: pg_times(q + ";") for name, q in count.items()} if use_pg else {}
        if use_pg:
            for name, q in count.items():
                assert str(expect[name]) == psql(q + ";"), f"the same count: {name}"
        # The indexes.
        build_n, build_pg = {}, {}
        for name, sql in INDEXES["nucleus"]:
            s = time.perf_counter()
            db.execute(sql)
            build_n[name] = time.perf_counter() - s
        db.execute("ANALYZE j")
        if use_pg:
            for name, sql in INDEXES["pg"]:
                build_pg[name] = pg_times(sql + ";", n=0, warm=False)[0]
            psql("ANALYZE j;")
        ix_n, ix_pg = {}, {}
        for name, q in count.items():
            got = db.execute(q).rows[0][0]
            assert got == expect[name], f"{name}: {got} with the index, {expect[name]} without"
            ix_n[name] = times(lambda q=q: db.execute(q))
            if use_pg:
                ix_pg[name] = pg_times(q + ";")
        for name, _ in QUERIES:
            method.emit(f"{name}, no index", scan_n[name], unit="s")
            method.emit(f"{name}, indexed", ix_n[name], unit="s")
            if use_pg:
                method.emit(f"{name}, no index", scan_pg[name], unit="s", system="postgresql")
                method.emit(f"{name}, indexed", ix_pg[name], unit="s", system="postgresql")
        for name, _ in INDEXES["nucleus"]:
            method.emit(f"build {name} index", build_n[name], unit="s", phase="write")
            if use_pg:
                method.emit(f"build {name} index", build_pg[name], unit="s", system="postgresql", phase="write")
        med = method.median
        print(f"rows {args.rows}")
        print(f"{'query':<10} {'rows':>7} | {'scan s':>8} {'pg':>8} | {'index ms':>9} {'pg':>9}")
        for name, _ in QUERIES:
            pg_scan = f"{med(scan_pg[name]):>8.3f}" if use_pg else f"{'-':>8}"
            pg_ix = f"{med(ix_pg[name]) * 1000:>9.2f}" if use_pg else f"{'-':>9}"
            print(f"{name:<10} {expect[name]:>7} | {med(scan_n[name]):>8.3f} {pg_scan} | "
                  f"{med(ix_n[name]) * 1000:>9.2f} {pg_ix}")
        print()
        print(f"{'index':<12} {'build s':>8} {'pg':>8}")
        for name, _ in INDEXES["nucleus"]:
            pg_b = f"{build_pg[name]:>8.2f}" if use_pg else f"{'-':>8}"
            print(f"{name:<12} {build_n[name]:>8.2f} {pg_b}")
    finally:
        server.terminate()
        server.wait()
        if CLUSTER:
            psql("DROP TABLE IF EXISTS j;")
            CLUSTER.__exit__(None, None, None)
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
