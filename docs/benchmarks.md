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
`940395c`; each query's best of three runs; nucleus with every table in
rows, and with all but `region` and `nation` in columns; PostgreSQL 18 in
the same run:

| TPC-H SF 1 | nucleus (row) (ms) | nucleus (column) (ms) | postgresql (ms) | nucleus (row) / PostgreSQL | nucleus (column) / PostgreSQL | results |
|---|---:|---:|---:|---:|---:|---|
| Q1 | 1,001.6 | 746.3 | 1,001.4 | 1.00× | 0.75× | same |
| Q2 | 50.8 | 63.4 | 182.8 | 0.28× | 0.35× | same |
| Q3 | 1,299.2 | 182.3 | 289.1 | 4.49× | 0.63× | same |
| Q4 | 597.0 | 103.7 | 146.7 | 4.07× | 0.71× | same |
| Q5 | 2,151.4 | 576.1 | 416.6 | 5.16× | 1.38× | same |
| Q6 | 443.4 | 155.8 | 195.2 | 2.27× | 0.80× | same |
| Q7 | 1,928.1 | 255.5 | 2,724.9 | 0.71× | 0.09× | same |
| Q8 | 655.3 | 159.8 | 187.4 | 3.50× | 0.85× | same |
| Q9 | 3,110.5 | 2,744.3 | 1,530.2 | 2.03× | 1.79× | same |
| Q10 | 1,126.2 | 675.4 | 345.1 | 3.26× | 1.96× | same |
| Q11 | 270.6 | 47.8 | 40.6 | 6.67× | 1.18× | same |
| Q12 | 827.5 | 585.1 | 301.4 | 2.75× | 1.94× | same |
| Q13 | 1,259.9 | 1,201.2 | 347.1 | 3.63× | 3.46× | same |
| Q14 | 502.6 | 187.3 | 198.6 | 2.53× | 0.94× | same |
| Q15 | 470.4 | 151.9 | 208.9 | 2.25× | 0.73× | same |
| Q16 | 432.2 | 475.7 | 127.5 | 3.39× | 3.73× | same |
| Q17 | 61.0 | 103.6 | 479.1 | 0.13× | 0.22× | same |
| Q18 | 1,404.1 | 866.0 | 1,586.5 | 0.88× | 0.55× | same |
| Q19 | 142.2 | 168.2 | 18.9 | 7.54× | 8.92× | same |
| Q20 | 594.1 | 630.5 | 257.8 | 2.30× | 2.45× | same |
| Q21 | 3,127.0 | 1,223.2 | 1,722.0 | 1.82× | 0.71× | same |
| Q22 | 392.6 | 236.9 | 35.2 | 11.16× | 6.74× | same |
| geometric mean (22 queries) | 632.9 | 312.4 | 282.8 | 2.24× | 1.10× |  |
| load (COPY, indexes, ANALYZE; s) | 118.2 | 125.2 | 26.1 |  |  |  |

Every result agrees with PostgreSQL's.  On column tables nucleus takes
1.10× PostgreSQL's time in the geometric mean and is faster on 12 of the
22 queries (Q7 0.26 s against 2.7 s, Q17 0.10 s against 0.48 s); on row
tables 2.24×.  Against the first run (`09a6d55`: 7.9× and 6.5×, a load
of 221 s) and the one before this (`04c93f4`: 3.57× and 2.55×; loads
146 s and 675 s): joins vectorized and in parallel, narrow rows through
them, index nested loops, semi and anti joins placed by estimate and
hashing the smaller side, joins on two-column keys estimated by the key,
statistics across columns (§M14.20 V12), constant comparison operands
folded and a column's range bounds estimated together, correlated
subqueries kept by the values they read, `char(n)` and dictionary
columns read a row at a time from column segments, parallel aggregates
and sorts, the buffer pool in shards, hardware CRC-32C, less LZ; loads
from write intents (V11: a large transaction's writes written once) and
a vacuum that waits for an idle database.

Where the time still goes:

- **Q19 and Q22** (9× and 7×): PostgreSQL answers them in 19 and 35 ms
  through indexes; nucleus still reads more than it must (Q19's lookups
  each read a part's lines and filter them; Q22 scans `orders` for its
  anti join).
- **Q13 and Q16** (3.5×): a left join and a grouping over most of
  `orders`, and an anti join with a `LIKE` filter; built a row at a time.
- **Row tables' scans** (Q3–Q6, Q10, Q12: 2.3–5×): a row's every column is
  decoded where a column table reads only the wanted ones.
- **Loads** (118 s and 125 s against 26 s): `lineitem`'s `COPY` 40 s,
  the indexes on it 20–30 s each.

## Targets

Set from this first run (PLAN.md §M14.11 asks for targets from the first
results): within 2× of PostgreSQL on loads and updates, within 3× on
scans, sorts and joins once §M14.20 lands, at least even on reads by key.
