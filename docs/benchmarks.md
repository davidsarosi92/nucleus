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

**Latest** (2026-11-06, nucleus on §M14.27 S4·sort; PostgreSQL 18.6 in the
same session, `postgres:18-alpine` with its defaults; one run each, each
step's best of its repeats):

| 1,000,000 rows | nucleus (ms) | PostgreSQL (ms) | ratio |
|---|---:|---:|---:|
| load (INSERT … SELECT) | 1,039.5 | 1,040.1 | 1.0× |
| count(*) | 16.4 | 14.2 | 1.1× |
| filtered count | 19.6 | 17.4 | 1.1× |
| sum | 15.9 | 17.2 | 0.9× |
| group by (1000 groups) | 22.2 | 34.5 | 0.6× |
| sort | 82.0 | 155.3 | 0.5× |
| self-join | 81.7 | 126.4 | 0.6× |
| point reads (1000 by key) | 61.0 | 112.8 | 0.5× |
| update 25% | 640.4 | 878.9 | 0.7× |
| delete 10% | 117.8 | 76.7 | 1.5× |
| count after writes | 22.6 | 21.8 | 1.0× |

Every target met (Targets, below).

**A columnar table** (§M14.20 V7), the same million rows in `WITH
(storage = column)` (its columns 32 MB): `sum` 2.0 ms (one thread 12.8
ms; the row table 22 / 81 ms), the filtered count 2.4 ms, the 1000-group
`GROUP BY` 19 ms, a read by key 0.3–1.7 ms (0.1 ms).  Not yet in the
suite run against PostgreSQL.

**Results into numpy** (§M14.20 V10): a million rows (`id int, x bigint,
f float, s text`) fetched with the Python driver and every column turned
into a numpy array, best of three, server and client on the same
machine — fetched as rows and converted, and as the executor's batches
sent as columns (`batches()`):

| query | row table, rows → columns (ms) | row table, batches (ms) | column table, rows → columns (ms) | column table, batches (ms) |
|---|---:|---:|---:|---:|
| `SELECT id, x, f` | 262 | 92 | 241 | 51 |
| `SELECT id, x, f, s` | 789 | 198 | 805 | 162 |
| `SELECT id, f … WHERE x < 500` (half) | 52 | 33 | 34 | 16 |

**Per-statement costs** (PLAN.md §M14.20 W8): one statement at a time on
one connection, what an AI agent's traffic pays —
`crates/client/examples/latency.rs` against a `nucleus server --no-auth`
(a 100 000-row table `(k BIGINT PRIMARY KEY, v TEXT, n INT)`), 20 000 of
each, the median in µs (writes wait on their fsyncs, and move by 10 %
and more from run to run):

| `SELECT 1` | point read, literal | point read, parameter | insert | update by key |
|---:|---:|---:|---:|---:|
| 20.4 | 36.5 | 25.8 | 172 | 178 |

The parameterized point read sent 100 at a time (`Client::pipeline`):
5.9 µs a statement.  Inserts 100 rows a call (`Client::execute_many`:
pipelined, one commit for all): 11.3 / 12.4 µs a row.  Through the C
driver, the parameterized point read: 25 µs.  A million rows of a
bigint, a text, a numeric and a timestamp read whole
(`crates/client/examples/bigread.rs`), median: 446 ms as rows, 146 ms as
columnar batches; through a cursor, 1000 rows a batch, 461 / 466 ms and
165 / 166 ms; a Python stream of a million rows of three columns
(`Connection.stream`, the rows summed) 2,429 / 2,370 ms.

Through the PostgreSQL front end (`--pg-compat`), the `postgres` driver's
prepared statements (`modules/pg-compat/examples/pg_latency.rs`, the
same table), median: a prepared point read 39.4 µs, a prepared update by
key 197 µs.

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


2026-11-06, SF 1, the machine above, nucleus after §M14.12 V3; each query's
best of three runs; nucleus with every table in rows, and with all but
`region` and `nation` in columns; PostgreSQL 18.6 in the same run:

| TPC-H SF 1 | nucleus (row) (ms) | nucleus (column) (ms) | postgresql (ms) | nucleus (row) / PostgreSQL | nucleus (column) / PostgreSQL | results |
|---|---:|---:|---:|---:|---:|---|
| Q1 | 389.7 | 173.5 | 1,003.9 | 0.39× | 0.17× | same |
| Q2 | 26.5 | 23.9 | 208.5 | 0.13× | 0.11× | same |
| Q3 | 533.2 | 75.5 | 313.4 | 1.70× | 0.24× | same |
| Q4 | 205.8 | 71.1 | 155.5 | 1.32× | 0.46× | same |
| Q5 | 765.1 | 74.5 | 501.7 | 1.53× | 0.15× | same |
| Q6 | 330.0 | 71.4 | 198.5 | 1.66× | 0.36× | same |
| Q7 | 511.7 | 84.8 | 2,932.5 | 0.17× | 0.03× | same |
| Q8 | 213.1 | 80.6 | 194.3 | 1.10× | 0.41× | same |
| Q9 | 600.9 | 276.4 | 1,581.6 | 0.38× | 0.17× | same |
| Q10 | 488.3 | 134.7 | 337.4 | 1.45× | 0.40× | same |
| Q11 | 190.5 | 24.7 | 48.1 | 3.96× | 0.51× | same |
| Q12 | 348.1 | 65.3 | 317.4 | 1.10× | 0.21× | same |
| Q13 | 144.1 | 67.5 | 396.1 | 0.36× | 0.17× | same |
| Q14 | 323.2 | 55.8 | 206.6 | 1.56× | 0.27× | same |
| Q15 | 313.9 | 62.1 | 225.7 | 1.39× | 0.27× | same |
| Q16 | 164.9 | 100.0 | 130.7 | 1.26× | 0.76× | same |
| Q17 | 18.3 | 30.1 | 502.4 | 0.04× | 0.06× | same |
| Q18 | 765.8 | 173.4 | 1,712.3 | 0.45× | 0.10× | same |
| Q19 | 41.4 | 38.0 | 18.3 | 2.26× | 2.08× | same |
| Q20 | 201.4 | 164.7 | 317.5 | 0.63× | 0.52× | same |
| Q21 | 608.1 | 468.9 | 1,676.9 | 0.36× | 0.28× | same |
| Q22 | 79.5 | 15.6 | 33.5 | 2.37× | 0.47× | same |
| geometric mean (22 queries) | 229.0 | 76.4 | 300.2 | 0.76× | 0.25× |  |
| load (COPY, indexes, ANALYZE; s) | 25.6 | 22.6 | 29.2 |  |  |  |

Every result agrees with PostgreSQL's.  Column tables 0.25× PostgreSQL's
time in the geometric mean, faster on 21 of the 22 queries; row tables
0.76×, faster on 9.  The loads — `COPY`, the indexes and `ANALYZE` —
25.6 s (rows) and 22.6 s (columns) against 29.2 s.

Where the time still goes:

- **Q19** on column tables (2.08×, 38 ms against 18): its `OR` of three
  conjunctions over `part` and `lineitem`, ~14,000 `lineitem` rows each
  fetched by key from the column table, where PostgreSQL reads a heap
  tuple each.
- **Row tables** (0.76×): Q11 4.0×, Q22 2.4×, Q19 2.3×, Q3 1.7×, Q6
  1.7× the highest — a row's columns decoded where a column table reads
  only the wanted ones.
- **Q18** on row tables (766 ms; 489 before §M14.12 V3): its `HAVING`
  estimated a third of the orders, 57 in fact — a hash join over
  `lineitem` where lookups would do; the adaptive join (PLAN.md §M14.12)
  is the answer.

The load phase by phase (`bench/phases.py load`, SF 1, row tables,
PostgreSQL 18 beside it in the same session):

| | `COPY` | `CREATE INDEX` | `ANALYZE` | total (s) |
|---|---:|---:|---:|---:|
| nucleus (`7b5f092`, §M14.20 W7) | 14.5 | 12.9 | 1.8 | 29.3 |
| PostgreSQL 18 | 18.3 | 5.5 | 5.3 | 29.1 |

How a step is measured: `bench/phases.py` — `load` times each
table's `COPY`, each index and `ANALYZE`; `reads` loads, then times the
22 queries right after the load (`first`), as the best of `--runs`
(`best`) and, with `--vacuum`, after a `VACUUM`.  `clickbench` loads
`hits` and times its 43 queries the same way, from `bench/clickbench.py`'s
cache (`create.sql`, `queries.sql`, `hits-N.tsv`).  The data is
`bench/tpch.py --data DIR`'s generated `*.tbl` files (SF 1).  A step is
compared with the commit before it built in a worktree into its own
target directory (`target/base`), the two binaries run alternating, two
rounds or more: the machine's load moves a run by 10 % and more.  The
script's header has the commands.

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


2026-11-06, the first million rows, nucleus after §M14.12 V3, as the
TPC-H run above; each query's best of three runs; PostgreSQL 18.6 in the
same run:

| ClickBench, 1,000,000 rows | nucleus (row) (ms) | nucleus (column) (ms) | postgresql (ms) | nucleus (row) / PostgreSQL | nucleus (column) / PostgreSQL | results |
|---|---:|---:|---:|---:|---:|---|
| Q0 | 119.6 | 2.4 | 71.4 | 1.68× | 0.03× | same |
| Q1 | 126.2 | 2.7 | 78.3 | 1.61× | 0.03× | same |
| Q2 | 100.9 | 2.6 | 79.3 | 1.27× | 0.03× | same |
| Q3 | 96.3 | 2.6 | 68.5 | 1.41× | 0.04× | same |
| Q4 | 106.5 | 13.2 | 222.5 | 0.48× | 0.06× | same |
| Q5 | 107.8 | 12.2 | 368.3 | 0.29× | 0.03× | same |
| Q6 | 119.7 | 6.7 | 68.9 | 1.74× | 0.10× | same |
| Q7 | 118.9 | 2.8 | 77.5 | 1.53× | 0.04× | same |
| Q8 | 161.9 | 23.0 | 349.7 | 0.46× | 0.07× | same |
| Q9 | 186.1 | 71.6 | 475.3 | 0.39× | 0.15× | same |
| Q10 | 123.3 | 5.3 | 84.1 | 1.47× | 0.06× | same |
| Q11 | 161.4 | 5.5 | 81.5 | 1.98× | 0.07× | differs (ties at the LIMIT) |
| Q12 | 153.1 | 10.9 | 92.5 | 1.65× | 0.12× | same |
| Q13 | 157.3 | 21.3 | 146.6 | 1.07× | 0.15× | same |
| Q14 | 136.2 | 11.8 | 258.0 | 0.53× | 0.05× | same |
| Q15 | 144.5 | 17.5 | 109.4 | 1.32× | 0.16× | same |
| Q16 | 146.3 | 28.7 | 323.2 | 0.45× | 0.09× | same |
| Q17 | 149.4 | 24.9 | 307.2 | 0.49× | 0.08× | differs (ties at the LIMIT) |
| Q18 | 229.7 | 131.7 | 346.6 | 0.66× | 0.38× | differs (ties at the LIMIT) |
| Q19 | 110.2 | 3.3 | 69.7 | 1.58× | 0.05× | same |
| Q20 | 103.4 | 7.4 | 95.3 | 1.08× | 0.08× | same |
| Q21 | 106.1 | 6.4 | 116.8 | 0.91× | 0.05× | same |
| Q22 | 114.6 | 7.2 | 141.6 | 0.81× | 0.05× | same rows (ties ordered differently) |
| Q23 | 198.5 | 130.0 | 96.2 | 2.06× | 1.35× | same |
| Q24 | 107.1 | 5.5 | 79.1 | 1.35× | 0.07× | differs (ties at the LIMIT) |
| Q25 | 139.7 | 6.9 | 81.1 | 1.72× | 0.08× | same |
| Q26 | 103.6 | 7.3 | 80.1 | 1.29× | 0.09× | same |
| Q27 | 139.3 | 41.9 | 148.9 | 0.94× | 0.28× | same |
| Q28 | 331.1 | 263.4 | 2,972.2 | 0.11× | 0.09× | differs (ties at the LIMIT) |
| Q29 | 329.1 | 233.5 | 403.1 | 0.82× | 0.58× | same |
| Q30 | 144.3 | 15.5 | 121.9 | 1.18× | 0.13× | differs (ties at the LIMIT) |
| Q31 | 151.0 | 28.0 | 127.3 | 1.19× | 0.22× | differs (ties at the LIMIT) |
| Q32 | 440.7 | 319.9 | 1,110.5 | 0.40× | 0.29× | differs (ties at the LIMIT) |
| Q33 | 220.6 | 119.7 | 670.4 | 0.33× | 0.18× | same |
| Q34 | 234.2 | 132.0 | 659.4 | 0.36× | 0.20× | same |
| Q35 | 129.5 | 33.1 | 144.8 | 0.89× | 0.23× | same rows (ties ordered differently) |
| Q36 | 165.5 | 57.4 | 286.7 | 0.58× | 0.20× | same |
| Q37 | 121.4 | 17.2 | 164.3 | 0.74× | 0.10× | same |
| Q38 | 119.4 | 10.0 | 133.6 | 0.89× | 0.07× | differs (ties at the LIMIT) |
| Q39 | 286.6 | 188.4 | 576.2 | 0.50× | 0.33× | differs (ties at the LIMIT) |
| Q40 | 112.2 | 10.3 | 209.8 | 0.53× | 0.05× | differs (ties at the LIMIT) |
| Q41 | 119.2 | 9.0 | 202.3 | 0.59× | 0.04× | same |
| Q42 | 125.1 | 22.7 | 150.9 | 0.83× | 0.15× | same |
| geometric mean (43 queries) | 147.2 | 17.9 | 177.3 | 0.83× | 0.10× |  |
| load (COPY, ANALYZE; s) | 7.6 | 6.3 | 10.3 |  |  |  |

Every result agrees, but for ties at a `LIMIT`.  A column table 0.10× of
PostgreSQL's time in the geometric mean (none above 1.4×: Q23, `SELECT
*` … `ORDER BY` a large text, 1.35×); a row table 0.83× (none above
2.1×: Q23 2.06×, Q11 1.98×).  The loads 7.6 s (rows) and 6.3 s (columns)
against 10.3 s.

## Vectors

Exact nearest neighbours (PLAN.md §M14.12; `bench/vectors/knn.py
target/release/nucleus --pg`): 100,000 random vectors of 768 dimensions
(293 MiB), the ten nearest of 20 queries, median; nucleus with a 1 GB
page cache (`--cache 1GB`) as pgvector has `shared_buffers = 1GB`;
2026-11-06, after §M14.12 V2.

| | ms | GiB/s read | answers differing from numpy's |
|---|---:|---:|---:|
| numpy brute force (one core) | 57 | 5.0 | — |
| nucleus `ORDER BY v <-> $1 LIMIT 10` | 48 | 6.0 | 0 of 20 |
| pgvector (PostgreSQL 18, no index) | 167 | 1.7 | 0 of 20 |

Loading the 100,000 vectors: nucleus 21 s (Python `load`, a list of
numpy arrays), pgvector 42 s (`COPY` of text through `psql`).

## Statistics

Aggregates over doubles and integers (PLAN.md §M14.27;
`bench/stats/aggs.py NUCLEUS --psql … --numpy`): a table of 10 million
rows — `x` on a large offset (1e9 + a fraction), `y` spread over 30
decades, `id` the integers — each query's best of five, the better of
two rounds; PostgreSQL 18 (Docker, its parallel workers) and numpy (one
core, the same data in an array) beside it.  2026-11-06, after §M14.27
S3:

| query | nucleus (ms) | PostgreSQL 18 (ms) | numpy (ms) |
|---|---:|---:|---:|
| `sum(x)` | 193 | 150 | 2 |
| `avg(y)` | 185 | 169 | |
| `sum(x)` by 1000 groups | 196 | 328 | |
| `avg(y)` by a million groups | 1,864 | 2,272 | |
| `variance(x)` | 177 | 160 | 7 |
| `stddev(x)` by 1000 groups | 209 | 354 | |
| `variance(id)` (integers) | 160 | 241 | |
| `skewness(x)`, `kurtosis(x)` | 286 | — | |
| `corr(x, id)` | 220 | 173 | 37 |
| `regr_slope`, `regr_intercept` by 1000 groups | 275 | 359 | |

nucleus gives the same bits every run, the exact value rounded once:
the variance of `x` is 0.08333332757266079 (PostgreSQL's differs from
run to run and from the ninth digit: 0.0833333256…, 0.0833333257…),
`corr(x, id)` −3.4589588652949373e−6 (PostgreSQL's −1.667e−6 and
−3.056e−6 in two runs; numpy's two-pass −3.4589588653e−6).  numpy is an
array in memory, not a table scan.

Window functions over the same table (§M14.27 S4), each folded to one
row so the result is small; best of five, the best of two rounds
(2026-11-06, §M14.27 S4·sort):

| query | nucleus (ms) | PostgreSQL 18 (ms) |
|---|---:|---:|
| `row_number()` by 1000 partitions, ordered by `x` | 2,534 | 4,507 |
| top 3 by 1000 partitions (`rank() … WHERE r <= 3`) | 2,207 | 3,880 |
| running `sum(x)`, ordered by `id` | 3,447 | 2,570 |
| `avg(x)` over 10 rows by partition | 5,242 | 7,924 |
| `x - lag(x)`, ordered by `id` | 2,578 | 2,407 |

Ordered by `id` nothing is sorted: a table's scan gives the primary
key's order, as PostgreSQL's index does.  What remains is the one
partition of 10M rows held in memory, where PostgreSQL streams it (PLAN.md
§M14.27 S4·sort, next).  Sorts alone, read whole: 10M rows by `(g, x)`
1.8 s (PostgreSQL 1.9 s), by `id` 0.9 s (PostgreSQL's index 1.65 s).
The sums are exact: the running sum's largest value is
1.0000000004999998e16, PostgreSQL's 1.0000000004503596e16 (its double
accumulator loses the fractions past 10¹⁶), and of 200,000 ten-row
moving averages PostgreSQL rounds 98,995 to another double than the
exact average's (by up to 2 ulps; nucleus none).

Exact quantiles over the same table (§M14.27 S5), best of five, the
better of two rounds (2026-11-06); nucleus with its default `work_mem`
of 64 MB per worker, PostgreSQL with its 4 MB:

| query | nucleus (ms) | PostgreSQL 18 (ms) | numpy (ms) |
|---|---:|---:|---:|
| `percentile_cont(0.5)` of `x` | 613 | 2,249 | 26 |
| `percentile_cont(ARRAY[0.5, 0.95, 0.99])` of `y` | 668 | 2,240 | |
| `percentile_disc(0.9)` of `x` | 618 | 2,374 | 44 |
| `percentile_cont(0.5)` of `x` by 1000 groups | 615 | 550 | |
| `mode()` of `id % 1000` | 736 | 1,333 | |
| `histogram(x, …, 100)` | 167 | — | |

The results are PostgreSQL's and numpy's, bit for bit.  PostgreSQL's
`work_mem` holds each group's values (1000 × 4 MB), nucleus's one
accumulator's (with 1 GB: 499 ms).  An exact median of 10⁸ values
(`generate_series`) takes 23.0 s and 216 MB at its peak, against
`sum`'s 18.0 s and 146 MB: the values past `work_mem` go to scratch
space and are selected a partition at a time.

Sketches (§M14.27 S6) beside the exact answers, same table, best of
five, the better of two rounds (2026-11-06; PostgreSQL has no
approximate aggregates):

| query | sketch (ms) | exact, nucleus (ms) | exact, PostgreSQL 18 (ms) |
|---|---:|---:|---:|
| distinct `id`: `approx_count_distinct` / `count(DISTINCT)` | 172 | 941 | 1,126 |
| the same by 1000 groups | 352 | 1,658 | 878 |
| p50, p95, p99 of `y`: `approx_percentile` / `percentile_cont` | 207 | 747 | 2,240 |
| the 3 most frequent of `id % 1000`: `approx_top_k` / `mode` (one) | 248 | 757 | 1,353 |

`approx_count_distinct(id)` gives 10,029,981 (+0.30 %), by groups of
10,000 9,937–9,973; `approx_percentile` of `y` 0, 2.6524e16 (−0.04 %),
1.2549e19 (+0.83 %).  The sketches' bytes are the same in every
executor and run.  `count(DISTINCT)` by groups is behind PostgreSQL's
(PLAN.md §M14.27 S6, next).

Sampling (§M14.27 S7), same table, best of five, the better of two
rounds (2026-11-06); each query sums the sample (`count(*), sum(x)`):

| query | nucleus (ms) | PostgreSQL 18 (ms) |
|---|---:|---:|
| `TABLESAMPLE BERNOULLI (10)` | 196 | 318 |
| `TABLESAMPLE BERNOULLI (10) REPEATABLE (42)` | 248 | 263 |
| `WHERE hash_unit(id) < 0.8` (8M rows) | 399 | — |
| `reservoir_sample(x, 1000)` | 179 | — |
| `ORDER BY random() LIMIT 1000` | 250 | 636 |

nucleus's `REPEATABLE` sample is the same rows on every machine and in
row and column storage; PostgreSQL's depends on the pages.

Drift and tests (§M14.27 S8a), same table, its halves by `id % 2`
(the categories `id % 100` by `g < 500`), best of five, the better of
two rounds (2026-11-06); PostgreSQL has none, so scipy and
scikit-learn 1.9 on the same values, already in numpy arrays:

| query | nucleus (ms) | Python (ms) |
|---|---:|---:|
| `ks_test(x, side)` | 854 (373 with `work_mem` 1 GB) | 407 (`ks_2samp`) |
| `auc(y, side)` | 899 | 1,327 (`roc_auc_score`) |
| `psi(x, side)` | 888 | — |
| `chisq_test(category, side)` | 282 | — |
| `js_divergence(category, side)` | 275 | — |

At the default `work_mem` (64 MB) the 80 MB of keys go to scratch
space; in memory the scan and the test together beat scipy's test
alone.

`SUMMARIZE t` (§M14.27 S8b) of the same table, four columns, one scan:
624 ms (`sum(x)` 183).

## Write amplification

Bytes written to disk per byte of rows inserted (PLAN.md §M14.28;
`bench/wamp/wamp.py`): rows of a `bigint` key and 100 bytes of text; a
phase counts once its pages are durable — nucleus killed and restarted
(recovery replays the WAL and checkpoints), the server process's bytes
(macOS `proc_pid_rusage`); PostgreSQL 18 in Docker with its defaults, a
`CHECKPOINT`, its cgroup's bytes.  Apple M2 Pro, APFS; 2026-11-06, two
runs where they differ.

| phase | nucleus | PostgreSQL 18 |
|---|---:|---:|
| bulk: 10M rows, 1M a transaction, keys in order | 4.1 | 5.2 – 7.8 |
| bulk: 10M rows, 1M a transaction, random keys | 4.7 – 5.7 | 6.7 – 11.3 |
| 100k rows, one a transaction, keys in order | 152 | 80 |
| 100k rows, one a transaction, random keys among the 10M | 204 | 83 |
| 100k rows, 100 a transaction, random keys | 27 | 6.6 |
| 5M rows, 100 a transaction, random keys (steady state) | 29.4 | 29.6 |

A commit costs nucleus ~16 KiB on APFS whatever the WAL does — APFS
writes a changed block elsewhere, so an overwrite costs more than an
append there (§M14.28 L0) —; PostgreSQL's ~8 KiB is a cgroup's count on
ext4, which leaves out the file system's journal: not the same quantity.
The 100k-row phases end in a forced checkpoint; the steady state is the
comparison that holds.


On **ext4** (Docker's Linux VM; the server built for Linux, the bytes
the VM's disk wrote, journal included; `nucleus-docker` below), one-row
commits, three runs:

| phase | nucleus |
|---|---:|
| 100k rows, one a transaction, keys in order: time | 13.6 – 15.2 s |
| … bytes a commit (the disk's, two runs) | 4.4 KiB |
| 100k rows, 100 a transaction: time | 3.3 – 4.0 s |

```sh
python3 -I bench/wamp/wamp.py nucleus target/release/nucleus /tmp/wamp-db 21006 [0,1,2,3,4,5]
python3 -I bench/wamp/wamp.py nucleus-docker VOLUME /t/nucleus 21500 [2,4]   # a Linux build in VOLUME
python3 -I bench/wamp/wamp.py pg [0,1,2,3,4,5]     # Docker, postgres:18-alpine
```

## Targets

PLAN.md §M14.11 asks for targets set from the first results, and
§M14.20 is done when analytical queries beat PostgreSQL by them and
transactional workloads stay within their margin.  First set from the
first `bench/run.py` run (`79506d4`, 2026-10-27) — within 2× of
PostgreSQL on loads and updates, within 3× on scans, sorts and joins, at
least even on reads by key —, checked by the runs above (2026-11-06), on
this machine against PostgreSQL 18 in the same run:

| target | now | met |
|---|---|---|
| TPC-H SF 1, column tables: geometric mean ≤ 0.8× PostgreSQL's time | 0.25× | yes |
| ClickBench 1M rows, column table: geometric mean ≤ 0.5× | 0.10× | yes |
| column tables: no TPC-H or ClickBench query above 3× | the highest TPC-H Q19 1.93×, ClickBench Q23 1.36× | yes |
| row tables: geometric mean ≤ 2× (TPC-H, ClickBench) | TPC-H 0.76×, ClickBench 0.83× | yes |
| loads ≤ 2× (`bench/run.py`, TPC-H, ClickBench) | 1.0×; TPC-H 0.62× row, 0.55× column; ClickBench 0.92×, 0.74× | yes |
| updates and deletes ≤ 2× (`bench/run.py`) | update 0.7×, delete 1.5× | yes |
| scans, aggregates, sorts and joins ≤ 3× (`bench/run.py`) | 0.5–1.1× (sort 0.5×, count(*) 1.1×, self-join 0.6×) | yes |
| reads by key ≤ 1× (`bench/run.py`) | 0.5× | yes |

Ratios are best-of-three times on one machine in one run; the CI's
shared runners are too noisy to fail on them yet, so a target is checked
here, by a local run.
