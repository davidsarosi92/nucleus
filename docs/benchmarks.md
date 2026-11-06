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

**Latest** (2026-11-06, nucleus on `a2e7f6f`; PostgreSQL 18.6 in the same
session, `postgres:18-alpine` with its defaults; one run each, each
step's best of its repeats):

| 1,000,000 rows | nucleus (ms) | PostgreSQL (ms) | ratio |
|---|---:|---:|---:|
| load (INSERT … SELECT) | 970.2 | 883.0 | 1.1× |
| count(*) | 18.1 | 12.7 | 1.4× |
| filtered count | 21.0 | 16.5 | 1.3× |
| sum | 17.3 | 15.0 | 1.2× |
| group by (1000 groups) | 24.3 | 33.4 | 0.7× |
| sort | 279.4 | 152.3 | 1.8× |
| self-join | 82.4 | 129.6 | 0.6× |
| point reads (1000 by key) | 58.7 | 149.2 | 0.4× |
| update 25% | 639.4 | 685.2 | 0.9× |
| delete 10% | 111.9 | 72.2 | 1.5× |
| count after writes | 20.7 | 20.7 | 1.0× |

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


2026-11-06, SF 1, the machine above, nucleus on `a2e7f6f`; each query's
best of three runs; nucleus with every table in rows, and with all but
`region` and `nation` in columns; PostgreSQL 18.6 in the same run:

| TPC-H SF 1 | nucleus (row) (ms) | nucleus (column) (ms) | postgresql (ms) | nucleus (row) / PostgreSQL | nucleus (column) / PostgreSQL | results |
|---|---:|---:|---:|---:|---:|---|
| Q1 | 395.9 | 160.7 | 1,003.1 | 0.39× | 0.16× | same |
| Q2 | 24.0 | 24.8 | 209.2 | 0.11× | 0.12× | same |
| Q3 | 422.5 | 71.0 | 319.1 | 1.32× | 0.22× | same |
| Q4 | 219.1 | 68.3 | 156.8 | 1.40× | 0.44× | same |
| Q5 | 793.3 | 74.3 | 494.3 | 1.60× | 0.15× | same |
| Q6 | 363.1 | 76.5 | 195.0 | 1.86× | 0.39× | same |
| Q7 | 623.4 | 83.5 | 3,024.5 | 0.21× | 0.03× | same |
| Q8 | 187.9 | 83.8 | 200.6 | 0.94× | 0.42× | same |
| Q9 | 626.5 | 235.4 | 1,502.0 | 0.42× | 0.16× | same |
| Q10 | 487.8 | 131.3 | 332.0 | 1.47× | 0.40× | same |
| Q11 | 177.4 | 24.5 | 47.8 | 3.71× | 0.51× | same |
| Q12 | 406.4 | 64.2 | 315.6 | 1.29× | 0.20× | same |
| Q13 | 151.5 | 61.9 | 372.8 | 0.41× | 0.17× | same |
| Q14 | 329.2 | 56.7 | 198.9 | 1.65× | 0.29× | same |
| Q15 | 298.4 | 62.6 | 215.4 | 1.38× | 0.29× | same |
| Q16 | 210.3 | 102.8 | 129.5 | 1.62× | 0.79× | same |
| Q17 | 19.0 | 29.1 | 499.9 | 0.04× | 0.06× | same |
| Q18 | 489.3 | 166.3 | 1,727.2 | 0.28× | 0.10× | same |
| Q19 | 48.2 | 38.3 | 19.8 | 2.43× | 1.93× | same |
| Q20 | 201.4 | 166.5 | 289.2 | 0.70× | 0.58× | same |
| Q21 | 980.5 | 395.6 | 1,732.2 | 0.57× | 0.23× | same |
| Q22 | 57.2 | 14.9 | 34.7 | 1.65× | 0.43× | same |
| geometric mean (22 queries) | 231.4 | 74.5 | 298.7 | 0.77× | 0.25× |  |
| load (COPY, indexes, ANALYZE; s) | 25.5 | 22.6 | 41.1 |  |  |  |

Every result agrees with PostgreSQL's.  Column tables 0.25× PostgreSQL's
time in the geometric mean, faster on 21 of the 22 queries; row tables
0.77×, faster on 10.  The loads — `COPY`, the indexes and `ANALYZE` —
25.5 s (rows) and 22.6 s (columns) against 41.1 s.

Where the time still goes:

- **Q19** on column tables (1.93×, 38 ms against 20): its `OR` of three
  conjunctions over `part` and `lineitem`, ~14,000 `lineitem` rows each
  fetched by key from the column table, where PostgreSQL reads a heap
  tuple each.
- **Row tables** (0.77×): Q11 3.7×, Q19 2.4×, Q6 1.9×, Q14 and Q22 1.65×
  the highest — a row's columns decoded where a column table reads only
  the wanted ones.

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


2026-11-06, the first million rows, nucleus on `a2e7f6f`, as the TPC-H
run above; each query's best of three runs; PostgreSQL 18.6 in the same
run:

| ClickBench, 1,000,000 rows | nucleus (row) (ms) | nucleus (column) (ms) | postgresql (ms) | nucleus (row) / PostgreSQL | nucleus (column) / PostgreSQL | results |
|---|---:|---:|---:|---:|---:|---|
| Q0 | 93.7 | 2.4 | 71.8 | 1.31× | 0.03× | same |
| Q1 | 126.2 | 2.9 | 77.0 | 1.64× | 0.04× | same |
| Q2 | 143.8 | 2.7 | 78.9 | 1.82× | 0.03× | same |
| Q3 | 118.0 | 2.6 | 71.6 | 1.65× | 0.04× | same |
| Q4 | 129.8 | 14.7 | 221.2 | 0.59× | 0.07× | same |
| Q5 | 134.4 | 12.6 | 368.0 | 0.37× | 0.03× | same |
| Q6 | 127.7 | 6.1 | 72.2 | 1.77× | 0.08× | same |
| Q7 | 131.8 | 2.9 | 76.8 | 1.72× | 0.04× | same |
| Q8 | 161.9 | 22.4 | 364.1 | 0.44× | 0.06× | same |
| Q9 | 213.6 | 72.7 | 474.2 | 0.45× | 0.15× | same |
| Q10 | 144.3 | 5.3 | 80.5 | 1.79× | 0.07× | same |
| Q11 | 103.9 | 5.6 | 82.9 | 1.25× | 0.07× | differs (ties at the LIMIT) |
| Q12 | 110.4 | 12.6 | 92.0 | 1.20× | 0.14× | same |
| Q13 | 127.7 | 22.0 | 141.1 | 0.90× | 0.16× | same |
| Q14 | 123.5 | 13.6 | 257.2 | 0.48× | 0.05× | same |
| Q15 | 143.7 | 22.9 | 110.2 | 1.30× | 0.21× | same |
| Q16 | 134.5 | 37.9 | 320.9 | 0.42× | 0.12× | same |
| Q17 | 119.0 | 23.7 | 304.4 | 0.39× | 0.08× | differs (ties at the LIMIT) |
| Q18 | 258.3 | 162.0 | 346.1 | 0.75× | 0.47× | differs (ties at the LIMIT) |
| Q19 | 119.3 | 3.2 | 72.8 | 1.64× | 0.04× | same |
| Q20 | 134.1 | 7.5 | 94.8 | 1.41× | 0.08× | same |
| Q21 | 127.1 | 6.8 | 112.9 | 1.13× | 0.06× | same |
| Q22 | 132.1 | 6.7 | 143.0 | 0.92× | 0.05× | same rows (ties ordered differently) |
| Q23 | 172.6 | 131.4 | 96.5 | 1.79× | 1.36× | same rows (ties ordered differently) |
| Q24 | 133.0 | 5.6 | 84.7 | 1.57× | 0.07× | same rows (ties ordered differently) |
| Q25 | 127.8 | 6.0 | 77.7 | 1.64× | 0.08× | same |
| Q26 | 142.5 | 6.3 | 78.1 | 1.83× | 0.08× | same |
| Q27 | 163.1 | 40.7 | 143.7 | 1.14× | 0.28× | same |
| Q28 | 396.4 | 249.8 | 2,948.6 | 0.13× | 0.08× | differs (ties at the LIMIT) |
| Q29 | 332.8 | 235.8 | 397.2 | 0.84× | 0.59× | same |
| Q30 | 139.5 | 17.6 | 120.4 | 1.16× | 0.15× | differs (ties at the LIMIT) |
| Q31 | 154.4 | 35.3 | 127.3 | 1.21× | 0.28× | differs (ties at the LIMIT) |
| Q32 | 557.3 | 418.3 | 1,094.6 | 0.51× | 0.38× | differs (ties at the LIMIT) |
| Q33 | 246.7 | 140.3 | 633.5 | 0.39× | 0.22× | same |
| Q34 | 255.4 | 157.8 | 627.5 | 0.41× | 0.25× | same |
| Q35 | 139.9 | 41.6 | 139.8 | 1.00× | 0.30× | same rows (ties ordered differently) |
| Q36 | 180.9 | 70.8 | 276.9 | 0.65× | 0.26× | same |
| Q37 | 131.8 | 20.5 | 159.4 | 0.83× | 0.13× | same |
| Q38 | 115.3 | 10.0 | 133.0 | 0.87× | 0.08× | differs (ties at the LIMIT) |
| Q39 | 362.5 | 235.0 | 566.5 | 0.64× | 0.41× | differs (ties at the LIMIT) |
| Q40 | 124.3 | 12.4 | 213.4 | 0.58× | 0.06× | differs (ties at the LIMIT) |
| Q41 | 138.1 | 9.5 | 205.5 | 0.67× | 0.05× | same |
| Q42 | 148.0 | 18.7 | 150.3 | 0.98× | 0.12× | same |
| geometric mean (43 queries) | 156.0 | 19.1 | 176.3 | 0.89× | 0.11× |  |
| load (COPY, ANALYZE; s) | 7.9 | 6.4 | 8.6 |  |  |  |

Every result agrees, but for ties at a `LIMIT`.  A column table 0.11× of
PostgreSQL's time in the geometric mean (none above 1.4×: Q23, `SELECT
*` … `ORDER BY` a large text, 1.36×); a row table 0.89× (none above
1.9×).  The loads 7.9 s (rows) and 6.4 s (columns) against 8.6 s.

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
| ClickBench 1M rows, column table: geometric mean ≤ 0.5× | 0.11× | yes |
| column tables: no TPC-H or ClickBench query above 3× | the highest TPC-H Q19 1.93×, ClickBench Q23 1.36× | yes |
| row tables: geometric mean ≤ 2× (TPC-H, ClickBench) | TPC-H 0.77×, ClickBench 0.89× | yes |
| loads ≤ 2× (`bench/run.py`, TPC-H, ClickBench) | 1.1×; TPC-H 0.62× row, 0.55× column; ClickBench 0.92×, 0.74× | yes |
| updates and deletes ≤ 2× (`bench/run.py`) | update 0.9×, delete 1.5× | yes |
| scans, aggregates, sorts and joins ≤ 3× (`bench/run.py`) | 0.6–1.8× (sort 1.8×, count(*) 1.4×, self-join 0.6×) | yes |
| reads by key ≤ 1× (`bench/run.py`) | 0.4× | yes |

Ratios are best-of-three times on one machine in one run; the CI's
shared runners are too noisy to fail on them yet, so a target is checked
here, by a local run.
