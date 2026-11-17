#!/usr/bin/env python3
"""The benchmark suite (PLAN.md §M14.11 L8, by §M14.31 P1's method): one
workload on nucleus and on PostgreSQL, on the same machine and data, timed
statement by statement.

    bench/run.py --nucleus target/release/nucleus [--rows 1000000] [--runs 5]
                 [--pg-data target/bench-pg | --psql "psql …" | --no-pg]
                 [--embedded] [--retries 2] [--out bench/results/run.json]

nucleus runs as `nucleus server` on a fresh directory each round, driven by
`nucleus sql -h` (--embedded: in-process, no server); PostgreSQL native on
this machine, a cluster of its own under --pg-data tuned for it
(bench/pg.py) — or one already running, through --psql (PG* variables),
which cannot be restarted and so has no cold runs.  Each system's times
come from its own client's `\\timing`, in one session kept open, so
neither pays for a connection.

The method (bench/method.py): rounds of the whole workload, the systems in
turn (A, B, then B, A, …), each on a freshly loaded table; a read step is
timed cold — its server just restarted (its caches empty, not the OS's) —
and then warm; per step and phase the median of the rounds with its
interval and spread, rounds run again while one spreads past the bound.
Every read's answer is checked between the systems.  The output is a
Markdown table (the ratio is nucleus over PostgreSQL, `≈` where the
intervals overlap); --out writes every time with the environment.
"""

import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import method  # noqa: E402
from common import COLD, NucleusSystem, PostgresSystem, same  # noqa: E402

# (name, SQL with {n} for the row count, kind) — run in order, so later
# steps see the table earlier ones built.  A "read" is timed cold and
# warm; a "write" once a round.
WORKLOAD = [
    ("load (INSERT … SELECT)", "INSERT INTO b SELECT i, i % 1000, i::bigint * 7919 % 1000003, "
                               "'row ' || i FROM generate_series(1, {n}) i", "write"),
    ("count(*)", "SELECT count(*) FROM b", "read"),
    ("filtered count", "SELECT count(*) FROM b WHERE v % 7 = 3", "read"),
    ("sum", "SELECT sum(v) FROM b", "read"),
    ("group by (1000 groups)", "SELECT g, count(*), sum(v) FROM b GROUP BY g ORDER BY g", "read"),
    ("sort", "SELECT count(*) FROM (SELECT v FROM b ORDER BY v) x", "read"),
    ("self-join", "SELECT count(*) FROM b x JOIN b y ON x.id = y.v", "read"),
    ("point reads (1000 by key)", None, "points"),
    ("update 25%", "UPDATE b SET v = v + 1 WHERE id % 4 = 0", "write"),
    ("delete 10%", "DELETE FROM b WHERE id % 10 = 0", "write"),
    ("count after writes", "SELECT count(*) FROM b", "read"),
]
CREATE = "CREATE TABLE b (id INT PRIMARY KEY, g INT, v BIGINT, s TEXT)"

POINTS = 1000


def point_reads(n):
    step = max(1, n // POINTS)
    return [f"SELECT s FROM b WHERE id = {1 + (i * step) % n}" for i in range(POINTS)]


def one_round(system, n, check):
    """The workload once on a fresh table: {(step, phase): ms}; with
    `check`, each read's answer as well."""
    times, answers = {}, {}
    system.fresh(["b"])
    system.session.time(CREATE)
    for name, sql, kind in WORKLOAD:
        if kind == "write":
            times[(name, "write")] = system.session.time(sql.format(n=n))
            if name.startswith("load"):
                system.session.time(system.after_load + " b")
            continue
        statements = point_reads(n) if kind == "points" else [sql.format(n=n)]
        cold = name != "count after writes" and system.can_restart
        if cold:
            system.restart()
            times[(name, "cold")] = sum(system.session.time(s) for s in statements)
        times[(name, "warm")] = sum(system.session.time(s) for s in statements)
        if check:
            answers[name] = [system.results(s) for s in statements[:10]]
    return times, answers


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--nucleus", required=True, help="the nucleus binary")
    ap.add_argument("--rows", type=int, default=1_000_000)
    ap.add_argument("--runs", type=int, default=method.RUNS)
    ap.add_argument("--pg-data", default=os.path.join(method.ROOT, "target", "bench-pg"),
                    help="the PostgreSQL cluster's directory (made on first use)")
    ap.add_argument("--psql", default=None, help="a running PostgreSQL's psql command instead")
    ap.add_argument("--no-pg", action="store_true", help="nucleus only")
    ap.add_argument("--embedded", action="store_true", help="nucleus in-process, without a server")
    ap.add_argument("--retries", type=int, default=method.RETRIES,
                    help="times the rounds run again while one spreads past the bound")
    ap.add_argument("--out", "--json", dest="out", default=None, help="the results file")
    a = ap.parse_args()

    systems = [NucleusSystem(a.nucleus, "nucleus (embedded)" if a.embedded else "nucleus", a.embedded)]
    try:
        if not a.no_pg:
            systems.append(PostgresSystem(a.pg_data, a.psql))
        env = method.environment({s.name: s.describe() for s in systems},
                                 where=systems[0].work)
        env["cold"] = COLD
        attempts, checks = 0, {}
        while True:
            attempts += 1
            got = {}
            for r in range(a.runs):
                answers = {}
                for s in method.turn(systems, r):
                    method.quiet()
                    print(f"round {r + 1}/{a.runs} (attempt {attempts}): {s.name}", file=sys.stderr, flush=True)
                    t, answers[s.name] = one_round(s, a.rows, check=r == 0)
                    for (name, phase), ms in t.items():
                        got.setdefault((name, phase, s.name), []).append(ms)
                if r == 0:
                    checks = check_answers(systems, answers)
            worst = max(method.cv(v) for v in got.values())
            if worst <= method.CV_BOUND or attempts > a.retries:
                break
        samples = [method.Sample(name, system, v, phase, flagged=method.cv(v) > method.CV_BOUND,
                                 attempts=attempts) for (name, phase, system), v in got.items()]
    finally:
        for s in systems:
            s.close()

    baseline = "postgresql" if not a.no_pg else None
    print(method.table(samples, baseline=baseline, title=f"{a.rows:,} rows"))
    print()
    print(method.describe(env))
    if a.out:
        method.write(a.out, "run", env, samples, {"baseline": baseline, "rows": a.rows, "checks": checks})
    if any(v == "DIFFERENT" for v in checks.values()):
        sys.exit("some answers differ")


def check_answers(systems, answers):
    """Each read's answer (its first ten statements') on nucleus against
    PostgreSQL's."""
    ref = answers.get("postgresql")
    out = {}
    for s in systems:
        if s.name == "postgresql" or ref is None:
            continue
        for name, got in answers[s.name].items():
            if any(g is None for g in got):
                out[name] = "unchecked"
                continue
            verdicts = {same(g, w) for g, w in zip(got, ref[name])}
            out[name] = "DIFFERENT" if "DIFFERENT" in verdicts else ", ".join(sorted(verdicts))
            if out[name] == "DIFFERENT":
                print(f"DIFFERENT: {name}", file=sys.stderr)
    return out


if __name__ == "__main__":
    main()
