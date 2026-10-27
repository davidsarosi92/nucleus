# Benchmarks

nucleus against PostgreSQL on the same machine and data (PLAN.md §M14.11
L8).  The suite is `bench/run.py`: one workload — a 4-column table loaded
by `INSERT … SELECT`, counts, a filtered scan, a sum, a 1000-group
`GROUP BY`, a sort, a self-join, 1000 reads by key, an update of a quarter
of the rows and a delete of a tenth — run statement by statement through
each system's own client with `\timing`, both as client and server
(`nucleus server` with `nucleus sql -h`; `psql`).  A step repeated three
times reports its best; the point reads their total.

CI runs it on every push to `main` with a million rows against the
`postgres:18` image (job **benchmarks against PostgreSQL**; the table is
in the job summary, the numbers in the `bench` artifact).  It fails only
when a run fails: shared runners are too noisy for thresholds yet.

## Reproduce

```sh
cargo build --release -p nucleus-cli
# PostgreSQL reachable through the usual PG* variables (or --pg "…"):
PGHOST=127.0.0.1 PGUSER=postgres python3 bench/run.py \
  --nucleus target/release/nucleus --rows 1000000 --json bench.json
python3 bench/run.py --nucleus target/release/nucleus --no-pg   # nucleus alone
```

## Latest local run

2026-10-27, Apple M2 Pro (macOS), nucleus release build at `b03c130`;
PostgreSQL 18.6 in Docker (Linux VM, 10 CPUs) with its defaults — so it
uses parallel workers for scans, which nucleus does not have yet.

| 1,000,000 rows | nucleus (ms) | PostgreSQL (ms) | ratio |
|---|---:|---:|---:|
| load (INSERT … SELECT) | 5,177.5 | 1,088.6 | 4.8× |
| count(*) | 362.2 | 13.6 | 26.5× |
| filtered count | 423.6 | 15.9 | 26.6× |
| sum | 426.4 | 16.2 | 26.3× |
| group by (1000 groups) | 538.9 | 34.5 | 15.6× |
| sort | 1,147.6 | 151.4 | 7.6× |
| self-join | 1,975.2 | 134.9 | 14.6× |
| point reads (1000 by key) | 60.8 | 111.5 | 0.5× |
| update 25% | 2,437.7 | 765.7 | 3.2× |
| delete 10% | 2,249.7 | 73.4 | 30.7× |
| count after writes | 545.7 | 20.9 | 26.1× |

What the numbers say: loads and updates are within 3–5×, reads by key are
faster (no planner round trip to speak of); scans are 15–30× slower — the
executor builds a value per cell and copies rows between the B+tree, the
engine and itself, and has no parallel workers.  That is §M14.20's work
(column batches, vectorized execution, parallel scans).  Deletes pay for
a lookup per row in the engine's conflict and garbage checks.

## Targets

Set from this first run (PLAN.md §M14.11 asks for targets from the first
results): within 2× of PostgreSQL on loads and updates, within 3× on
scans, sorts and joins once §M14.20 lands, at least even on reads by key.
