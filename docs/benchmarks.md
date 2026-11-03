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
on `f95a6c3`; each query's best of three runs; nucleus with every table
in rows, and with all but `region` and `nation` in columns; PostgreSQL
18.6 in the same run:

| TPC-H SF 1 | nucleus (row) (ms) | nucleus (column) (ms) | postgresql (ms) | nucleus (row) / PostgreSQL | nucleus (column) / PostgreSQL | results |
|---|---:|---:|---:|---:|---:|---|
| Q1 | 499.4 | 184.5 | 989.5 | 0.50× | 0.19× | same |
| Q2 | 41.3 | 49.3 | 177.9 | 0.23× | 0.28× | same |
| Q3 | 520.7 | 133.5 | 289.2 | 1.80× | 0.46× | same |
| Q4 | 231.9 | 67.8 | 147.9 | 1.57× | 0.46× | same |
| Q5 | 836.4 | 333.6 | 423.0 | 1.98× | 0.79× | same |
| Q6 | 444.4 | 78.9 | 194.1 | 2.29× | 0.41× | same |
| Q7 | 669.8 | 206.1 | 2,751.7 | 0.24× | 0.07× | same |
| Q8 | 216.8 | 101.7 | 171.1 | 1.27× | 0.59× | same |
| Q9 | 1,462.3 | 948.0 | 1,370.3 | 1.07× | 0.69× | same |
| Q10 | 614.0 | 436.4 | 329.7 | 1.86× | 1.32× | same |
| Q11 | 179.3 | 39.9 | 40.6 | 4.41× | 0.98× | same |
| Q12 | 558.4 | 90.5 | 303.6 | 1.84× | 0.30× | same |
| Q13 | 182.7 | 98.9 | 337.4 | 0.54× | 0.29× | same |
| Q14 | 530.9 | 101.5 | 201.2 | 2.64× | 0.50× | same |
| Q15 | 474.5 | 70.0 | 204.5 | 2.32× | 0.34× | same |
| Q16 | 226.2 | 126.0 | 124.9 | 1.81× | 1.01× | same |
| Q17 | 42.0 | 75.6 | 461.7 | 0.09× | 0.16× | same |
| Q18 | 686.2 | 343.8 | 1,615.6 | 0.42× | 0.21× | same |
| Q19 | 43.2 | 55.4 | 17.8 | 2.42× | 3.11× | same |
| Q20 | 494.0 | 504.6 | 261.2 | 1.89× | 1.93× | same |
| Q21 | 908.3 | 494.9 | 1,561.0 | 0.58× | 0.32× | same |
| Q22 | 76.6 | 14.5 | 33.3 | 2.30× | 0.43× | same |
| geometric mean (22 queries) | 307.4 | 129.8 | 276.2 | 1.11× | 0.47× |  |
| load (COPY, indexes, ANALYZE; s) | 73.6 | 91.1 | 23.8 |  |  |  |

Every result agrees with PostgreSQL's.  On column tables nucleus takes
0.47× PostgreSQL's time in the geometric mean and is faster on 18 of the
22 queries (Q1 0.18 s against 0.99 s, Q7 0.21 s against 2.75 s, Q18 0.34
s against 1.62 s); slower on Q10 (1.32×), Q16 (1.01×), Q20 (1.93×) and
Q19 (3.11×, 55 ms against 18).  On row tables 1.11×, faster on 7.

Against the run before (on `14c3445`: 0.67× and 1.27×), the changes of
§M14.20 V13 (on column / row tables):

- Key filters (`keyfilter`): a hash join's narrowed right side hands
  its keys to the scans below its probe side, which skip segments and
  drop rows before the join.  Q5 528 → 334 ms, 1,010 → 836.
- Eager aggregation (`eager`): Q13's count of orders a customer
  grouped by `o_custkey` below the left join — 1.5 million rows
  through the join become 100,000 groups.  Q13 367 → 99 ms, 424 → 183.
- Aggregation tables: groups merged by hash part on the workers, those
  in key order finished there, compact partial states, a salted table
  hashing a batch's keys in place (`keytable`).  Q18 885 → 344 ms,
  1,334 → 686.
- Join tables on two integer keys hashed as a pair, a build side that
  is no pipeline gathered in columns rather than spilled as rows past
  work_mem.  Q9 2,437 → 948 ms, 2,950 → 1,462.
- Decimal vectors (`Vector::Dec`): one scale's mantissas, `+ - *` and
  comparisons on them, `sum` / `avg` straight from them — no value a
  row.  Q1 535 → 185 ms, 810 → 499; Q6 135 → 79; Q15 133 → 70; Q14
  119 → 102; Q3 154 → 134.
- Dictionary segments filtered on their entries (`DictFilter`): Q12
  110 → 91 ms, Q19 58 → 55.

The run before it, against the run on `6b22808` with the changes committed as
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

- **Q19** (3.11×): its ~14,000 `lineitem` rows are each fetched by key
  from the column table — the delta store, the segment directory and the
  deletion markers each a look at the tree, per row — on the workers,
  where PostgreSQL reads a heap tuple each (18 ms).
- **Q20** (1.93×) and **Q10** (1.32×): correlated and join-heavy plans
  whose groupings and lookups still run partly a row at a time.
- **Row tables** (1.11×): Q11 4.4×, Q14 2.6×, Q19 2.4×, Q6 2.3× the
  highest; a row's every column decoded where a column table reads only
  the wanted ones.
- **Loads** (74 s and 91 s against 24 s: 3.1× and 3.8×): `lineitem`'s
  `COPY` and the indexes on it.

The load since, phase by phase (§M14.20 V14; SF 1, row tables, the same
machine, PostgreSQL 18 beside it in the same session; full runs of the
suite come when §M14.20 closes):

| step | `COPY` | `CREATE INDEX` | `ANALYZE` | total (s) |
|---|---:|---:|---:|---:|
| profiled (`4cc9ff4`) | 27.6 | 40.3 | 11.5 | 79.4 |
| W1: Algorithm L, ordered syncs, 1 GiB checkpoints | 26.2 | | 5.5 | 69.8 |
| W2: one sorted index builder, parallel | 23.0 | 15.8 | 6.8 | 45.6 |
| W3: one row builder, parsing in parallel | 14.5 | 16.6 | 6.9 | 37.9 |
| W3, column tables | 17.9 | 12.1 | 6.4 | 36.4 |
| W4: checkpoints off the writer lock | 13.4 | 15.2 | 6.7 | 35.3 |
| W4, D16: a new index's entries without the WAL | 13.4 | 13.5 | 7.1 | 34.0 |
| … the same binary, `--wal full` | 14.0 | 17.0 | 7.3 | 38.2 |
| PostgreSQL 18 | 18.3 | 5.5 | 5.3 | 29.1 |

(`lineitem`'s `COPY` alone, W2 → W3: a row table 17.5 → 10.4 s, a
column table 36.5 → 12.8 s; PostgreSQL's 13.4 s.)

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
`f95a6c3`, as the TPC-H run above; each query's best of
three runs; nucleus with `hits` in rows and in columns; PostgreSQL 18.6
in the same run:

| ClickBench, 1,000,000 rows | nucleus (row) (ms) | nucleus (column) (ms) | postgresql (ms) | nucleus (row) / PostgreSQL | nucleus (column) / PostgreSQL | results |
|---|---:|---:|---:|---:|---:|---|
| Q0 | 210.4 | 2.8 | 70.3 | 2.99× | 0.04× | same |
| Q1 | 211.8 | 3.2 | 87.4 | 2.42× | 0.04× | same |
| Q2 | 207.8 | 3.1 | 77.4 | 2.68× | 0.04× | same |
| Q3 | 212.2 | 3.0 | 60.9 | 3.48× | 0.05× | same |
| Q4 | 1,389.7 | 591.8 | 206.8 | 6.72× | 2.86× | same |
| Q5 | 1,385.2 | 581.5 | 351.8 | 3.94× | 1.65× | same |
| Q6 | 213.7 | 14.3 | 70.6 | 3.03× | 0.20× | same |
| Q7 | 205.7 | 3.4 | 77.5 | 2.65× | 0.04× | same |
| Q8 | 1,453.0 | 656.9 | 345.4 | 4.21× | 1.90× | same |
| Q9 | 1,529.4 | 746.2 | 466.4 | 3.28× | 1.60× | same |
| Q10 | 246.1 | 21.1 | 76.5 | 3.22× | 0.28× | same |
| Q11 | 246.6 | 21.9 | 77.1 | 3.20× | 0.28× | differs (ties at the LIMIT) |
| Q12 | 217.1 | 15.4 | 90.4 | 2.40× | 0.17× | same |
| Q13 | 290.4 | 82.9 | 143.3 | 2.03× | 0.58× | same |
| Q14 | 219.3 | 16.6 | 247.8 | 0.89× | 0.07× | same |
| Q15 | 236.9 | 36.6 | 102.4 | 2.31× | 0.36× | same |
| Q16 | 263.7 | 58.2 | 292.4 | 0.90× | 0.20× | same |
| Q17 | 227.5 | 23.4 | 273.4 | 0.83× | 0.09× | differs (ties at the LIMIT) |
| Q18 | 2,104.0 | 1,046.5 | 339.7 | 6.19× | 3.08× | differs (ties at the LIMIT) |
| Q19 | 236.0 | 3.8 | 59.7 | 3.95× | 0.06× | same |
| Q20 | 207.2 | 7.2 | 97.8 | 2.12× | 0.07× | same |
| Q21 | 207.9 | 6.3 | 114.9 | 1.81× | 0.05× | same |
| Q22 | 242.8 | 11.9 | 142.7 | 1.70× | 0.08× | same rows (ties ordered differently) |
| Q23 | 225.3 | 138.0 | 102.6 | 2.20× | 1.34× | same |
| Q24 | 212.7 | 12.5 | 80.7 | 2.64× | 0.15× | same |
| Q25 | 215.2 | 13.4 | 75.4 | 2.86× | 0.18× | same |
| Q26 | 214.9 | 15.4 | 75.4 | 2.85× | 0.20× | same |
| Q27 | 236.9 | 39.3 | 140.9 | 1.68× | 0.28× | same |
| Q28 | 400.5 | 235.7 | 2,918.7 | 0.14× | 0.08× | differs (ties at the LIMIT) |
| Q29 | 451.0 | 291.9 | 392.2 | 1.15× | 0.74× | same |
| Q30 | 221.8 | 22.9 | 122.3 | 1.81× | 0.19× | differs (ties at the LIMIT) |
| Q31 | 249.2 | 53.6 | 125.4 | 1.99× | 0.43× | differs (ties at the LIMIT) |
| Q32 | 3,966.2 | 2,937.3 | 934.8 | 4.24× | 3.14× | differs (ties at the LIMIT) |
| Q33 | 1,533.9 | 436.3 | 541.5 | 2.83× | 0.81× | same |
| Q34 | 1,650.4 | 546.9 | 563.4 | 2.93× | 0.97× | same |
| Q35 | 258.3 | 62.9 | 138.6 | 1.86× | 0.45× | same rows (ties ordered differently) |
| Q36 | 300.8 | 99.0 | 260.3 | 1.16× | 0.38× | same |
| Q37 | 225.5 | 24.6 | 162.7 | 1.39× | 0.15× | same |
| Q38 | 212.0 | 10.1 | 132.3 | 1.60× | 0.08× | differs (ties at the LIMIT) |
| Q39 | 2,298.8 | 1,167.1 | 535.5 | 4.29× | 2.18× | differs (ties at the LIMIT) |
| Q40 | 216.7 | 15.4 | 206.7 | 1.05× | 0.07× | differs (ties at the LIMIT) |
| Q41 | 211.6 | 9.3 | 200.2 | 1.06× | 0.05× | same |
| Q42 | 217.3 | 18.8 | 150.1 | 1.45× | 0.13× | same |
| geometric mean (43 queries) | 361.7 | 41.3 | 170.0 | 2.13× | 0.24× |  |
| load (COPY, ANALYZE; s) | 17.3 | 19.6 | 7.3 |  |  |  |

Every result agrees, but for ties at a `LIMIT` (the same rows where the
order is total).  On a column table nucleus takes a quarter of
PostgreSQL's time in the geometric mean (0.24×) and is faster on 35 of
the 43 queries; the counts and aggregates over a few columns (Q0–Q3,
Q7, Q19) take 3–4 ms against 60–87.  Slower: the distinct counts (Q4,
Q5, Q8, Q9: 1.6–2.9× — `count(DISTINCT …)` is not vectorized yet),
the groupings over large text keys (Q18 3.1×, Q32 3.1×, Q39 2.2×) and
Q23 (`SELECT *` … `ORDER BY` a large text: 1.3×).  On a row table
2.13×: every query decodes each row's 105 columns.  The loads 17.3 s
and 19.6 s against 7.3 s (2.4× and 2.7×).

Against the run before (on `14c3445`: 0.31× and 2.31×), the changes of
the TPC-H run above: the groupings of many groups — merged by hash part
on the workers, keys hashed and compared in place in a salted table —
Q33 997 → 436 ms, Q34 992 → 547, Q36 275 → 99, Q31 104 → 54, Q35 123 →
63, Q16 106 → 58, Q17 77 → 23, Q18 1,444 → 1,047, Q39 1,709 → 1,167;
dictionary filters Q12 24 → 15, Q14 28 → 17, Q30 38 → 23; on the row
table the same groupings about a quarter faster (Q33 2,150 → 1,534).
That run, against the one on `6b22808` (0.32× and 2.32×): the same
within the variation.  The one before it, against the run at
`e2d1afa` (0.34× and 2.47×; loads 18.7 s and 20.9 s against 7.2 s):
the row table's queries about a tenth faster (its `char(n)` columns a
vector now), the column table's within the variation but for Q23, 220 →
146 ms (the cache of decoded columns no longer evicting what
`SELECT *` comes back to — not measured apart).

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
| TPC-H SF 1, column tables: geometric mean ≤ 0.8× PostgreSQL's time | 0.47× (0.67× in the run before) | yes |
| ClickBench 1M rows, column table: geometric mean ≤ 0.5× | 0.24× (0.31×) | yes |
| column tables: no TPC-H or ClickBench query above 3× | TPC-H Q19 3.1×; ClickBench Q32 3.1×, Q18 3.1× (Q4 2.9×, Q39 2.2×; TPC-H Q20 1.9×) | no |
| row tables: geometric mean ≤ 2× (TPC-H, ClickBench) | TPC-H 1.11× (1.27×), ClickBench 2.13× (2.31×) | TPC-H yes, ClickBench no |
| loads ≤ 2× (`bench/run.py`, TPC-H, ClickBench) | 0.9×; TPC-H 3.1× (row), 3.8× (column); ClickBench 2.4×, 2.7× | `bench/run.py` yes, the others no |
| updates and deletes ≤ 2× (`bench/run.py`) | update 0.9×, delete 1.3× | yes |
| scans, aggregates, sorts and joins ≤ 3× (`bench/run.py`) | 1.0–2.0× (sort 2.0×, count(*) 1.7×, self-join 1.6×) | yes |
| reads by key ≤ 1× (`bench/run.py`) | 0.6× | yes |

Ratios are best-of-three times on one machine in one run; the CI's
shared runners are too noisy to fail on them yet, so a target is checked
here, by a local run, when §M14.20 is closed.

