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

2026-10-29, Apple M2 Pro (macOS, 12 cores), nucleus release build after
§M14.20 V1–V4 (chunked scans, the vectorized executor, batch kernels,
parallel scans and aggregates); PostgreSQL 18.6 in Docker (Linux VM,
10 CPUs) with its defaults, parallel workers included.

| 1,000,000 rows | nucleus (ms) | PostgreSQL (ms) | ratio | nucleus at `b03c130` (ms) |
|---|---:|---:|---:|---:|
| load (INSERT … SELECT) | 6,833.1 | 6,084.1 | 1.1× | 5,177.5 |
| count(*) | 27.2 | 15.3 | 1.8× | 362.2 |
| filtered count | 27.2 | 17.2 | 1.6× | 423.6 |
| sum | 26.7 | 17.8 | 1.5× | 426.4 |
| group by (1000 groups) | 37.0 | 35.9 | 1.0× | 538.9 |
| sort | 910.0 | 154.7 | 5.9× | 1,147.6 |
| self-join | 1,724.5 | 545.2 | 3.2× | 1,975.2 |
| point reads (1000 by key) | 101.5 | 128.9 | 0.8× | 60.8 |
| update 25% | 4,503.0 | 7,160.4 | 0.6× | 2,437.7 |
| delete 10% | 3,670.4 | 210.1 | 17.5× | 2,249.7 |
| count after writes | 126.2 | 22.5 | 5.6× | 545.7 |

(Writes vary by a factor of two from run to run on this machine, both
systems' — Docker's file system and the laptop's disk; compare them
within one run.)

What the numbers say: scans and aggregates went from 15–30× PostgreSQL's
time to 1.5–1.8× (the 1000-group `GROUP BY` even).  A profile had put
half their time in the allocator (every entry copied into fresh vectors
by the B+tree, the engine and the executor; V1), most of the rest in a
value built per cell (V2); kernels per CPU level (V3) and morsels on
worker threads (V4) did the rest.  What is left of a scan is the
engine's work per entry — decoding each version key and copying entries
out of pages — which columnar storage (V7) removes.  A count after the
writes reads the old versions too, until vacuum.  Sorts and joins still
run a row at a time on one thread; deletes pay for a lookup per row in
the engine's conflict and garbage checks.

Earlier runs (at `b03c130`, §M14.11: scans 15–30× slower; after V2:
5–6×) are in the git history of this file.

## Targets

Set from this first run (PLAN.md §M14.11 asks for targets from the first
results): within 2× of PostgreSQL on loads and updates, within 3× on
scans, sorts and joins once §M14.20 lands, at least even on reads by key.
