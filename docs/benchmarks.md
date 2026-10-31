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

2026-10-31, Apple M2 Pro (macOS, 12 cores), nucleus release build at
`3043e69`; PostgreSQL 18 in Docker (Linux VM, 10 CPUs) with its
defaults, parallel workers included.

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

What the numbers say: scans and aggregates are 1.2–2.1× PostgreSQL's
time on a row table (they were 15–30× at `b03c130`), reads by key twice
as fast.  Sorts and joins went parallel and vectorized since 10-29
(3–4× faster); the self-join still builds its table a row at a time.
Writes are the gap: a delete pays for a lookup per row in the engine's
conflict and garbage checks, a load and an update for encoding and
writing each row's versions.

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

## ClickBench

`bench/clickbench.py` (§M14.20 V10): ClickBench's `hits` table (its
PostgreSQL schema) and 43 queries, from the ClickBench repository at a
fixed commit, on the first `--rows` rows of its data, each system's
results checked against the other's (a query whose `LIMIT` may cut ties
differently is reported, not failed).

```sh
python3 bench/clickbench.py --nucleus target/release/nucleus --rows 1000000 \
  --psql "docker exec -i pg psql -U postgres"
```

2026-10-31, the first million rows, the machine above, at `3043e69`;
each query's best of three runs; nucleus with `hits` in rows and in
columns; PostgreSQL 18 in the same run:

| ClickBench, 1,000,000 rows | nucleus (row) (ms) | nucleus (column) (ms) | postgresql (ms) | nucleus (row) / PostgreSQL | nucleus (column) / PostgreSQL | results |
|---|---:|---:|---:|---:|---:|---|
| Q0 | 251.9 | 3.6 | 58.1 | 4.34× | 0.06× | same |
| Q1 | 218.6 | 3.6 | 75.6 | 2.89× | 0.05× | same |
| Q2 | 215.8 | 4.0 | 78.2 | 2.76× | 0.05× | same |
| Q3 | 227.3 | 3.7 | 63.7 | 3.57× | 0.06× | same |
| Q4 | 1,415.1 | 606.9 | 225.1 | 6.29× | 2.70× | same |
| Q5 | 1,442.3 | 604.8 | 351.2 | 4.11× | 1.72× | same |
| Q6 | 260.5 | 15.9 | 57.4 | 4.54× | 0.28× | same |
| Q7 | 245.5 | 4.0 | 76.0 | 3.23× | 0.05× | same |
| Q8 | 1,557.0 | 680.4 | 325.4 | 4.78× | 2.09× | same |
| Q9 | 1,680.0 | 778.7 | 459.3 | 3.66× | 1.70× | same |
| Q10 | 264.3 | 21.2 | 78.0 | 3.39× | 0.27× | same |
| Q11 | 259.5 | 21.8 | 81.2 | 3.20× | 0.27× | differs (ties at the LIMIT) |
| Q12 | 239.7 | 24.7 | 93.0 | 2.58× | 0.27× | same |
| Q13 | 289.3 | 86.4 | 144.1 | 2.01× | 0.60× | same |
| Q14 | 236.8 | 28.2 | 246.4 | 0.96× | 0.11× | same |
| Q15 | 267.6 | 59.7 | 107.4 | 2.49× | 0.56× | same |
| Q16 | 314.0 | 109.1 | 302.1 | 1.04× | 0.36× | same |
| Q17 | 282.0 | 80.5 | 298.3 | 0.95× | 0.27× | differs (ties at the LIMIT) |
| Q18 | 2,802.4 | 1,470.9 | 342.8 | 8.17× | 4.29× | differs (ties at the LIMIT) |
| Q19 | 266.5 | 4.0 | 58.2 | 4.58× | 0.07× | same |
| Q20 | 257.2 | 9.6 | 100.9 | 2.55× | 0.10× | same |
| Q21 | 258.5 | 10.0 | 113.5 | 2.28× | 0.09× | same |
| Q22 | 286.5 | 21.6 | 142.0 | 2.02× | 0.15× | same rows (ties ordered differently) |
| Q23 | 276.3 | 217.1 | 100.6 | 2.75× | 2.16× | same |
| Q24 | 282.5 | 12.1 | 84.2 | 3.35× | 0.14× | differs (ties at the LIMIT) |
| Q25 | 259.8 | 12.4 | 79.4 | 3.27× | 0.16× | same |
| Q26 | 266.1 | 14.9 | 95.5 | 2.79× | 0.16× | same |
| Q27 | 302.0 | 41.5 | 157.4 | 1.92× | 0.26× | same |
| Q28 | 538.2 | 244.1 | 3,031.9 | 0.18× | 0.08× | differs (ties at the LIMIT) |
| Q29 | 534.7 | 329.2 | 397.4 | 1.35× | 0.83× | same |
| Q30 | 269.2 | 38.8 | 121.0 | 2.23× | 0.32× | differs (ties at the LIMIT) |
| Q31 | 340.0 | 102.6 | 128.7 | 2.64× | 0.80× | differs (ties at the LIMIT) |
| Q32 | 4,043.9 | 3,018.5 | 1,059.2 | 3.82× | 2.85× | differs (ties at the LIMIT) |
| Q33 | 2,395.3 | 1,075.3 | 561.7 | 4.26× | 1.91× | same |
| Q34 | 2,299.8 | 1,060.1 | 555.1 | 4.14× | 1.91× | same |
| Q35 | 349.4 | 128.0 | 147.7 | 2.36× | 0.87× | same rows (ties ordered differently) |
| Q36 | 490.6 | 308.6 | 262.0 | 1.87× | 1.18× | same |
| Q37 | 255.9 | 48.0 | 158.2 | 1.62× | 0.30× | same |
| Q38 | 227.1 | 17.2 | 134.3 | 1.69× | 0.13× | differs (ties at the LIMIT) |
| Q39 | 2,797.2 | 1,711.9 | 541.0 | 5.17× | 3.16× | differs (ties at the LIMIT) |
| Q40 | 289.1 | 53.5 | 205.8 | 1.40× | 0.26× | differs (ties at the LIMIT) |
| Q41 | 256.0 | 13.2 | 203.3 | 1.26× | 0.06× | same |
| Q42 | 265.1 | 24.4 | 147.4 | 1.80× | 0.17× | same |
| geometric mean (43 queries) | 430.5 | 58.4 | 171.8 | 2.51× | 0.34× |  |
| load (COPY, ANALYZE; s) | 21.7 | 21.4 | 7.7 |  |  |  |

Every result agrees, but for ties at a `LIMIT` (the same rows where the
order is total).  On a column table nucleus takes a third of
PostgreSQL's time in the geometric mean (0.34×) and is faster on 32 of
the 43 queries; the counts and aggregates over a few columns (Q0–Q3,
Q7, Q19) take 4 ms against 60–80.  Slower: the distinct counts and
groupings over large text keys (Q4, Q5, Q8, Q9: 1.7–2.7×; Q18, Q39:
3–4×), `SearchPhrase` groupings (Q32–Q34: 1.9–2.9×) and Q23 (`SELECT *`
… `ORDER BY` a large text: 2.2×).  On a row table 2.51×: every query
decodes each row's 105 columns.

## Targets

PLAN.md §M14.11 asks for targets set from the first results, and
§M14.20 is done when analytical queries beat PostgreSQL by them and
transactional workloads stay within their margin.  First set from the
first `bench/run.py` run (`79506d4`, 2026-10-27) — within 2× of PostgreSQL on loads
and updates, within 3× on scans, sorts and joins, at least even on reads
by key —, now recorded from the runs above (2026-10-31), on this machine
against PostgreSQL 18 in the same run:

| target | now | met |
|---|---|---|
| TPC-H SF 1, column tables: geometric mean ≤ 0.8× PostgreSQL's time | 1.10× | no |
| ClickBench 1M rows, column table: geometric mean ≤ 0.5× | 0.34× | yes |
| column tables: no TPC-H or ClickBench query above 3× | TPC-H Q19 8.9×, Q22 6.7×, Q16 3.7×, Q13 3.5×; ClickBench Q18 4.3×, Q39 3.2× | no |
| row tables: geometric mean ≤ 2× (TPC-H, ClickBench) | 2.24×, 2.51× | no |
| loads ≤ 2× (`bench/run.py`, TPC-H, ClickBench) | 3.3×; 4.5× (row), 4.8× (column); 2.8× | no |
| updates and deletes ≤ 2× (`bench/run.py`) | 2.2×, 7.5× | no |
| scans, aggregates, sorts and joins ≤ 3× (`bench/run.py`) | 1.2–2.1×; sort 1.9×; self-join 3.8× | joins no |
| reads by key ≤ 1× (`bench/run.py`) | 0.5× | yes |

Ratios are best-of-three times on one machine in one run; the CI's
shared runners are too noisy to fail on them yet, so a target is checked
here, by a local run, when §M14.20 is closed.

