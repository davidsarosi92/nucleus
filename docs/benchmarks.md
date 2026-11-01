# Benchmarks

nucleus against PostgreSQL on the same machine and data (PLAN.md §M14.11
L8).  The suite is `bench/run.py`: one workload — a 4-column table loaded
by `INSERT … SELECT`, counts, a filtered scan, a sum, a 1000-group
`GROUP BY`, a sort, a self-join, 1000 reads by key, an update of a quarter
of the rows and a delete of a tenth — run statement by statement through
each system's own client with `\timing`, both as client and server:
nucleus as `nucleus server` driven by `nucleus sql -h` over its own
protocol, `ncp`; PostgreSQL, the baseline, through `psql`.  nucleus's
PostgreSQL protocol (`--pg-compat`) is not used.  A step repeated three
times reports its best; the point reads their total.

CI runs it on every push to `main` with a million rows against the
`postgres:18` image (job **benchmarks against PostgreSQL**; the table is
in the job summary, the numbers in the `bench` artifact).  It fails only
when a run fails: shared runners are too noisy for thresholds yet.

## Reproduce

```sh
cargo build --release -p nucleus-cli
# nucleus: --nucleus names the binary; the script starts its server on a
# fresh directory and drives it with `nucleus sql -h`.
# PostgreSQL (the baseline): psql, reached through the usual PG* variables
# (or --pg "…"); they concern only PostgreSQL.
PGHOST=127.0.0.1 PGUSER=postgres python3 bench/run.py \
  --nucleus target/release/nucleus --rows 1000000 --json bench.json
python3 bench/run.py --nucleus target/release/nucleus --no-pg   # nucleus alone
```

## Local runs

Apple M2 Pro (macOS, 12 cores), nucleus release builds; PostgreSQL 18 in
Docker (Linux VM, 10 CPUs) with its defaults, parallel workers included.

**Latest** (2026-11-01, nucleus on `6b22808` with the vectorized
`char(n)`, `IN` and `substr`, the parallel index nested loops and the
buffer pool's read-locked hits — TPC-H below; PostgreSQL 18.6 in the same
session; one run each, each step's best of its repeats):

| 1,000,000 rows | nucleus (ms) | PostgreSQL (ms) | ratio |
|---|---:|---:|---:|
| load (INSERT … SELECT) | 1,040.1 | 1,168.2 | 0.9× |
| count(*) | 21.8 | 13.1 | 1.7× |
| filtered count | 21.3 | 15.8 | 1.3× |
| sum | 20.3 | 14.5 | 1.4× |
| group by (1000 groups) | 33.3 | 32.4 | 1.0× |
| sort | 295.4 | 147.1 | 2.0× |
| self-join | 182.8 | 113.6 | 1.6× |
| point reads (1000 by key) | 62.7 | 104.1 | 0.6× |
| update 25% | 685.9 | 776.1 | 0.9× |
| delete 10% | 126.1 | 99.9 | 1.3× |
| count after writes | 33.7 | 18.8 | 1.8× |

Against the run before (`e2d1afa`, below): the scans somewhat faster
(`count(*)` 28.6 → 21.8 ms, the sum 23.8 → 20.3 — the parallel scans'
workers no longer queue on a buffer pool shard's lock for the pages they
share, the likely cause, not measured apart), the rest within the
run-to-run variation; every target still met.

How it got there, oldest first.  2026-10-31, nucleus at `3043e69`:

| 1,000,000 rows | nucleus (ms) | PostgreSQL (ms) | ratio | nucleus on 2026-10-29 (ms) |
|---|---:|---:|---:|---:|
| load (INSERT … SELECT) | 3,144.4 | 960.9 | 3.3× | 6,833.1 |
| count(*) | 26.5 | 12.8 | 2.1× | 27.2 |
| filtered count | 26.5 | 15.9 | 1.7× | 27.2 |
| sum | 24.3 | 14.5 | 1.7× | 26.7 |
| group by (1000 groups) | 37.9 | 32.4 | 1.2× | 37.0 |
| sort | 298.6 | 154.8 | 1.9× | 910.0 |
| self-join | 442.1 | 116.1 | 3.8× | 1,724.5 |
| point reads (1000 by key) | 66.8 | 121.7 | 0.5× | 101.5 |
| update 25% | 1,867.1 | 839.7 | 2.2× | 4,503.0 |
| delete 10% | 797.5 | 106.3 | 7.5× | 3,670.4 |
| count after writes | 42.3 | 19.3 | 2.2× | 126.2 |

(Writes vary by a factor of two from run to run on this machine, both
systems' — Docker's file system and the laptop's disk; PostgreSQL's were
6–7× faster here than on 10-29.  Compare them within one run.)

What the numbers said then: scans and aggregates are 1.2–2.1× PostgreSQL's
time on a row table (they were 15–30× at `b03c130`), reads by key twice
as fast.  Sorts and joins went parallel and vectorized since 10-29
(3–4× faster); the self-join still builds its table a row at a time.
Writes are the gap: a delete pays for a lookup per row in the engine's
conflict and garbage checks, a load and an update for encoding and
writing each row's versions.

**Writes since** (2026-11-01, nucleus alone; the same machine, the ratio
against PostgreSQL's times above): `UPDATE` and `DELETE` find their rows
by the parallel vectorized scan, a write's early conflict check no longer
reads the tree, a commit's checks walk one cursor, and a commit's leaves
are merged and encoded without being decoded; then less per row — stashed
rows consumed rather than copied, keys encoded in one buffer, a commit's
batch sorted by the engine and applied as it is; then a commit's leaves
merged on the CPUs; then garbage dropped from the leaves a commit rewrites
(a compaction filter) instead of a walk over its keys' versions, the
commit's conflict check answered by the recent commits' keys:

| 1,000,000 rows | before (ms) | now (ms) | ratio then → now |
|---|---:|---:|---:|
| load (INSERT … SELECT) | 3,193 | 2,240–2,520 | 3.3× → 2.3–2.6× |
| update 25% | 1,896 | 910–1,020 | 2.2× → 1.1–1.2× |
| delete 10% | 827 | 220–230 | 7.5× → 2.1–2.2× |

What the delete's ~225 ms is now (a profile): finding the rows (the
parallel scan) about 20%, the per-row write (the write set, key
encoding) about 17%, the commit about half — applying its batch to the
tree (the leaves merged and filtered on the workers) about 20%, the rest
building and sorting the batch, the WAL record and the recent keys — and
the WAL's sync a few percent.

Hot updates (100,000 rows, each updated 20 times, a statement each): the
commits' collection by walking each key's versions took 6.6 s and left a
32 MB table; the compaction filter 5.5 s and 28 MB, a full scan 3.7 ms
after either.  Without any collection at commit (vacuum's alone, the
alternative weighed) it was 8.4 s, 311 MB and a 78 ms scan.

**Against PostgreSQL again** (2026-11-01; PostgreSQL 18.6 in the same
container, its `psql` beside it; three runs of each side, the median, in
the same session): PostgreSQL's own writes came out faster than on 10-31
(the delete 72–74 ms, not 106; the update 744–758, not 840), so the
ratios of the writes are higher than the ones above, measured against
10-31's times.  First at `a6d85e0` (garbage collected by the compaction
filter), then with the write set a sorted vector and a large commit's
keys one run among the recent commits' (per written row a push and one
comparison, where it was two tree lookups and two inserts):

| 1,000,000 rows | nucleus `a6d85e0` (ms) | nucleus now (ms) | PostgreSQL (ms) | ratio now |
|---|---:|---:|---:|---:|
| load (INSERT … SELECT) | 2,335 | 2,045 | 1,015 | 2.0× |
| count(*) | 26 | 26 | 13 | 2.0× |
| filtered count | 26 | 25 | 16 | 1.5× |
| sum | 24 | 25 | 15 | 1.6× |
| group by (1000 groups) | 37 | 37 | 34 | 1.1× |
| sort | 317 | 298 | 151 | 2.0× |
| self-join | 453 | 489 | 115 | 4.3× |
| point reads (1000 by key) | 59 | 57 | 108 | 0.5× |
| update 25% | 992 | 782 | 758 | 1.0× |
| delete 10% | 225 | 160 | 74 | 2.2× |
| count after writes | 32 | 36 | 19 | 1.9× |

The hot updates above (100,000 rows, 20 times): 5.5 → 4.6 s.

Then a `DELETE` from a table that is not partitioned no longer stashes
its rows a second time (to put the partition's number before each): the
delete 139–147 ms against PostgreSQL's 70–74 (1.96×, the median), the
update 818 against 726 (1.1×), the load 1,995 against 1,018 (1.96×).
Where the delete's time goes now (timed by phase, 1M rows, 100,000
deleted): finding and stashing the rows ~57 ms (the filtered scan alone
~35), writing them ~19, the commit ~70 — of which applying the batch to
the tree ~50 (14,768 leaves: found 5, prepared on the workers 18–21,
installed one by one with the internal nodes ~17, a window's workers and
its installs not yet overlapping) and the WAL's sync ~10.

Then the workers prepare leaves for the whole batch, a window ahead of
the walk installing them, and a prepared page is swapped into the
pool's frame rather than copied: the walk ~24 ms, never waiting long.
Against PostgreSQL in the same run (three each, the median): the delete
137 ms (130–143) against 106 (75–107: PostgreSQL's own delete varied in
this run; 70–76 in the runs before), 1.3× — 1.8× against its typical
time; the update 760 against 782 (1.0×); the load 1,991 against 920
(2.2×, PostgreSQL's load fast in this run: 909–1,162).

Then the load (profiled: per row, a look at the tree from the root to
learn the key is new ~19%; the spill's look at each key ~7%; the commit
of the spilled transaction deleting an intent for each of the ~420,000
keys written after the spill, which had none; per-row allocations in the
SQL layer): a run of new keys found by one look, the spill's keys checked
from the recent commits' keys, intents deleted only inside the spilled
span, the commit's ops built in order, fewer allocations per row.
Against PostgreSQL (two runs each, alternating, the best):

| 1,000,000 rows | nucleus `cb18578` (ms) | nucleus now (ms) | PostgreSQL (ms) | ratio now |
|---|---:|---:|---:|---:|
| load (INSERT … SELECT) | 1,991 | 1,253 | 888 (888–1,073) | 1.4× (1.2–1.4×) |
| update 25% | 760 | 680 | 734 | 0.9× |
| delete 10% | 137 | 127 | 74 (74–122) | 1.7× |
| self-join | 489 | 476 | 113 | 4.2× |

The rest as before (count 2.1×, sort 2.0×, reads by key 0.6×).

Then the self-join (profiled: its right side — a million rows — scanned
and stashed on one thread ~40% of its time, its table built on one
thread with a vector allocated per key ~19%, those vectors freed ~8%; the
probe already on the workers): the right side collected by the workers,
morsels of its scan in key order, its keys evaluated a batch at a time;
the table each key's row numbers side by side in one array.  Against
PostgreSQL (two runs each, alternating, the best; PostgreSQL's plan a
parallel hash join on three processes):

| 1,000,000 rows | nucleus `76d7cfb` (ms) | nucleus now (ms) | PostgreSQL (ms) | ratio now |
|---|---:|---:|---:|---:|
| self-join | 476 | 189 | 112 | 1.7× |

The whole run at `e2d1afa` (nucleus three runs, PostgreSQL two, each
step's best; the rest within the run-to-run noise of the runs above):

| 1,000,000 rows | nucleus (ms) | PostgreSQL (ms) | ratio |
|---|---:|---:|---:|
| load (INSERT … SELECT) | 1,111 | 936 | 1.2× |
| count(*) | 28.6 | 12.5 | 2.3× |
| filtered count | 25.3 | 15.2 | 1.7× |
| sum | 23.8 | 14.9 | 1.6× |
| group by (1000 groups) | 34.6 | 33.2 | 1.0× |
| sort | 294 | 154 | 1.9× |
| self-join | 189 | 112 | 1.7× |
| point reads (1000 by key) | 56.5 | 96.9 | 0.6× |
| update 25% | 733 | 719 | 1.0× |
| delete 10% | 135 | 74.6 | 1.8× |
| count after writes | 36.2 | 20.1 | 1.8× |

Then the changes for TPC-H's joins and filters (below): the **Latest**
table at the top of this section.

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
results compared with PostgreSQL's (numbers to cents).  nucleus runs as
its own server, driven by `nucleus sql -h` (loads by its `\copy`) and
read back by the Python driver; `--psql` gives only the command that
reaches PostgreSQL, the baseline (here in Docker; `--no-pg` leaves it
out).

```sh
python3 bench/tpch.py --nucleus target/release/nucleus --sf 1 \
  --psql "docker exec -i pg psql -U postgres"
```

2026-11-01, SF 1 (6 million `lineitem` rows), the machine above, nucleus
on `14c3445` with the changes below; each query's best of three runs;
nucleus with every table in rows, and with all but `region` and `nation`
in columns; PostgreSQL 18.6 in the same run:

| TPC-H SF 1 | nucleus (row) (ms) | nucleus (column) (ms) | postgresql (ms) | nucleus (row) / PostgreSQL | nucleus (column) / PostgreSQL | results |
|---|---:|---:|---:|---:|---:|---|
| Q1 | 809.5 | 534.7 | 995.5 | 0.81× | 0.54× | same |
| Q2 | 40.6 | 50.2 | 167.5 | 0.24× | 0.30× | same |
| Q3 | 468.8 | 153.6 | 292.9 | 1.60× | 0.52× | same |
| Q4 | 223.9 | 69.8 | 146.3 | 1.53× | 0.48× | same |
| Q5 | 1,010.0 | 527.7 | 442.0 | 2.29× | 1.19× | same |
| Q6 | 482.5 | 135.3 | 194.5 | 2.48× | 0.70× | same |
| Q7 | 665.1 | 234.1 | 2,821.7 | 0.24× | 0.08× | same |
| Q8 | 217.8 | 135.7 | 176.9 | 1.23× | 0.77× | same |
| Q9 | 2,950.3 | 2,436.7 | 1,345.3 | 2.19× | 1.81× | same |
| Q10 | 604.3 | 460.2 | 321.5 | 1.88× | 1.43× | same |
| Q11 | 179.0 | 50.8 | 32.8 | 5.45× | 1.55× | same |
| Q12 | 504.2 | 109.8 | 303.0 | 1.66× | 0.36× | same |
| Q13 | 424.1 | 367.0 | 335.9 | 1.26× | 1.09× | same |
| Q14 | 458.7 | 119.0 | 197.3 | 2.33× | 0.60× | same |
| Q15 | 498.6 | 133.3 | 205.9 | 2.42× | 0.65× | same |
| Q16 | 224.6 | 124.9 | 125.3 | 1.79× | 1.00× | same |
| Q17 | 40.2 | 81.4 | 454.4 | 0.09× | 0.18× | same |
| Q18 | 1,334.1 | 885.3 | 1,615.0 | 0.83× | 0.55× | same |
| Q19 | 45.1 | 58.0 | 17.5 | 2.57× | 3.31× | same |
| Q20 | 511.5 | 546.1 | 232.7 | 2.20× | 2.35× | same |
| Q21 | 997.9 | 501.8 | 1,593.3 | 0.63× | 0.31× | same |
| Q22 | 67.0 | 15.7 | 34.4 | 1.95× | 0.45× | same |
| geometric mean (22 queries) | 345.6 | 182.1 | 272.2 | 1.27× | 0.67× |  |
| load (COPY, indexes, ANALYZE; s) | 75.7 | 94.0 | 27.1 |  |  |  |

Every result agrees with PostgreSQL's.  On column tables nucleus takes
0.67× PostgreSQL's time in the geometric mean and is faster on 14 of the
22 queries (Q7 0.23 s against 2.82 s, Q17 0.08 s against 0.45 s), even
on Q16; on row tables 1.27×, faster on 6.  One run each: the row tables'
times varied by whole queries between runs at `e2d1afa` (Q1 and Q3 1.7
and 3.2 s in one, 1.0 and 1.2 s in the next, Q21 2.9 and 5.6 s), not
explained then and not looked at again since.

Against the run before (on `6b22808` with the changes committed as
`14c3445`: 0.75× and 1.40×; Q13 1,273 / 1,467 ms on column / row
tables, Q16 420 / 379):

- Q13's left join hashes `orders` (1.48 million rows past its `LIKE`
  filter): as rows its table passed work_mem (64 MB) and spilled, a
  grace hash join.  The table the workers make holds its rows a column
  at a time now, the keys too (`columns::Columns`: an integer 9 bytes,
  not a `Value`'s 32): about 30 MB, in memory.  `LIKE` with `%` inside
  the pattern is a kernel (`'%special%requests%'`: its parts found in
  order; it was a row at a time, the string copied into characters);
  substrings are searched for by their first and last bytes, 32 places
  at a time.  Q13 1,273 → 367 ms (1.09×) on column tables, 1,467 → 424
  on row tables.
- Q16: `NOT LIKE` was estimated as `LIKE` (a tenth of the rows), so
  `part`'s filter 3,072 rows where 29,412 come, and an index nested loop
  into `partsupp`; 0.9 now, a hash join.  An uncorrelated
  `IN (subquery)` is a list kernel (a hash set past eight items; it was
  a row at a time over 800,000 rows), and an uncorrelated subquery in a
  lookup's or a parallel scan's filters runs once on the statement's
  thread, its rows given to the workers (before, it kept the work on one
  thread).  Q16 420 → 125 ms (1.00×), 379 → 225 on row tables; Q22
  32 → 16 ms (its scan on the workers now).
- Q19 56 → 58 ms against PostgreSQL's 19 → 17.5: 3.31×, the one query
  above 3×.

The run before it, against the run at `e2d1afa` (2.34× and 1.03×; the
queries' times there: Q3 3,223 / 174 ms, Q19 127 / 151, Q22 404 / 231,
Q21 2,930 / 1,104):

- `char(n)` columns are a vector of their own, from row and column
  storage alike, compared, matched by `LIKE` and cut by `substr` a batch
  at a time (they were a value per row, a string allocated for each);
  `IN` lists of constants are a kernel, other lists evaluated over their
  operand and items only (before, a row at a time with every column of
  the row copied); scalar functions may give a batch kernel
  (`ScalarFunction::call_batch`, `substr` the first); an uncorrelated
  subquery in a filter is evaluated once.  On a column table Q22's
  `substring(c_phone …) IN (…)` over `customer` 50 → 2 ms (timed
  alone), Q19's filter
  on `part` 15 → 3 ms, on `lineitem` (a full scan) 580 → 100 ms.
- Index nested loops run on worker threads (blocks of left rows looked
  up in pieces, the rows in a single pass's order); the buffer pool's
  hits take their shard's lock only to read (the workers' lookups all
  descend from the same roots, which serialized them on one lock), and
  the column store's cache of decoded columns is in shards read-locked
  the same way.  Q19 on a column table, timed alone after the first
  change: 125 → 56 ms (one worker 150 ms, two 104, four 71, ten 54).
- Q22 on column tables 231 → 32 ms (1.00×), Q19 151 → 56 ms (2.95×);
  on row tables Q3 3,223 → 482 ms, Q21 2,930 → 945 ms.  Loads 78 s /
  92 s against PostgreSQL's 25 s (98 s / 111 s at `e2d1afa`).
- The cache's shards first had each its share of the bound (16 MB): a
  shard the hash gave more columns than that evicted while the cache as a
  whole had room — ClickBench's Q22 on a column table took 72 ms (21 ms
  before), Q23 266 ms (220).  The bound is all shards' together now, and
  eviction takes the columns used longest ago in any shard: Q22 21 ms,
  Q23 146 ms.

At `e2d1afa`, against the run before it (`940395c`, 2026-10-30: 2.24×
and 1.10×, loads 118 s and 125 s), the loads were faster (the large transaction's look at
the tree once per run of new keys, the commit's leaves prepared on the
workers) and the column tables' joins (the hash join's right side
collected on the workers).  Against the first run (`09a6d55`: 7.9× and
6.5×, a load of 221 s) and the one after it (`04c93f4`: 3.57× and
2.55×; loads 146 s and 675 s): joins vectorized and in parallel, narrow rows through
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

- **Q13** (1.09×): the join's 1.5 million rows come out as rows, and
  the grouping over them is a row at a time.
- **Q19** (3.31×): its ~14,000 `lineitem` rows are each fetched by key
  from the column table — the delta store, the segment directory and the
  deletion markers each a look at the tree, per row — on the workers now,
  where PostgreSQL reads a heap tuple each (19 ms).
- **Row tables** (1.27×): Q11 5.5×, Q19 2.6×, Q6 2.5× the highest;
  a row's every column decoded where a column table reads only the wanted
  ones.
- **Loads** (76 s and 94 s against 27 s: 2.8× and 3.5×): on 10-30
  `lineitem`'s `COPY` was 40 s, the indexes on it 20–30 s each.

## ClickBench

`bench/clickbench.py` (§M14.20 V10): ClickBench's `hits` table (its
PostgreSQL schema) and 43 queries, from the ClickBench repository at a
fixed commit, on the first `--rows` rows of its data, each system's
results checked against the other's (a query whose `LIMIT` may cut ties
differently is reported, not failed).  nucleus and PostgreSQL are driven
as for TPC-H: nucleus through its own client and driver, PostgreSQL
through the `--psql` command.

```sh
python3 bench/clickbench.py --nucleus target/release/nucleus --rows 1000000 \
  --psql "docker exec -i pg psql -U postgres"
```

2026-11-01, the first million rows, the machine above, nucleus on
`14c3445` with the changes of the TPC-H run above; each query's best of
three runs; nucleus with `hits` in rows and in columns; PostgreSQL 18.6
in the same run:

| ClickBench, 1,000,000 rows | nucleus (row) (ms) | nucleus (column) (ms) | postgresql (ms) | nucleus (row) / PostgreSQL | nucleus (column) / PostgreSQL | results |
|---|---:|---:|---:|---:|---:|---|
| Q0 | 203.2 | 2.8 | 61.0 | 3.33× | 0.05× | same |
| Q1 | 205.1 | 3.2 | 71.8 | 2.86× | 0.04× | same |
| Q2 | 251.7 | 3.4 | 78.5 | 3.20× | 0.04× | same |
| Q3 | 207.1 | 3.2 | 60.1 | 3.45× | 0.05× | same |
| Q4 | 1,432.0 | 591.8 | 202.2 | 7.08× | 2.93× | same |
| Q5 | 1,377.4 | 575.5 | 346.9 | 3.97× | 1.66× | same |
| Q6 | 216.3 | 13.9 | 61.5 | 3.51× | 0.23× | same |
| Q7 | 204.8 | 3.4 | 77.4 | 2.65× | 0.04× | same |
| Q8 | 1,460.6 | 671.9 | 324.5 | 4.50× | 2.07× | same |
| Q9 | 1,555.5 | 747.0 | 442.2 | 3.52× | 1.69× | same |
| Q10 | 235.1 | 18.7 | 75.5 | 3.11× | 0.25× | same |
| Q11 | 240.2 | 20.3 | 77.6 | 3.09× | 0.26× | differs (ties at the LIMIT) |
| Q12 | 231.4 | 24.3 | 90.6 | 2.56× | 0.27× | same |
| Q13 | 300.1 | 82.2 | 140.8 | 2.13× | 0.58× | same |
| Q14 | 253.8 | 28.4 | 232.3 | 1.09× | 0.12× | same |
| Q15 | 309.5 | 56.3 | 102.9 | 3.01× | 0.55× | same |
| Q16 | 394.3 | 106.4 | 290.3 | 1.36× | 0.37× | same |
| Q17 | 289.8 | 77.4 | 275.7 | 1.05× | 0.28× | differs (ties at the LIMIT) |
| Q18 | 2,639.0 | 1,444.3 | 342.1 | 7.72× | 4.22× | differs (ties at the LIMIT) |
| Q19 | 224.6 | 3.8 | 60.7 | 3.70× | 0.06× | same |
| Q20 | 212.7 | 7.0 | 98.0 | 2.17× | 0.07× | same |
| Q21 | 210.5 | 7.5 | 114.0 | 1.85× | 0.07× | same |
| Q22 | 233.4 | 14.7 | 143.1 | 1.63× | 0.10× | same rows (ties ordered differently) |
| Q23 | 219.7 | 146.5 | 99.8 | 2.20× | 1.47× | same rows (ties ordered differently) |
| Q24 | 214.0 | 12.1 | 84.0 | 2.55× | 0.14× | same |
| Q25 | 213.9 | 12.7 | 76.4 | 2.80× | 0.17× | same |
| Q26 | 216.4 | 14.3 | 78.0 | 2.77× | 0.18× | same |
| Q27 | 239.3 | 46.7 | 142.6 | 1.68× | 0.33× | same |
| Q28 | 440.8 | 252.0 | 2,913.3 | 0.15× | 0.09× | differs (ties at the LIMIT) |
| Q29 | 456.1 | 286.2 | 393.2 | 1.16× | 0.73× | same |
| Q30 | 240.0 | 37.6 | 233.4 | 1.03× | 0.16× | differs (ties at the LIMIT) |
| Q31 | 323.7 | 104.2 | 128.0 | 2.53× | 0.81× | differs (ties at the LIMIT) |
| Q32 | 3,970.8 | 2,916.1 | 983.9 | 4.04× | 2.96× | differs (ties at the LIMIT) |
| Q33 | 2,150.3 | 997.4 | 580.7 | 3.70× | 1.72× | same |
| Q34 | 2,105.1 | 992.0 | 565.3 | 3.72× | 1.75× | same |
| Q35 | 326.5 | 122.5 | 141.8 | 2.30× | 0.86× | same rows (ties ordered differently) |
| Q36 | 495.4 | 275.2 | 256.9 | 1.93× | 1.07× | same |
| Q37 | 248.9 | 43.4 | 161.1 | 1.54× | 0.27× | same |
| Q38 | 219.8 | 14.7 | 131.7 | 1.67× | 0.11× | differs (ties at the LIMIT) |
| Q39 | 2,873.3 | 1,708.5 | 540.2 | 5.32× | 3.16× | differs (ties at the LIMIT) |
| Q40 | 234.7 | 23.6 | 205.4 | 1.14× | 0.11× | differs (ties at the LIMIT) |
| Q41 | 214.4 | 12.7 | 200.1 | 1.07× | 0.06× | same |
| Q42 | 231.2 | 22.0 | 147.3 | 1.57× | 0.15× | same |
| geometric mean (43 queries) | 394.1 | 52.6 | 170.6 | 2.31× | 0.31× |  |
| load (COPY, ANALYZE; s) | 17.5 | 19.8 | 7.6 |  |  |  |

Every result agrees, but for ties at a `LIMIT` (the same rows where the
order is total).  On a column table nucleus takes a third of
PostgreSQL's time in the geometric mean (0.31×) and is faster on 32 of
the 43 queries; the counts and aggregates over a few columns (Q0–Q3,
Q7, Q19) take 3–4 ms against 60–79.  Slower: the distinct counts and
groupings over large text keys (Q4, Q5, Q8, Q9: 1.7–2.9×; Q18, Q39:
3.2–4.2×), `SearchPhrase` groupings (Q32–Q34: 1.8–3.0×) and Q23
(`SELECT *` … `ORDER BY` a large text: 1.5×).  On a row table 2.31×:
every query decodes each row's 105 columns.  The loads 17.5 s and
19.8 s against 7.6 s (2.3× and 2.6×).  Against the run before (on
`6b22808`, 0.32× and 2.32×) the same within the variation: the TPC-H
changes above touch few of these queries.  That run, against the run at `e2d1afa`
(0.34× and 2.47×; loads 18.7 s and 20.9 s against 7.2 s): the row
table's queries about a tenth faster (its `char(n)` columns a vector
now), the column table's within the variation but for Q23, 220 → 146 ms
(the cache of decoded columns no longer evicting what `SELECT *` comes
back to — not measured apart).

## Targets

PLAN.md §M14.11 asks for targets set from the first results, and
§M14.20 is done when analytical queries beat PostgreSQL by them and
transactional workloads stay within their margin.  First set from the
first `bench/run.py` run (`79506d4`, 2026-10-27) — within 2× of PostgreSQL on loads
and updates, within 3× on scans, sorts and joins, at least even on reads
by key —, now recorded from the runs above (2026-11-01), on this
machine against PostgreSQL 18 in the same run:

| target | now | met |
|---|---|---|
| TPC-H SF 1, column tables: geometric mean ≤ 0.8× PostgreSQL's time | 0.67× (0.75× in the run before) | yes |
| ClickBench 1M rows, column table: geometric mean ≤ 0.5× | 0.31× | yes |
| column tables: no TPC-H or ClickBench query above 3× | TPC-H Q19 3.3×; ClickBench Q18 4.2×, Q39 3.2× (Q32 3.0×, Q4 2.9×; TPC-H Q13 1.1×, Q16 1.0×) | no |
| row tables: geometric mean ≤ 2× (TPC-H, ClickBench) | TPC-H 1.27× (1.40×), ClickBench 2.31× | TPC-H yes, ClickBench no |
| loads ≤ 2× (`bench/run.py`, TPC-H, ClickBench) | 0.9×; TPC-H 2.8× (row), 3.5× (column); ClickBench 2.3×, 2.6× | `bench/run.py` yes, the others no |
| updates and deletes ≤ 2× (`bench/run.py`) | update 0.9×, delete 1.3× | yes |
| scans, aggregates, sorts and joins ≤ 3× (`bench/run.py`) | 1.0–2.0× (sort 2.0×, count(*) 1.7×, self-join 1.6×) | yes |
| reads by key ≤ 1× (`bench/run.py`) | 0.6× | yes |

Ratios are best-of-three times on one machine in one run; the CI's
shared runners are too noisy to fail on them yet, so a target is checked
here, by a local run, when §M14.20 is closed.

