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


2026-11-06, SF 1, the machine above (quiet: load average under 4), nucleus after
the adaptive join (§M14.12); each query's best of three runs; nucleus with every table in rows, and with all but
`region` and `nation` in columns; PostgreSQL 18.6 in the same run:

| TPC-H SF 1 | nucleus (row) (ms) | nucleus (column) (ms) | postgresql (ms) | nucleus (row) / PostgreSQL | nucleus (column) / PostgreSQL | results |
|---|---:|---:|---:|---:|---:|---|
| Q1 | 344.3 | 139.4 | 999.8 | 0.34× | 0.14× | same |
| Q2 | 23.1 | 22.7 | 177.1 | 0.13× | 0.13× | same |
| Q3 | 381.5 | 66.0 | 298.3 | 1.28× | 0.22× | same |
| Q4 | 186.0 | 62.3 | 153.1 | 1.21× | 0.41× | same |
| Q5 | 660.9 | 66.1 | 462.0 | 1.43× | 0.14× | same |
| Q6 | 277.7 | 63.1 | 195.9 | 1.42× | 0.32× | same |
| Q7 | 411.7 | 77.6 | 2,906.4 | 0.14× | 0.03× | same |
| Q8 | 174.6 | 113.1 | 188.4 | 0.93× | 0.60× | same |
| Q9 | 491.4 | 232.0 | 1,404.7 | 0.35× | 0.17× | same |
| Q10 | 452.6 | 122.8 | 335.3 | 1.35× | 0.37× | same |
| Q11 | 180.6 | 24.4 | 37.1 | 4.87× | 0.66× | same |
| Q12 | 305.9 | 56.7 | 309.9 | 0.99× | 0.18× | same |
| Q13 | 117.9 | 58.4 | 340.1 | 0.35× | 0.17× | same |
| Q14 | 255.2 | 49.6 | 204.6 | 1.25× | 0.24× | same |
| Q15 | 258.2 | 56.3 | 211.0 | 1.22× | 0.27× | same |
| Q16 | 139.2 | 91.3 | 140.9 | 0.99× | 0.65× | same |
| Q17 | 16.6 | 27.4 | 470.9 | 0.04× | 0.06× | same |
| Q18 | 383.7 | 148.5 | 1,673.7 | 0.23× | 0.09× | same |
| Q19 | 36.8 | 33.8 | 21.1 | 1.74× | 1.60× | same |
| Q20 | 181.3 | 153.8 | 280.5 | 0.65× | 0.55× | same |
| Q21 | 477.8 | 399.7 | 1,625.5 | 0.29× | 0.25× | same |
| Q22 | 52.3 | 13.8 | 32.7 | 1.60× | 0.42× | same |
| geometric mean (22 queries) | 187.8 | 69.4 | 286.2 | 0.66× | 0.24× |  |
| load (COPY, indexes, ANALYZE; s) | 22.4 | 19.9 | 28.6 |  |  |  |

Every result agrees with PostgreSQL's.  Column tables 0.24× PostgreSQL's
time in the geometric mean, faster on 21 of the 22 queries; row tables
0.66×, faster on 12.  The loads — `COPY`, the indexes and `ANALYZE` —
22.4 s (rows) and 19.9 s (columns) against 28.6 s.

Where the time still goes:

- **Q19** on column tables (1.60×, 34 ms against 21): its `OR` of three
  conjunctions over `part` and `lineitem`, ~14,000 `lineitem` rows each
  fetched by key from the column table, where PostgreSQL reads a heap
  tuple each.
- **Row tables** (0.66×): Q11 4.9×, Q19 1.7×, Q22 1.6×, Q5 and Q6
  1.4×, Q10 1.35× the highest — a row's columns decoded where a column table
  reads only the wanted ones.
- **Q8** on column tables (113 ms here; 79–91 ms in most loads): the
  `part`–`lineitem` join is a near tie between hashing `lineitem`'s
  matches and looking them up per `part` row, decided by `ANALYZE`'s
  sample — about one load in four looks them up, which takes 100–140 ms
  where the hash join takes 88.  The lookups are costed ~10 % too cheap
  there; the same before the adaptive join (PLAN.md §M14.12).

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


2026-11-06, the first million rows, nucleus after the adaptive join, as the
TPC-H run above; each query's best of three runs; PostgreSQL 18.6 in the
same run:

| ClickBench, 1,000,000 rows | nucleus (row) (ms) | nucleus (column) (ms) | postgresql (ms) | nucleus (row) / PostgreSQL | nucleus (column) / PostgreSQL | results |
|---|---:|---:|---:|---:|---:|---|
| Q0 | 100.5 | 2.5 | 71.7 | 1.40× | 0.04× | same |
| Q1 | 131.4 | 2.7 | 78.0 | 1.69× | 0.03× | same |
| Q2 | 104.7 | 3.1 | 87.4 | 1.20× | 0.04× | same |
| Q3 | 95.5 | 2.7 | 72.4 | 1.32× | 0.04× | same |
| Q4 | 103.9 | 14.6 | 224.6 | 0.46× | 0.07× | same |
| Q5 | 113.4 | 12.0 | 370.3 | 0.31× | 0.03× | same |
| Q6 | 151.2 | 6.4 | 72.7 | 2.08× | 0.09× | same |
| Q7 | 139.5 | 2.9 | 78.9 | 1.77× | 0.04× | same |
| Q8 | 152.9 | 22.3 | 358.7 | 0.43× | 0.06× | same |
| Q9 | 193.3 | 67.4 | 481.1 | 0.40× | 0.14× | same |
| Q10 | 143.9 | 5.6 | 81.5 | 1.76× | 0.07× | same |
| Q11 | 99.6 | 5.8 | 87.8 | 1.13× | 0.07× | differs (ties at the LIMIT) |
| Q12 | 115.5 | 11.2 | 93.3 | 1.24× | 0.12× | same |
| Q13 | 118.6 | 20.6 | 143.0 | 0.83× | 0.14× | same |
| Q14 | 106.4 | 11.3 | 256.7 | 0.41× | 0.04× | same |
| Q15 | 110.6 | 19.8 | 112.9 | 0.98× | 0.18× | same |
| Q16 | 127.0 | 30.5 | 322.1 | 0.39× | 0.09× | same |
| Q17 | 134.8 | 23.8 | 307.8 | 0.44× | 0.08× | differs (ties at the LIMIT) |
| Q18 | 229.2 | 132.9 | 347.0 | 0.66× | 0.38× | differs (ties at the LIMIT) |
| Q19 | 111.5 | 3.3 | 73.9 | 1.51× | 0.04× | same |
| Q20 | 113.0 | 7.5 | 100.6 | 1.12× | 0.07× | same |
| Q21 | 120.5 | 6.0 | 113.9 | 1.06× | 0.05× | same |
| Q22 | 112.1 | 6.7 | 144.4 | 0.78× | 0.05× | same rows (ties ordered differently) |
| Q23 | 171.9 | 129.8 | 98.2 | 1.75× | 1.32× | same rows (ties ordered differently) |
| Q24 | 129.7 | 5.4 | 86.0 | 1.51× | 0.06× | differs (ties at the LIMIT) |
| Q25 | 103.2 | 6.3 | 83.2 | 1.24× | 0.08× | same |
| Q26 | 102.7 | 6.4 | 79.5 | 1.29× | 0.08× | same |
| Q27 | 132.4 | 36.9 | 144.0 | 0.92× | 0.26× | same |
| Q28 | 333.4 | 235.7 | 2,969.2 | 0.11× | 0.08× | differs (ties at the LIMIT) |
| Q29 | 347.2 | 236.2 | 398.8 | 0.87× | 0.59× | same |
| Q30 | 136.7 | 15.3 | 119.3 | 1.15× | 0.13× | differs (ties at the LIMIT) |
| Q31 | 151.1 | 28.6 | 127.4 | 1.19× | 0.22× | differs (ties at the LIMIT) |
| Q32 | 454.1 | 304.0 | 1,109.2 | 0.41× | 0.27× | differs (ties at the LIMIT) |
| Q33 | 285.4 | 128.3 | 637.2 | 0.45× | 0.20× | same |
| Q34 | 260.9 | 130.5 | 638.2 | 0.41× | 0.20× | same |
| Q35 | 155.6 | 37.4 | 144.6 | 1.08× | 0.26× | same rows (ties ordered differently) |
| Q36 | 171.6 | 57.4 | 277.5 | 0.62× | 0.21× | same |
| Q37 | 137.4 | 17.5 | 161.4 | 0.85× | 0.11× | same |
| Q38 | 142.1 | 9.6 | 136.3 | 1.04× | 0.07× | differs (ties at the LIMIT) |
| Q39 | 309.4 | 188.7 | 557.7 | 0.55× | 0.34× | differs (ties at the LIMIT) |
| Q40 | 123.4 | 10.6 | 209.7 | 0.59× | 0.05× | differs (ties at the LIMIT) |
| Q41 | 126.1 | 9.1 | 204.4 | 0.62× | 0.04× | same |
| Q42 | 126.0 | 19.0 | 148.9 | 0.85× | 0.13× | same |
| geometric mean (43 queries) | 146.1 | 17.9 | 178.7 | 0.82× | 0.10× |  |
| load (COPY, ANALYZE; s) | 7.8 | 6.5 | 9.0 |  |  |  |

Every result agrees, but for ties at a `LIMIT`.  A column table 0.10× of
PostgreSQL's time in the geometric mean (none above 1.4×: Q23, `SELECT
*` … `ORDER BY` a large text, 1.32×); a row table 0.82× (none above
2.1×: Q6 2.08×, Q7 1.77×).  The loads 7.8 s (rows) and 6.5 s (columns)
against 9.0 s.

## Vectors

In short — each measured below, against pgvector 0.8 (PostgreSQL 18, in
Docker) on the same machine, 2026-11-06/09:

| | nucleus | pgvector | |
|---|---:|---:|---:|
| exact search, 100,000 × 768 | 36 ms | 167 ms | 4.6× |
| SIFT-1M at recall ~0.93, a search (through SQL) | 442 µs | 894 µs | 2.0× |
| SIFT-1M, inserts by 10 writers | 4,806 rows/s | 2,141 rows/s | 2.2× |
| SIFT-1M, a search while they come | 885 µs | 1,571 µs | 1.8× |
| OpenAI 1536-d, built (99,000 rows) | 42.5 s; `int8` 11.8 s | 136.7 s | 3.2×; 11.6× |
| OpenAI 1536-d at recall ~0.95, the server's time | 1,149 µs; `int8` 634 µs | 1,571 µs | 1.4×; 2.5× |
| a 1 % filter, recall | 1.4 ms, 1.0 | 14.5 ms, 0.987 | 10× |
| a tenant's tenth (`PARTITION BY`) | 0.46 ms, 0.939 | 2.4 ms, 0.938 | 5× |
| "more like this" by a row's id, OpenAI 1536-d | 1.10 ms, recall 0.965 | 1.48 ms, 0.975 | 1.3× |
| each of 1,000 rows' ten nearest (`LATERAL`) | 154 ms, recall 0.982 | 1,442 ms, 0.987 | 9.4× |
| sparse vectors, exact, 100,000 | 20.3 ms | 17.9 ms | 0.88× |

Where pgvector is ahead: it reaches a recall at a lower `ef_search`
(nucleus needs about 1.5× the `ef` for the same recall, still less time);
a 0.01 % filter whose second condition no index covers (471 against 498
µs); and sparse exact searches, where its row reads cost less (both read
every row on several workers; PLAN.md §M14.12 V7.3).

Exact nearest neighbours (PLAN.md §M14.12; `bench/vectors/knn.py
target/release/nucleus --pg`): 100,000 random vectors of 768 dimensions
(293 MiB), the ten nearest of 20 queries, median; nucleus with a 1 GB
page cache (`--cache 1GB`) as pgvector has `shared_buffers = 1GB`;
nucleus and numpy 2026-11-09 (§M14.12 V7: the vectors in their rows),
pgvector 2026-11-06.

| | ms | GiB/s read | answers differing from numpy's |
|---|---:|---:|---:|
| numpy brute force (one core) | 41 | 7.0 | — |
| nucleus `ORDER BY v <-> $1 LIMIT 10` | 36 | 7.9 | 0 of 20 |
| pgvector (PostgreSQL 18, no index) | 167 | 1.7 | 0 of 20 |

Loading the 100,000 vectors: nucleus 21 s (Python `load`, a list of
numpy arrays), pgvector 42 s (`COPY` of text through `psql`).

Approximate nearest neighbours — the HNSW index (PLAN.md §M14.12 V4b:
`bench/ann/sql.py`, the server on a fresh database, the Python driver's
clients each in a process; `bench/ann/pgvector_ref.py`, pgvector 0.8 in
Docker with its clients inside the container): ann-benchmarks' SIFT-1M,
`m` 16, `ef_construction` 64, 900,000 rows indexed at once, then 100,000
inserted by 10 writers (a row a transaction) while one client searches;
2,000 queries, `SELECT id FROM t ORDER BY v <-> $1 LIMIT 10`, recall@10,
µs a search on one client, searches a second on 10; 2026-11-07.

| | nucleus | pgvector |
|---|---:|---:|
| indexed 900,000 rows | 53.8 s | 58.8 s |
| recall ~0.93: µs a search, searches a second | 442 µs, 10,637 (`ef` 60, 0.933) | 894 µs, 5,564 (`ef` 40, 0.927) |
| recall ~0.99 | 903 µs, 6,596 (`ef` 200, 0.989) | 2,818 µs (`ef` 200, 0.993) |
| inserts by 10 writers | 4,806 rows/s | 2,141 rows/s |
| a search meanwhile (`ef` 40) | 885 µs (recall 0.89) | 1,571 µs (0.93) |

Both include SQL — parsing, planning, the protocol on a local socket;
nucleus's round trip alone (`SELECT 1` from the Python driver) is ~60 µs.
While the writers run the upkeep is theirs — seals, folds, compactions
apart from the folds — and a search scans only the few hundred rows not
yet in the graph; after the inserts it is at 463 µs (`ef` 60, recall
0.930).

```
target/ann-data/.venv/bin/python bench/ann/data.py      # the datasets
target/ann-data/.venv/bin/python bench/ann/sql.py target/release/nucleus \
  target/ann-data/sift-128-euclidean --efs 10,20,40,60,80,120,200
target/ann-data/.venv/bin/python bench/ann/pgvector_ref.py target/ann-data/sift-128-euclidean
```

Filtered searches (PLAN.md §M14.12 V5: `bench/ann/filtered.py`): the
same 1,000,000 rows indexed at once, a column `a` (0–999, from a hash of
the row's id: independent of its vector) with a B-tree index, `ANALYZE`d;
500 queries, `SELECT id FROM t WHERE … ORDER BY v <-> $1 LIMIT 10` at
`ef_search` 40, recall@10 against the exact answer among the rows that
pass, µs a search on one client; pgvector with `hnsw.iterative_scan =
relaxed_order` (without it, 4.1 and 0.44 rows of 10 come back at 10 % and
1 %); 2026-11-07.

| filter (share of rows) | nucleus | | pgvector | |
|---|---:|---:|---:|---:|
| | recall | µs | recall | µs |
| `a < 500` (50 %) | 0.926 | 587 | 0.891 | 1,021 |
| `a < 100` (10 %) | 0.982 | 1,760 | 0.932 | 2,533 |
| `a < 10` (1 %) | 1.0 | 1,425 | 0.987 | 14,477 |
| `a = 7` (0.1 %) | 1.0 | 313 | 1.0 | 604 |
| `a = 7 AND id % 10 = 3` (0.01 %) | 1.0 | 516 | 1.0 | 471 |

nucleus counts `hnsw.ef_search` among the rows that pass (recall holds
at every share) and searches a filter's B-tree range exactly, from the
vectors in the index's memory, when it holds up to ~33,000 rows at `ef`
40 (1 % and below here); pgvector searches the graph and filters (50–1
%) or uses the B-tree and sorts (0.1 % and below).

Groups (`--partition`): a column `g = a / 100`, ten groups of 100,000;
nucleus with `CREATE INDEX … USING hnsw (v) PARTITION BY (g)` (a graph
per group: indexed in 45 s against 67 s for one) against one graph, and
pgvector's one graph:

| filter | one graph | | `PARTITION BY (g)` | | pgvector | |
|---|---:|---:|---:|---:|---:|---:|
| | recall | µs | recall | µs | recall | µs |
| `g = 3` (10 %) | 0.981 | 1,882 | 0.939 | 455 | 0.938 | 2,352 |
| `g = 3 AND a BETWEEN 300 AND 309` (1 %) | 1.0 | 1,476 | 1.0 | 1,405 | 0.988 | 14,224 |

```
target/ann-data/.venv/bin/python bench/ann/filtered.py nucleus target/release/nucleus \
  target/ann-data/sift-128-euclidean [--partition] [--filters g10,g1]
target/ann-data/.venv/bin/python bench/ann/filtered.py pgvector target/ann-data/sift-128-euclidean
```

Quantized graphs (PLAN.md §M14.12 V6a: `WITH (quantize = …)`, the rows
read again for the exact distances; `bench/ann/sql.py --quantize`;
2026-11-07):

| SIFT-1M (as above) | a vector's bytes | built | `ef` 40: recall | µs | inserts a second | a search while they come |
|---|---:|---:|---:|---:|---:|---:|
| `f32` | 512 | 54.9 s | 0.891 | 363 | 4,862 | 899 µs |
| `half` | 256 | 48.8 s | 0.891 | 405 | 5,033 | 712 µs |
| `int8` | 144 | 29.0 s | 0.892 | 291 | 6,275 | 708 µs |

OpenAI embeddings against pgvector — `text-embedding-3-large`, 99,000 ×
1536, cosine (`bench/ann/data.py dbpedia-openai3-1536-100k`: Qdrant's
set), indexed at once (`--no-inserts`), `m` 16, `ef_construction` 64,
1,000 queries.  pgvector's ways to the same codes are its documented
ones: an expression index on `v::halfvec(1536)`, and on
`binary_quantize(v)::bit(1536)` with the nearest `k · 5` by the bits
re-ranked by the vectors in a subquery.  The time is the server's own
(`EXPLAIN ANALYZE` over 200 queries, the query vector taken by a
subquery on pgvector's side): the clients differ (nucleus's Python
client sends the 6 KB vector, pgbench an id), and pgvector reaches a
recall at a lower `ef_search`, so the rows pair equal recall:

| recall@10 | nucleus `f32` | nucleus `int8` | pgvector `vector` |
|---|---:|---:|---:|
| ~0.95 | 1,149 µs (`ef` 60, 0.951) | **634 µs** (`ef` 60, 0.952) | 1,571 µs (`ef` 40, 0.949) |
| ~0.963 | 1,398 µs (`ef` 80, 0.964) | **704 µs** (`ef` 80, 0.964) | 2,018 µs (`ef` 60, 0.963) |
| ~0.975 | 1,847 µs (`ef` 120, 0.978) | **861 µs** (`ef` 120, 0.978) | 2,408 µs (`ef` 80, 0.974) |
| ~0.99 | 2,843 µs (`ef` 200, 0.989) | **1,149 µs** (`ef` 200, 0.989) | 4,919 µs (`ef` 200, 0.991) |

| codes | a vector's bytes in the graph | built: nucleus | built: pgvector | `ef` 40: recall, server µs — nucleus | pgvector |
|---|---:|---:|---:|---:|---:|
| `f32` / `vector` | 6,144 | 42.5 s | 136.7 s | 0.926, 908 | 0.949, 1,571 |
| `half` / `halfvec` | 3,072 | 35.2 s | 81.2 s | 0.926, 887 | 0.947, 1,229 |
| `int8` (rescore 2) | 1,552 | **11.8 s** | — | 0.929, 556 | — |
| `binary` (rescore 5) / `bit` + re-rank of 50 | 192 | **5.5 s** | 22.7 s | 0.922, 988 | 0.916, 1,099 |
| `binary`, rescore 10 | 192 | 5.3 s | — | 0.964, 2,150 | — |

pgvector's indexes on disk: 773 MB (`vector`), 386 (`halfvec`), 47
(`bit`).  Through the clients (µs a search on one client, as the
harnesses print them): nucleus `f32` 1,400 at `ef` 40, `int8` 1,176,
`binary` 1,504; pgvector 1,522, 1,124 (`halfvec`), 925 (`bit`).  Of
nucleus's binary search, most is the 50 rows it rescores, their vectors
read with them (2026-11-09, PLAN.md §M14.12 V7.2: 1,080–1,107 µs on the
server while a 6 KB vector was a second lookup).

By a stored row's vector, both clients sending an id (`bench/ann/related.py`,
the same 99,000 rows, `ef_search` 40; 2026-11-09, §M14.12 V7):

| | nucleus | pgvector |
|---|---:|---:|
| "more like this": `ORDER BY v <=> (SELECT v FROM t WHERE id = $1) LIMIT 10`, the row left out — µs a search, the server's | 1,097, 884 | 1,478, 1,554 |
| recall@10 | 0.965 | 0.975 |
| each of 1,000 rows' ten nearest: `FROM t a, LATERAL (SELECT … ORDER BY b.v <=> a.v LIMIT 10)` | **154 ms** (865 on one thread) | 1,442 ms |
| recall@10 | 0.982 | 0.987 |

nucleus runs the lateral side's searches on its workers, a row each;
PostgreSQL's nested loop runs its inner index scans on one process.

```
target/ann-data/.venv/bin/python bench/ann/data.py dbpedia-openai3-1536-100k
target/ann-data/.venv/bin/python bench/ann/sql.py target/release/nucleus \
  target/ann-data/sift-128-euclidean --efs 10,20,40,60,80,120,200 --quantize int8
target/ann-data/.venv/bin/python bench/ann/sql.py target/release/nucleus \
  target/ann-data/dbpedia-openai3-1536-100k --no-inserts --bulk 99000 --queries 1000 \
  --efs 10,20,40,80,200 --server-efs 40,60,80,120,200 --quantize int8
target/ann-data/.venv/bin/python bench/ann/pgvector_ref.py target/ann-data/dbpedia-openai3-1536-100k \
  --no-inserts --bulk 99000 --queries 1000 --efs 10,20,40,60,80,200 \
  --server-efs 20,40,60,80,200 [--quantize half | --quantize binary --rescore 5]
target/ann-data/.venv/bin/python bench/ann/related.py nucleus target/release/nucleus \
  target/ann-data/dbpedia-openai3-1536-100k
target/ann-data/.venv/bin/python bench/ann/related.py pgvector target/ann-data/dbpedia-openai3-1536-100k
```

Sparse vectors searched exactly (PLAN.md §M14.12 V6b:
`bench/ann/sparse.py`): 100,000 SPLADE-like rows — a vocabulary of
30,522, ~120 terms a document and ~20 a query by a Zipf law, log-normal
weights — as `sparsevec(30522)`; 200 queries, `SELECT id FROM t ORDER BY
v <#> q LIMIT 10`, one client; nucleus 2026-11-09, pgvector 2026-11-07.

| | recall | ms a search | how |
|---|---:|---:|---|
| nucleus | 1.0 | 20.3 | a scan and a top-k per morsel on the workers (67 ms on one) |
| pgvector 0.8 | 1.0 | 17.9 | a parallel scan, two workers and the leader |

```
target/ann-data/.venv/bin/python bench/ann/sparse.py nucleus target/release/nucleus
target/ann-data/.venv/bin/python bench/ann/sparse.py pgvector
```

The storage models behind the index (`bench/ann/`, calls in a process,
PLAN.md §M14.12 V4) are recorded in the plan.

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

## Text analysis

`bench/text/analyzers.py` (PLAN.md §M14.21 T1, T2, T4): BEIR FiQA's 57,638
documents (title and text, 44 MB) in a table, each analyzer's terms
counted over every document through SQL — `sum(cardinality(tokenize(body,
a)))` —, best of three; PostgreSQL 18 (container `ncpg`, the same
machine, through `psql`) the same over `to_tsvector`, which counts
distinct terms a document.

| Analyzer | nucleus, 1 thread | nucleus, workers | PostgreSQL, 1 | PostgreSQL, parallel |
|---|---:|---:|---:|---:|
| `simple` (words, lower case) | 1,058 ms | 177 ms | 3,176 ms | 1,138 ms |
| `porter` (Lucene's English) | 2,053 ms | 307 ms | — | — |
| `english` (Snowball) | 2,105 ms | 309 ms | 3,915 ms | 1,383 ms |
| `hungarian` (Snowball) | 2,059 ms | 310 ms | 4,319 ms | 1,521 ms |
| `trigram` | 2,796 ms | 449 ms | — | — |

Text queries without an index — `count(*) WHERE body @@ q` against
PostgreSQL's `to_tsvector('simple', body) @@ q::tsquery`, same corpus:

| Query | nucleus, 1 thread | nucleus, workers | PostgreSQL, 1 | PostgreSQL, parallel |
|---|---:|---:|---:|---:|
| `market` | 779 ms | 122 ms | 3,252 ms | 1,139 ms |
| `stock market` (either) | 786 ms | 121 ms | 3,258 ms | 1,138 ms |
| `stock AND market` | 791 ms | 128 ms | 3,249 ms | 1,140 ms |
| `"stock market"` | 784 ms | 127 ms | 3,239 ms | 1,145 ms |
| `invest*` | 778 ms | 119 ms | 3,231 ms | 1,139 ms |
| `markte~1` (fuzzy) | 880 ms | 140 ms | — | — |

With an index — `CREATE INDEX … USING text (body)` against PostgreSQL's
GIN on `to_tsvector('simple', body)` (its server's execution time, ours
through the protocol), best of five:

| | nucleus | PostgreSQL GIN |
|---|---:|---:|
| `CREATE INDEX` | 2,628 ms | 3,145 ms |
| `count(*)` `market` | 1.2 ms | 6.7 ms |
| `stock market` | 1.7 ms | 6.5 ms |
| `stock AND market` | 0.5 ms | 4.1 ms |
| `"stock market"` | 0.6 ms | 274.9 ms |
| `invest*` | 1.9 ms | 7.5 ms |
| `markte~1` | 4.7 ms | — |
| `sum(length(body))` of `market`'s rows | 16.2 ms | 12.1 ms |

```sh
python3 bench/text/data.py beir
NUCLEUS_BENCH_HOST=::1 target/ann-data/.venv/bin/python bench/text/analyzers.py nucleus target/release/nucleus
python3 bench/text/analyzers.py postgres --container ncpg
```

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
| TPC-H SF 1, column tables: geometric mean ≤ 0.8× PostgreSQL's time | 0.24× | yes |
| ClickBench 1M rows, column table: geometric mean ≤ 0.5× | 0.10× | yes |
| column tables: no TPC-H or ClickBench query above 3× | the highest TPC-H Q19 1.60×, ClickBench Q23 1.36× | yes |
| row tables: geometric mean ≤ 2× (TPC-H, ClickBench) | TPC-H 0.66×, ClickBench 0.82× | yes |
| loads ≤ 2× (`bench/run.py`, TPC-H, ClickBench) | 1.0×; TPC-H 0.78× row, 0.70× column; ClickBench 0.92×, 0.74× | yes |
| updates and deletes ≤ 2× (`bench/run.py`) | update 0.7×, delete 1.5× | yes |
| scans, aggregates, sorts and joins ≤ 3× (`bench/run.py`) | 0.5–1.1× (sort 0.5×, count(*) 1.1×, self-join 0.6×) | yes |
| reads by key ≤ 1× (`bench/run.py`) | 0.5× | yes |

Ratios are best-of-three times on one machine in one run; the CI's
shared runners are too noisy to fail on them yet, so a target is checked
here, by a local run.
