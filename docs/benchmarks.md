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

> **Not up to date.**  These numbers are of `09a6d55`.  Changes since
> (join estimates on two-column keys, semi joins placed below joins, the
> sharded buffer pool, hardware CRC-32C, LZ only for values of 1 KiB or
> more, the column cache) cut, measured on rows in one session, Q9
> 26 → 6.7 s, Q18 25 → 9.6 s, Q4 2.5 → 1.9 s, Q6 1.1 → 0.7 s; a full run
> against PostgreSQL follows the next changes.

2026-10-30, SF 1 (6 million `lineitem` rows), the machine above; one run
per query; nucleus with every table in rows, and with all but `region` and
`nation` in columns:

| TPC-H SF 1 | nucleus (row) (ms) | nucleus (column) (ms) | postgresql (ms) | nucleus (row) / PostgreSQL | nucleus (column) / PostgreSQL | results |
|---|---:|---:|---:|---:|---:|---|
| Q1 | 1,838.8 | 3,585.0 | 1,510.5 | 1.22× | 2.37× | same |
| Q2 | 13,699.9 | 13,077.9 | 338.7 | 40.44× | 38.61× | same |
| Q3 | 3,244.6 | 2,836.3 | 445.7 | 7.28× | 6.36× | same |
| Q4 | 2,539.8 | 1,550.8 | 157.5 | 16.13× | 9.85× | same |
| Q5 | 7,517.9 | 5,634.4 | 585.5 | 12.84× | 9.62× | same |
| Q6 | 1,097.8 | 630.8 | 217.7 | 5.04× | 2.90× | same |
| Q7 | 6,183.0 | 4,914.1 | 3,300.9 | 1.87× | 1.49× | same |
| Q8 | 4,735.8 | 2,612.7 | 364.2 | 13.00× | 7.17× | same |
| Q9 | 26,283.7 | 22,200.5 | 1,607.5 | 16.35× | 13.81× | same |
| Q10 | 2,355.0 | 1,762.5 | 435.2 | 5.41× | 4.05× | same |
| Q11 | 1,017.3 | 878.7 | 162.4 | 6.26× | 5.41× | same |
| Q12 | 2,015.6 | 1,219.2 | 320.5 | 6.29× | 3.80× | same |
| Q13 | 2,593.2 | 2,442.7 | 405.1 | 6.40× | 6.03× | same |
| Q14 | 1,445.5 | 836.3 | 220.5 | 6.56× | 3.79× | same |
| Q15 | 961.3 | 476.7 | 239.5 | 4.01× | 1.99× | same |
| Q16 | 813.2 | 886.2 | 154.4 | 5.27× | 5.74× | same |
| Q17 | 6,114.5 | — | 707.4 | 8.64× | — | same |
| Q18 | 25,319.0 | 21,702.6 | 3,020.9 | 8.38× | 7.18× | same |
| Q19 | 1,573.6 | 2,472.8 | 59.9 | 26.26× | 41.26× | same |
| Q20 | 1,501.8 | — | 336.9 | 4.46× | — | same |
| Q21 | 10,802.9 | 8,292.9 | 1,734.5 | 6.23× | 4.78× | same |
| Q22 | 1,320.8 | 1,512.0 | 63.5 | 20.80× | 23.81× | same |
| geometric mean (20 queries) | 3,171.6 | 2,618.2 | 403.7 | 7.86× | 6.49× |  |
| load (COPY, indexes, ANALYZE; s) | 220.8 | 729.4 | 67.2 |  |  |  |

Every result agrees with PostgreSQL's.  Where the time goes:

- **Joins** run a row at a time on one thread (hash joins, no batches, no
  morsels): the many-join queries (Q2, Q5, Q8, Q9, Q18, Q21) are 7–40×
  PostgreSQL's.  Q2's correlated `min` subquery runs once per joined row.
- **Q17 and Q20 on column tables** passed the 600 s limit: their
  correlated subqueries read rows by index, and each read of a column
  table decoded a whole segment column again — its cache of decoded
  columns was smaller than one table's and emptied whole when full.
- **Loads**: `COPY` stores row by row in one transaction (3.3× slower in
  rows; 11× in columns).
- The single-table scans and aggregates, Q1 and Q6, are 1.2× and 5.0×
  PostgreSQL's in rows, 2.4× and 2.9× in columns.

## Targets

Set from this first run (PLAN.md §M14.11 asks for targets from the first
results): within 2× of PostgreSQL on loads and updates, within 3× on
scans, sorts and joins once §M14.20 lands, at least even on reads by key.
