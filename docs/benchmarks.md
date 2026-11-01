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

**Latest** (2026-11-01, nucleus at `e2d1afa`, PostgreSQL 18.6 in the
same session; nucleus three runs, PostgreSQL two, each step's best):

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

The whole run is the **Latest** table at the top of this section; the
rest within the run-to-run noise of the runs above.

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

2026-11-01, SF 1 (6 million `lineitem` rows), the machine above, at
`e2d1afa`; each query's best of three runs; nucleus with every table in
rows, and with all but `region` and `nation` in columns; PostgreSQL 18.6
in the same run:

| TPC-H SF 1 | nucleus (row) (ms) | nucleus (column) (ms) | postgresql (ms) | nucleus (row) / PostgreSQL | nucleus (column) / PostgreSQL | results |
|---|---:|---:|---:|---:|---:|---|
| Q1 | 1,680.2 | 828.4 | 1,005.7 | 1.67× | 0.82× | same |
| Q2 | 46.7 | 54.4 | 194.9 | 0.24× | 0.28× | same |
| Q3 | 3,223.0 | 174.0 | 301.2 | 10.70× | 0.58× | same |
| Q4 | 551.5 | 93.0 | 147.0 | 3.75× | 0.63× | same |
| Q5 | 2,264.3 | 536.2 | 472.5 | 4.79× | 1.13× | same |
| Q6 | 573.2 | 156.8 | 197.3 | 2.91× | 0.79× | same |
| Q7 | 2,048.6 | 247.1 | 2,900.2 | 0.71× | 0.09× | same |
| Q8 | 619.3 | 157.3 | 177.9 | 3.48× | 0.88× | same |
| Q9 | 3,000.8 | 2,517.2 | 1,536.0 | 1.95× | 1.64× | same |
| Q10 | 1,107.1 | 687.6 | 332.7 | 3.33× | 2.07× | same |
| Q11 | 256.6 | 51.0 | 44.1 | 5.81× | 1.16× | same |
| Q12 | 881.2 | 423.0 | 309.0 | 2.85× | 1.37× | same |
| Q13 | 1,542.4 | 1,343.7 | 380.4 | 4.05× | 3.53× | same |
| Q14 | 579.7 | 155.8 | 199.8 | 2.90× | 0.78× | same |
| Q15 | 511.7 | 155.5 | 209.7 | 2.44× | 0.74× | same |
| Q16 | 426.1 | 473.6 | 143.9 | 2.96× | 3.29× | same |
| Q17 | 52.7 | 91.2 | 482.7 | 0.11× | 0.19× | same |
| Q18 | 1,247.1 | 979.5 | 1,632.5 | 0.76× | 0.60× | same |
| Q19 | 127.1 | 151.3 | 19.1 | 6.65× | 7.91× | same |
| Q20 | 520.3 | 548.3 | 249.4 | 2.09× | 2.20× | same |
| Q21 | 2,929.6 | 1,103.5 | 1,560.9 | 1.88× | 0.71× | same |
| Q22 | 404.0 | 230.5 | 32.0 | 12.64× | 7.21× | same |
| geometric mean (22 queries) | 674.6 | 297.4 | 287.7 | 2.34× | 1.03× |  |
| load (COPY, indexes, ANALYZE; s) | 97.7 | 110.5 | 26.3 |  |  |  |

Every result agrees with PostgreSQL's.  On column tables nucleus takes
1.03× PostgreSQL's time in the geometric mean and is faster on 12 of the
22 queries (Q7 0.25 s against 2.9 s, Q17 0.09 s against 0.48 s); on row
tables 2.34×.

Row tables' times vary between runs by whole queries: in this run Q1
and Q3 took 1.7 and 3.2 s, in a second run of the row tables alone (the
same build and data) 1.0 and 1.2 s, while Q21 went from 2.9 s to 5.6 s;
the other queries within 10% of each other.  The second run's geometric
mean is 2.26× (2.14× with each query's best of both).  Loaded alone,
the row tables answer Q1, Q3 and Q6 in 0.89, 1.17 and 0.51 s against
0.95, 1.31 and 0.60 s at `940395c`, the build of the run before (same
plans, the same 3.6 GB).  What slows a whole query now and then is not
yet known.

Against the run before (`940395c`, 2026-10-30: 2.24× and 1.10×, loads
118 s and 125 s), the loads are faster (the large transaction's look at
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

- **Q19 and Q22** (8× and 7×): PostgreSQL answers them in 19 and 32 ms
  through indexes; nucleus still reads more than it must (Q19's lookups
  each read a part's lines and filter them; Q22 scans `orders` for its
  anti join).
- **Q13 and Q16** (3.5× and 3.3×): a left join and a grouping over most of
  `orders`, and an anti join with a `LIKE` filter; built a row at a time.
- **Row tables' scans** (Q3–Q6, Q10, Q12: 2.9–4.8×, Q3 4.1× in the
  second run): a row's every column is decoded where a column table
  reads only the wanted ones.
- **Loads** (98 s and 111 s against 26 s: 3.7× and 4.2×): on 10-30
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

2026-11-01, the first million rows, the machine above, at `e2d1afa`;
each query's best of three runs; nucleus with `hits` in rows and in
columns; PostgreSQL 18.6 in the same run:

| ClickBench, 1,000,000 rows | nucleus (row) (ms) | nucleus (column) (ms) | postgresql (ms) | nucleus (row) / PostgreSQL | nucleus (column) / PostgreSQL | results |
|---|---:|---:|---:|---:|---:|---|
| Q0 | 249.2 | 3.4 | 56.5 | 4.41× | 0.06× | same |
| Q1 | 240.6 | 4.1 | 74.3 | 3.24× | 0.06× | same |
| Q2 | 234.5 | 4.5 | 76.6 | 3.06× | 0.06× | same |
| Q3 | 237.1 | 3.6 | 57.3 | 4.14× | 0.06× | same |
| Q4 | 1,505.2 | 592.1 | 199.3 | 7.55× | 2.97× | same |
| Q5 | 1,440.0 | 574.7 | 378.4 | 3.81× | 1.52× | same |
| Q6 | 222.4 | 14.7 | 66.8 | 3.33× | 0.22× | same |
| Q7 | 215.1 | 3.6 | 77.2 | 2.78× | 0.05× | same |
| Q8 | 1,454.2 | 646.2 | 349.0 | 4.17× | 1.85× | same |
| Q9 | 1,542.2 | 754.9 | 462.8 | 3.33× | 1.63× | same |
| Q10 | 267.2 | 20.2 | 81.2 | 3.29× | 0.25× | same |
| Q11 | 274.3 | 20.9 | 80.9 | 3.39× | 0.26× | differs (ties at the LIMIT) |
| Q12 | 261.9 | 25.8 | 91.0 | 2.88× | 0.28× | same |
| Q13 | 343.0 | 82.0 | 140.9 | 2.43× | 0.58× | same |
| Q14 | 283.0 | 27.9 | 248.4 | 1.14× | 0.11× | same |
| Q15 | 324.4 | 60.1 | 104.7 | 3.10× | 0.57× | same |
| Q16 | 343.8 | 107.2 | 298.1 | 1.15× | 0.36× | same |
| Q17 | 326.1 | 78.0 | 289.6 | 1.13× | 0.27× | differs (ties at the LIMIT) |
| Q18 | 2,826.2 | 1,520.2 | 341.8 | 8.27× | 4.45× | differs (ties at the LIMIT) |
| Q19 | 229.8 | 4.6 | 62.8 | 3.66× | 0.07× | same |
| Q20 | 222.8 | 11.6 | 96.8 | 2.30× | 0.12× | same |
| Q21 | 222.0 | 11.0 | 110.9 | 2.00× | 0.10× | same |
| Q22 | 241.0 | 21.6 | 142.0 | 1.70× | 0.15× | same rows (ties ordered differently) |
| Q23 | 237.0 | 220.1 | 100.4 | 2.36× | 2.19× | same rows (ties ordered differently) |
| Q24 | 221.6 | 11.5 | 81.9 | 2.70× | 0.14× | same |
| Q25 | 223.3 | 13.1 | 78.9 | 2.83× | 0.17× | same |
| Q26 | 224.7 | 15.5 | 76.9 | 2.92× | 0.20× | same |
| Q27 | 249.2 | 41.8 | 141.7 | 1.76× | 0.29× | same |
| Q28 | 441.0 | 258.4 | 2,960.6 | 0.15× | 0.09× | differs (ties at the LIMIT) |
| Q29 | 454.3 | 302.6 | 405.8 | 1.12× | 0.75× | same |
| Q30 | 253.3 | 41.5 | 125.2 | 2.02× | 0.33× | differs (ties at the LIMIT) |
| Q31 | 331.6 | 108.1 | 126.7 | 2.62× | 0.85× | differs (ties at the LIMIT) |
| Q32 | 4,340.9 | 3,078.4 | 1,025.2 | 4.23× | 3.00× | differs (ties at the LIMIT) |
| Q33 | 2,183.3 | 1,061.9 | 568.8 | 3.84× | 1.87× | same |
| Q34 | 2,225.4 | 1,012.1 | 600.1 | 3.71× | 1.69× | same |
| Q35 | 372.6 | 126.9 | 143.1 | 2.60× | 0.89× | same rows (ties ordered differently) |
| Q36 | 565.7 | 287.1 | 262.9 | 2.15× | 1.09× | same |
| Q37 | 278.1 | 50.8 | 158.0 | 1.76× | 0.32× | same |
| Q38 | 251.4 | 15.2 | 131.4 | 1.91× | 0.12× | differs (ties at the LIMIT) |
| Q39 | 3,139.1 | 1,741.7 | 536.4 | 5.85× | 3.25× | differs (ties at the LIMIT) |
| Q40 | 276.1 | 57.0 | 217.7 | 1.27× | 0.26× | differs (ties at the LIMIT) |
| Q41 | 244.0 | 13.5 | 203.8 | 1.20× | 0.07× | same |
| Q42 | 238.5 | 24.9 | 149.3 | 1.60× | 0.17× | same |
| geometric mean (43 queries) | 421.1 | 58.7 | 170.6 | 2.47× | 0.34× |  |
| load (COPY, ANALYZE; s) | 18.7 | 20.9 | 7.2 |  |  |  |

Every result agrees, but for ties at a `LIMIT` (the same rows where the
order is total).  On a column table nucleus takes a third of
PostgreSQL's time in the geometric mean (0.34×) and is faster on 32 of
the 43 queries; the counts and aggregates over a few columns (Q0–Q3,
Q7, Q19) take 3–5 ms against 56–77.  Slower: the distinct counts and
groupings over large text keys (Q4, Q5, Q8, Q9: 1.5–3.0×; Q18, Q39:
3.3–4.5×), `SearchPhrase` groupings (Q32–Q34: 1.7–3.0×) and Q23
(`SELECT *` … `ORDER BY` a large text: 2.2×).  On a row table 2.47×:
every query decodes each row's 105 columns.  The loads 18.7 s and
20.9 s against 7.2 s (2.6× and 2.9×; 21.7 s and 21.4 s on 10-31, at
`3043e69`, when the geometric means were 2.51× and 0.34×).

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
| TPC-H SF 1, column tables: geometric mean ≤ 0.8× PostgreSQL's time | 1.03× | no |
| ClickBench 1M rows, column table: geometric mean ≤ 0.5× | 0.34× | yes |
| column tables: no TPC-H or ClickBench query above 3× | TPC-H Q19 7.9×, Q22 7.2×, Q13 3.5×, Q16 3.3×; ClickBench Q18 4.5×, Q39 3.3×, Q32 3.0× | no |
| row tables: geometric mean ≤ 2× (TPC-H, ClickBench) | 2.34× (2.26× in a second run), 2.47× | no |
| loads ≤ 2× (`bench/run.py`, TPC-H, ClickBench) | 1.2–1.4× (as PostgreSQL's own time varies); TPC-H 3.7× (row), 4.2× (column); ClickBench 2.6×, 2.9× | `bench/run.py` yes, the others no |
| updates and deletes ≤ 2× (`bench/run.py`) | update 0.9–1.0×, delete 1.7–1.8× (2026-11-01; the delete 127–135 ms) | yes |
| scans, aggregates, sorts and joins ≤ 3× (`bench/run.py`) | 1.0–2.3× (count(*) the highest); sort 1.9–2.0×; self-join 1.7× (2026-11-01) | yes |
| reads by key ≤ 1× (`bench/run.py`) | 0.6× (2026-11-01) | yes |

Ratios are best-of-three times on one machine in one run; the CI's
shared runners are too noisy to fail on them yet, so a target is checked
here, by a local run, when §M14.20 is closed.

