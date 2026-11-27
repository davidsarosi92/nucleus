#!/usr/bin/env python3
"""Multi-valued indexes measured against PostgreSQL's GIN and MongoDB's
multikey indexes (PLAN.md §M15 D5).

    python3 bench/multikey.py --nucleus target/release/nucleus [--docs 200000] [--no-mongo]

--docs documents `{"_id": i, "tags": [3 of 1,000 tags], "items": [3 ×
{"sku": one of 5,000, "qty": 0–999}]}`, loaded with the indexes there
(`load`), then 100 lookups each, a different value each time, counting
the documents found:
- `tag`: a tag's documents (about 600) — nucleus `doc @> '{"tags":
  ["t…"]}'` by `CREATE INDEX ON c (tags[*])`; PostgreSQL the same
  condition by a GIN index (`jsonb_path_ops`); MongoDB `{tags: "t…"}` by
  its multikey index on `tags`;
- `sku`: an item's (about 120) — `doc @> '{"items": [{"sku": "s…"}]}'`
  by `(items[*].sku)`; the same GIN index; `{"items.sku": "s…"}`;
- `qty range`: an item quantity above a bound (about 1,800) — `doc @?
  'strict $.items[*] ? (@.qty > n)'` by `(items[*].qty)`; PostgreSQL's
  GIN serves no range (a full scan, as PostgreSQL has it); `{"items.qty":
  {$gt: n}}`.
`nucleus filter`: the same lookups written as filter documents (§M16
Q1, Q2) — `doc @? json_filter('{"tags": "t…"}')`, `{"items.sku":
"s…"}`, `{"items.qty": {"$gt": n}}`, MongoDB's own filters — served by
path indexes on `tags`, `items.sku`, `items.qty` (the values lax mode
finds there).
Each the median of five runs of its 100 lookups.  nucleus as a server of
the given binary (its Python client, one round trip a lookup);
PostgreSQL 18 native and tuned (`bench/pg.py`; one psql script, its
`\\timing`); MongoDB 8 in Docker (`mongo:8`, 1 GB, a 256 MB cache — the
data is about 60 MB —, mongosh inside the
container: no network between them; Docker's VM is the host's other
cost).  Every time is printed as an `@metric` line, for `bench/method.py
run`.
"""

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "drivers", "python"))
sys.path.insert(0, HERE)

import method  # noqa: E402
import nucleus  # noqa: E402
from pg import DEFAULT, Cluster  # noqa: E402

RUNS = 5
LOOKUPS = 100
MONGO = "nucleus-bench-mongo"


def load_sql(docs, jsonb):
    """The documents, as `INSERT … SELECT` builds them."""
    j = "jsonb" if jsonb else "json"
    return (
        f"SELECT {j}_build_object('_id', i, "
        f"'tags', {j}_build_array('t' || (i % 1000), 't' || ((i * 7) % 1000), 't' || ((i * 13) % 1000)), "
        f"'items', {j}_build_array("
        f"{j}_build_object('sku', 's' || (i % 5000), 'qty', (i * 31) % 1000), "
        f"{j}_build_object('sku', 's' || ((i * 3) % 5000), 'qty', (i * 37) % 1000), "
        f"{j}_build_object('sku', 's' || ((i * 11) % 5000), 'qty', (i * 41) % 1000))) "
        f"FROM generate_series(1, {docs}) i"
    )


def values(kind):
    """The 100 values looked up."""
    if kind == "tag":
        return [f"t{(k * 37) % 1000}" for k in range(LOOKUPS)]
    if kind == "sku":
        return [f"s{(k * 97) % 5000}" for k in range(LOOKUPS)]
    return [990 - (k % 10) for k in range(LOOKUPS)]


def condition(kind, v, filtered=False):
    if filtered:
        f = {"tag": f'{{"tags": "{v}"}}', "sku": f'{{"items.sku": "{v}"}}'}.get(
            kind, f'{{"items.qty": {{"$gt": {v}}}}}')
        return f"doc @? json_filter('{f}')"
    if kind == "tag":
        return f"doc @> '{{\"tags\": [\"{v}\"]}}'"
    if kind == "sku":
        return f"doc @> '{{\"items\": [{{\"sku\": \"{v}\"}}]}}'"
    return f"doc @? 'strict $.items[*] ? (@.qty > {v})'"


KINDS = ["tag", "sku", "qty range"]


def run_nucleus(binary, docs, filtered=False):
    work = tempfile.mkdtemp(prefix="nucleus-multikey-")
    server = subprocess.Popen([binary, "server", os.path.join(work, "db"), "--listen", "127.0.0.1:20239"],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    out, counts = {}, {}
    try:
        for _ in range(60):
            try:
                db = nucleus.connect("nucleus://nucleus:nucleus@127.0.0.1:20239/nucleus")
                break
            except Exception:
                time.sleep(0.5)
        loads = []
        for _ in range(RUNS):
            db.execute("DROP TABLE IF EXISTS c")
            db.execute("CREATE COLLECTION c")
            for p in (("tags", "items.sku", "items.qty") if filtered
                      else ("tags[*]", "items[*].sku", "items[*].qty")):
                db.execute(f"CREATE INDEX ON c ({p})")
            start = time.perf_counter()
            db.execute(f"INSERT INTO c (doc) {load_sql(docs, False)}")
            loads.append(time.perf_counter() - start)
        out["load"] = loads
        db.execute("ANALYZE c")
        for kind in KINDS:
            plan = "\n".join(r[0] for r in db.execute(
                f"EXPLAIN SELECT count(*) FROM c WHERE {condition(kind, values(kind)[0], filtered)}").rows)
            assert "Index Scan" in plan, plan
            times = []
            for _ in range(RUNS + 1):
                total = 0
                start = time.perf_counter()
                for v in values(kind):
                    total += db.execute(
                        f"SELECT count(*) FROM c WHERE {condition(kind, v, filtered)}").rows[0][0]
                times.append(time.perf_counter() - start)
            out[kind] = times[1:]
            counts[kind] = total
    finally:
        server.terminate()
        server.wait()
        shutil.rmtree(work, ignore_errors=True)
    return out, counts


def psql(pg, script):
    return subprocess.run(pg.tool("psql") + ["-X", "-q", "-A", "-t", "-v", "ON_ERROR_STOP=1"], input=script,
                          capture_output=True, text=True, check=True).stdout


def run_postgres(docs):
    out, counts = {}, {}
    with Cluster(DEFAULT) as pg:
        loads = []
        for _ in range(RUNS):
            script = ("DROP TABLE IF EXISTS c;\nCREATE TABLE c (id serial PRIMARY KEY, doc jsonb NOT NULL);\n"
                      "CREATE INDEX c_doc ON c USING gin (doc jsonb_path_ops);\n\\timing on\n"
                      f"INSERT INTO c (doc) {load_sql(docs, True)};\n")
            loads.append(float(re.findall(r"Time: ([0-9.]+) ms", psql(pg, script))[-1]) / 1000)
        out["load"] = loads
        psql(pg, "VACUUM ANALYZE c;\n")
        for kind in KINDS:
            body = "".join(f"SELECT count(*) FROM c WHERE {condition(kind, v)};\n" for v in values(kind))
            times = []
            for _ in range(RUNS + 1):
                text = psql(pg, "\\timing on\n" + body)
                times.append(sum(float(m) for m in re.findall(r"Time: ([0-9.]+) ms", text)) / 1000)
                counts[kind] = sum(int(line) for line in text.splitlines() if line.strip().isdigit())
            out[kind] = times[1:]
    return out, counts


def mongosh(script):
    r = subprocess.run(["docker", "exec", "-i", MONGO, "mongosh", "--quiet", "bench"], input=script,
                       capture_output=True, text=True, check=True)
    return r.stdout


def run_mongo(docs):
    subprocess.run(["docker", "rm", "-f", MONGO], capture_output=True)
    subprocess.run(["docker", "run", "-d", "--name", MONGO, "--memory", "1g", "mongo:8",
                    "--wiredTigerCacheSizeGB", "0.25"], check=True, capture_output=True)
    out, counts = {}, {}
    try:
        for _ in range(60):
            if subprocess.run(["docker", "exec", MONGO, "mongosh", "--quiet", "--eval", "1"],
                              capture_output=True).returncode == 0:
                break
            time.sleep(1)
        # Built and inserted 10,000 at a time (all of them at once outgrow
        # the container), only the inserts timed.
        load = ("db.c.drop(); db.c.createIndex({tags: 1}); db.c.createIndex({'items.sku': 1}); "
                "db.c.createIndex({'items.qty': 1});\nlet ms = 0;\n"
                f"for (let k = 1; k <= {docs}; k += 10000) {{ const docs = []; "
                f"for (let i = k; i < k + 10000 && i <= {docs}; i++) docs.push({{_id: i, "
                "tags: ['t' + (i % 1000), 't' + ((i * 7) % 1000), 't' + ((i * 13) % 1000)], "
                "items: [{sku: 's' + (i % 5000), qty: (i * 31) % 1000}, "
                "{sku: 's' + ((i * 3) % 5000), qty: (i * 37) % 1000}, "
                "{sku: 's' + ((i * 11) % 5000), qty: (i * 41) % 1000}]});\n"
                "const t0 = Date.now(); db.c.insertMany(docs, {ordered: false}); ms += Date.now() - t0; }\n"
                "print('ms', ms);\n")
        out["load"] = [float(re.findall(r"ms (\d+)", mongosh(load))[-1]) / 1000 for _ in range(RUNS)]
        for kind in KINDS:
            vs = values(kind)
            if kind == "tag":
                filters = [f"{{tags: '{v}'}}" for v in vs]
            elif kind == "sku":
                filters = [f"{{'items.sku': '{v}'}}" for v in vs]
            else:
                filters = [f"{{'items.qty': {{$gt: {v}}}}}" for v in vs]
            script = (f"const fs = [{', '.join(filters)}];\n"
                      f"if (db.c.find(fs[0]).explain().queryPlanner.winningPlan.inputStage?.stage !== 'IXSCAN') "
                      f"print('noindex', JSON.stringify(db.c.find(fs[0]).explain().queryPlanner.winningPlan));\n"
                      f"for (let r = 0; r <= {RUNS}; r++) {{ let n = 0; const t0 = Date.now(); "
                      "for (const f of fs) n += db.c.countDocuments(f); print('ms', Date.now() - t0, n); }\n")
            text = mongosh(script)
            assert "noindex" not in text, text
            runs = re.findall(r"ms (\d+) (\d+)", text)
            out[kind] = [float(ms) / 1000 for ms, _ in runs[1:]]
            counts[kind] = int(runs[-1][1])
    finally:
        subprocess.run(["docker", "rm", "-f", MONGO], capture_output=True)
    return out, counts


def main():
    a = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    a.add_argument("--nucleus", required=True)
    a.add_argument("--docs", type=int, default=200_000)
    a.add_argument("--no-mongo", action="store_true")
    args = a.parse_args()
    results = {"nucleus": run_nucleus(args.nucleus, args.docs),
               "nucleus filter": run_nucleus(args.nucleus, args.docs, filtered=True),
               "postgresql": run_postgres(args.docs)}
    if not args.no_mongo:
        results["mongodb"] = run_mongo(args.docs)
    # The same documents found by every system.
    for kind in KINDS:
        found = {s: c[kind] for s, (_, c) in results.items()}
        assert len(set(found.values())) == 1, (kind, found)
    systems = list(results)
    print(f"{args.docs} documents; load and {LOOKUPS} lookups a case, median of {RUNS}, ms")
    print(f"{'':10}" + "".join(f"{s:>15}" for s in systems))
    for case in ["load"] + KINDS:
        row = []
        for s in systems:
            t = results[s][0][case]
            method.emit(case, t, unit="s", system=s, phase="write" if case == "load" else "warm")
            row.append(method.median(t) * 1000)
        print(f"{case:10}" + "".join(f"{x:15.1f}" for x in row))


if __name__ == "__main__":
    main()
