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

**A columnar table** (§M14.20 V7), the same million rows in `WITH
(storage = column)` (loaded by `INSERT … SELECT` from the row table in
4.0 s; its columns 32 MB): `sum` 2.0 ms (one thread 12.8 ms; the row
table 22 / 81 ms), the filtered count 2.4 ms, the 1000-group `GROUP BY`
19 ms, a read by key 0.3–1.7 ms (0.1 ms).  Not yet in the suite run
against PostgreSQL.

**Results into numpy** (§M14.20 V10): a million rows (`id int, x bigint,
f float, s text`) fetched with the Python driver's `batches()` and every
column turned into a numpy array, best of three, server and client on
the same machine — through rows (before V10) and with the executor's
batches going out as columns:

| query | row table, rows → columns (ms) | row table, batches (ms) | column table, rows → columns (ms) | column table, batches (ms) |
|---|---:|---:|---:|---:|
| `SELECT id, x, f` | 262 | 92 | 241 | 51 |
| `SELECT id, x, f, s` | 789 | 198 | 805 | 162 |
| `SELECT id, f … WHERE x < 500` (half) | 52 | 33 | 34 | 16 |

(With `s`, the driver's text decoding was most of it: it now decodes a
batch's text once and cuts it.)

Earlier runs (at `b03c130`, §M14.11: scans 15–30× slower; after V2:
5–6×) are in the git history of this file.

## TPC-H

`bench/tpch.py` (§M14.20 V10): TPC-H's schema, its data generated from a
fixed seed with the specification's sizes, keys and distributions
(`bench/tpch_data.py`; not `dbgen`'s text), primary keys and indexes on
the foreign keys' columns on both systems, the 22 queries with their
validation parameters (`bench/tpch_queries.py`), each nucleus run's
results compared with PostgreSQL's (numbers to cents).

```sh
python3 bench/tpch.py --nucleus target/release/nucleus --sf 1 \
  --psql "docker exec -i pg psql -U postgres"
```

2026-10-30, SF 1 (6 million `lineitem` rows), the machine above, at
`04c93f4`; each query's best of three runs; nucleus with every table in
rows, and with all but `region` and `nation` in columns; PostgreSQL 18 in
the same run:

| TPC-H SF 1 | nucleus (row) (ms) | nucleus (column) (ms) | postgresql (ms) | nucleus (row) / PostgreSQL | nucleus (column) / PostgreSQL | results |
|---|---:|---:|---:|---:|---:|---|
| Q1 | 992.9 | 975.2 | 990.6 | 1.00× | 0.98× | same |
| Q2 | 133.1 | — | 172.3 | 0.77× | — | same |
| Q3 | 1,219.2 | 340.5 | 284.9 | 4.28× | 1.20× | same |
| Q4 | 598.4 | 1,267.1 | 148.9 | 4.02× | 8.51× | same |
| Q5 | 2,071.3 | 644.8 | 430.5 | 4.81× | 1.50× | same |
| Q6 | 640.0 | 446.3 | 194.6 | 3.29× | 2.29× | same |
| Q7 | 4,078.3 | 3,745.3 | 2,543.9 | 1.60× | 1.47× | same |
| Q8 | 563.6 | 228.0 | 172.1 | 3.28× | 1.33× | same |
| Q9 | 2,775.5 | 2,368.1 | 1,302.0 | 2.13× | 1.82× | same |
| Q10 | 1,095.5 | 837.5 | 341.5 | 3.21× | 2.45× | same |
| Q11 | 250.4 | 54.1 | 33.3 | 7.53× | 1.63× | same |
| Q12 | 817.9 | 604.4 | 308.3 | 2.65× | 1.96× | same |
| Q13 | 1,220.7 | 1,107.7 | 330.8 | 3.69× | 3.35× | same |
| Q14 | 707.4 | 563.0 | 199.7 | 3.54× | 2.82× | same |
| Q15 | 572.4 | 345.5 | 206.8 | 2.77× | 1.67× | same |
| Q16 | 390.3 | 281.2 | 140.4 | 2.78× | 2.00× | same |
| Q17 | 476.5 | — | 455.1 | 1.05× | — | same |
| Q18 | 1,151.4 | 843.5 | 1,664.2 | 0.69× | 0.51× | same |
| Q19 | 770.6 | 840.7 | 18.8 | 41.02× | 44.75× | same |
| Q20 | 540.0 | — | 242.9 | 2.22× | — | same |
| Q21 | 2,674.4 | 4,693.9 | 1,524.1 | 1.75× | 3.08× | same |
| Q22 | 1,033.9 | 788.1 | 32.2 | 32.08× | 24.46× | same |
| geometric mean (19 queries) | 977.6 | 697.5 | 274.0 | 3.57× | 2.55× |  |
| load (COPY, indexes, ANALYZE; s) | 145.9 | 675.0 | 41.7 |  |  |  |

Every result agrees with PostgreSQL's.  Against the first run (`09a6d55`:
7.9× and 6.5× PostgreSQL's time in the geometric mean, a load of 221 s):
the joins vectorized and in parallel, narrow rows through them, index
nested loops, semi and anti joins placed by estimate, joins on two-column
keys estimated by the key, parallel aggregates in key order and parallel
sorts, the buffer pool in shards, hardware CRC-32C, less LZ.

Where the time still goes:

- **Q19 and Q22** (41× and 32×): PostgreSQL finds their few rows through
  indexes in milliseconds; nucleus hashes a large side.  The lookup costs
  are next.
- **Q2, Q17 and Q20 on column tables**: Q2 failed (`XX000 outer column out
  of range`: a correlated subquery under the new narrowing, a bug), Q17
  and Q20 passed the limit (a correlated subquery's index lookups into a
  column table, each a point read of segments).
- **Loads**: rows 146 s against 42 s — a large transaction's writes are
  written twice (the side area, then the versions; PLAN V11 removes it);
  columns 675 s, of which `lineitem`'s `COPY` is 84 s: the rest is being
  measured.
- The joins of many tables (Q5, Q7, Q9, Q13, Q21) are 1.5–5×: the
  build side is still built a row at a time, on one thread.

## Targets

Set from this first run (PLAN.md §M14.11 asks for targets from the first
results): within 2× of PostgreSQL on loads and updates, within 3× on
scans, sorts and joins once §M14.20 lands, at least even on reads by key.
