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
(and, in nucleus, to the count without an index).  PostgreSQL runs in
Docker (postgres:18-alpine, its defaults); nucleus as a server of the
given binary.  Both on this machine, one at a time.
"""

import argparse
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

PG = "nucleus-json-index-pg"

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
    a.add_argument("--rows", type=int, default=200_000)
    a.add_argument("--no-pg", action="store_true", help="nucleus alone")
    args = a.parse_args()
    docs = list(documents(args.rows))
    pg = not args.no_pg

    if pg:
        subprocess.run(["docker", "rm", "-f", PG], capture_output=True)
        subprocess.run(["docker", "run", "-d", "--name", PG, "-e", "POSTGRES_HOST_AUTH_METHOD=trust",
                        "postgres:18-alpine"], check=True, capture_output=True)
    work = tempfile.mkdtemp(prefix="nucleus-json-index-")
    server = subprocess.Popen([args.nucleus, "server", os.path.join(work, "db"), "--listen",
                               "127.0.0.1:20236"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(60):
            try:
                if pg:
                    psql("SELECT 1")
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
        if pg:
            psql("CREATE TABLE j (id int PRIMARY KEY, doc jsonb);")
            copy = "".join(f"{i}\t{t}\n" for i, t in enumerate(docs))
            subprocess.run(["docker", "exec", "-i", PG, "psql", "-U", "postgres", "-q", "-c",
                            "COPY j FROM STDIN"], input=copy, text=True, check=True)
            psql("VACUUM ANALYZE j;")
        count = {name: f"SELECT count(*) FROM j WHERE {cond}" for name, cond in QUERIES}
        # Without an index.
        scan_n = {name: median(lambda q=q: db.execute(q)) for name, q in count.items()}
        expect = {name: db.execute(q).rows[0][0] for name, q in count.items()}
        scan_pg = {name: pg_time(q + ";") for name, q in count.items()} if pg else {}
        if pg:
            for name, q in count.items():
                assert str(expect[name]) == psql(q + ";"), f"the same count: {name}"
        # The indexes.
        build_n, build_pg = {}, {}
        for name, sql in INDEXES["nucleus"]:
            s = time.perf_counter()
            db.execute(sql)
            build_n[name] = time.perf_counter() - s
        db.execute("ANALYZE j")
        if pg:
            for name, sql in INDEXES["pg"]:
                s = time.perf_counter()
                psql(sql + ";")
                build_pg[name] = time.perf_counter() - s
            psql("ANALYZE j;")
        ix_n, ix_pg = {}, {}
        for name, q in count.items():
            got = db.execute(q).rows[0][0]
            assert got == expect[name], f"{name}: {got} with the index, {expect[name]} without"
            ix_n[name] = median(lambda q=q: db.execute(q))
            if pg:
                ix_pg[name] = pg_time(q + ";")
        print(f"rows {args.rows}")
        print(f"{'query':<10} {'rows':>7} | {'scan s':>8} {'pg':>8} | {'index ms':>9} {'pg':>9}")
        for name, _ in QUERIES:
            pg_scan = f"{scan_pg[name]:>8.3f}" if pg else f"{'-':>8}"
            pg_ix = f"{ix_pg[name] * 1000:>9.2f}" if pg else f"{'-':>9}"
            print(f"{name:<10} {expect[name]:>7} | {scan_n[name]:>8.3f} {pg_scan} | "
                  f"{ix_n[name] * 1000:>9.2f} {pg_ix}")
        print()
        print(f"{'index':<12} {'build s':>8} {'pg':>8}")
        for name, _ in INDEXES["nucleus"]:
            pg_b = f"{build_pg[name]:>8.2f}" if pg else f"{'-':>8}"
            print(f"{name:<12} {build_n[name]:>8.2f} {pg_b}")
    finally:
        server.terminate()
        server.wait()
        if pg:
            subprocess.run(["docker", "rm", "-f", PG], capture_output=True)
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
