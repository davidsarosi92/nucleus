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

2026-10-29, Apple M2 Pro (macOS), nucleus release build after §M14.20
V1–V2 (chunked scans, the vectorized executor); PostgreSQL 18.6 in
Docker (Linux VM, 10 CPUs) with its defaults — so it uses parallel
workers for scans, which nucleus does not have yet (§M14.20 V4).

| 1,000,000 rows | nucleus (ms) | PostgreSQL (ms) | ratio | nucleus at `b03c130` (ms) |
|---|---:|---:|---:|---:|
| load (INSERT … SELECT) | 5,311.5 | 8,717.0 | 0.6× | 5,177.5 |
| count(*) | 83.5 | 13.6 | 6.1× | 362.2 |
| filtered count | 88.4 | 16.1 | 5.5× | 423.6 |
| sum | 88.8 | 16.4 | 5.4× | 426.4 |
| group by (1000 groups) | 99.4 | 33.9 | 2.9× | 538.9 |
| sort | 925.3 | 153.6 | 6.0× | 1,147.6 |
| self-join | 1,531.6 | 336.8 | 4.5× | 1,975.2 |
| point reads (1000 by key) | 64.8 | 133.1 | 0.5× | 60.8 |
| update 25% | 2,241.0 | 2,223.4 | 1.0× | 2,437.7 |
| delete 10% | 1,967.7 | 143.9 | 13.7× | 2,249.7 |
| count after writes | 258.9 | 21.7 | 12.0× | 545.7 |

(PostgreSQL's load took 1.1 s in the run before; Docker's timings vary
from run to run, nucleus's within a few per cent.)

What the numbers say: scans and aggregates went from 15–30× to 3–6×
of PostgreSQL — a profile had put half their time in the allocator
(every entry copied into fresh vectors by the B+tree, the engine and the
executor; V1) and most of the rest in a value built per cell (V2).  What
is left of a scan is the engine's work per entry: decoding each version
key and copying entries out of pages — and PostgreSQL's ten workers.  A
count after the writes reads the old versions too, until vacuum.  Sorts
and joins still run a row at a time; deletes pay for a lookup per row in
the engine's conflict and garbage checks.

The run before, at `b03c130` (2026-10-27, §M14.11), against PostgreSQL's
numbers then (scans 15–30× slower) is in the git history of this file.

## Targets

Set from this first run (PLAN.md §M14.11 asks for targets from the first
results): within 2× of PostgreSQL on loads and updates, within 3× on
scans, sorts and joins once §M14.20 lands, at least even on reads by key.
