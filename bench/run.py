#!/usr/bin/env python3
"""The benchmark suite (PLAN.md §M14.11 L8): one workload on nucleus and on
PostgreSQL, on the same machine and data, timed statement by statement.

    bench/run.py --nucleus target/release/nucleus [--rows 1000000]
                 [--pg "host=127.0.0.1 user=postgres"] [--json out.json]

nucleus runs as `nucleus server` on a fresh directory, driven by
`nucleus sql -h` (--embedded: in-process, no server); PostgreSQL through
`psql` (PGHOST, PGUSER, PGPASSWORD … or --pg) — client and server both.  Each system's
times come from its own client's `\\timing`, so neither pays for the
other's connection.  The output is a Markdown table (the ratio is nucleus
over PostgreSQL); --json writes the numbers.  No dependencies.
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

# (name, SQL with {n} for the row count, repeat) — run in order, so later
# steps see the table earlier ones built.
WORKLOAD = [
    ("create", "CREATE TABLE b (id INT PRIMARY KEY, g INT, v BIGINT, s TEXT)", 1),
    ("load (INSERT … SELECT)", "INSERT INTO b SELECT i, i % 1000, i::bigint * 7919 % 1000003, "
                               "'row ' || i FROM generate_series(1, {n}) i", 1),
    ("count(*)", "SELECT count(*) FROM b", 3),
    ("filtered count", "SELECT count(*) FROM b WHERE v % 7 = 3", 3),
    ("sum", "SELECT sum(v) FROM b", 3),
    ("group by (1000 groups)", "SELECT g, count(*), sum(v) FROM b GROUP BY g", 3),
    ("sort", "SELECT count(*) FROM (SELECT v FROM b ORDER BY v) x", 3),
    ("self-join", "SELECT count(*) FROM b x JOIN b y ON x.id = y.v", 3),
    ("point reads (1000 by key)", None, 1),  # generated below
    ("update 25%", "UPDATE b SET v = v + 1 WHERE id % 4 = 0", 1),
    ("delete 10%", "DELETE FROM b WHERE id % 10 = 0", 1),
    ("count after writes", "SELECT count(*) FROM b", 3),
]

POINTS = 1000


def point_reads(n):
    step = max(1, n // POINTS)
    return [f"SELECT s FROM b WHERE id = {1 + (i * step) % n}" for i in range(POINTS)]


def timings(text):
    """The `Time: … ms` lines of a client's output, in order."""
    return [float(m) for m in re.findall(r"^Time: ([0-9.]+) ms", text, re.M)]


def run_client(cmd, statements, env=None):
    """Runs `statements` (one per line) through a client reading stdin with
    timing on; the time of each, in ms."""
    script = "\\timing on\n" + "".join(s.rstrip(";") + ";\n" for s in statements)
    out = subprocess.run(cmd, input=script, capture_output=True, text=True, env=env, check=False)
    text = out.stdout + out.stderr
    if out.returncode != 0 or "ERROR" in text:
        sys.exit(f"{cmd[0]} failed:\n{text[-4000:]}")
    got = timings(text)
    if len(got) != len(statements):
        sys.exit(f"{cmd[0]}: {len(got)} timings for {len(statements)} statements:\n{text[-4000:]}")
    return got


def plan(n):
    """Statements and, per workload step, which of them it is."""
    statements, steps = [], []
    for name, sql, repeat in WORKLOAD:
        if sql is None:
            group = point_reads(n)
        else:
            group = [sql.format(n=n)] * repeat
        steps.append((name, len(statements), len(group), repeat))
        statements.extend(group)
    return statements, steps


def summarize(times, steps):
    """Per step: the best of its repeats, or the total of a group of
    different statements (point reads)."""
    out = {}
    for name, at, count, repeat in steps:
        chunk = times[at:at + count]
        out[name] = min(chunk) if repeat > 1 else sum(chunk)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--nucleus", required=True, help="the nucleus binary")
    ap.add_argument("--rows", type=int, default=1_000_000)
    ap.add_argument("--pg", default=None, help="a libpq connection string (else PG* variables)")
    ap.add_argument("--no-pg", action="store_true", help="nucleus only")
    ap.add_argument("--embedded", action="store_true", help="nucleus in-process, without a server")
    ap.add_argument("--json", default=None)
    a = ap.parse_args()

    statements, steps = plan(a.rows)
    results = {}

    work = tempfile.mkdtemp(prefix="nucleus-bench-")
    server = None
    try:
        db = os.path.join(work, "db")
        if a.embedded:
            client = [a.nucleus, "sql", db]
        else:
            server = subprocess.Popen([a.nucleus, "server", db, "--listen", "127.0.0.1:0"],
                                      stderr=subprocess.PIPE, text=True)
            line = server.stderr.readline()
            m = re.search(r"127\.0\.0\.1:(\d+)", line)
            if not m:
                sys.exit(f"nucleus server did not start: {line}")
            client = [a.nucleus, "sql", "-h", f"127.0.0.1:{m.group(1)}"]
        started = time.time()
        t = run_client(client, statements)
        results["nucleus"] = summarize(t, steps)
        print(f"nucleus: {time.time() - started:.1f} s", file=sys.stderr)
    finally:
        if server:
            server.terminate()
            server.wait()
        shutil.rmtree(work, ignore_errors=True)

    if not a.no_pg:
        psql = ["psql", "-X", "-q", "-v", "ON_ERROR_STOP=1"] + ([a.pg] if a.pg else [])
        subprocess.run(psql + ["-c", "DROP TABLE IF EXISTS b"], check=True, capture_output=True)
        started = time.time()
        t = run_client(psql, statements)
        results["postgresql"] = summarize(t, steps)
        print(f"postgresql: {time.time() - started:.1f} s", file=sys.stderr)
        subprocess.run(psql + ["-c", "DROP TABLE IF EXISTS b"], check=True, capture_output=True)

    rows = []
    header = f"| {a.rows:,} rows | nucleus (ms) |"
    rule = "|---|---:|"
    if "postgresql" in results:
        header += " PostgreSQL (ms) | ratio |"
        rule += "---:|---:|"
    print(header)
    print(rule)
    for name, _, _, _ in steps:
        if name == "create":
            continue
        nu = results["nucleus"][name]
        line = f"| {name} | {nu:,.1f} |"
        if "postgresql" in results:
            pg = results["postgresql"][name]
            line += f" {pg:,.1f} | {nu / pg:,.1f}× |" if pg > 0 else f" {pg:,.1f} | — |"
        print(line)
        rows.append(name)
    if a.json:
        with open(a.json, "w") as f:
            json.dump({"rows": a.rows, "results": results}, f, indent=2)


if __name__ == "__main__":
    main()
