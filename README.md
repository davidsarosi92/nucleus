# nucleus

A layered, multi-model database written in Rust from scratch.  Its core is a
transactional, ordered key-value engine; SQL and document models are built on
top of it, and a distributed implementation of the same KV contract comes
later — without changing the layers above.

**Status:** the core (PLAN.md §F0) and the SQL layer (§F1) exist: a
crash-safe copy-on-write B+tree with a write-ahead log, MVCC transactions
(snapshot isolation and serializable), the KV contract, a PostgreSQL-like
SQL dialect (joins of every kind, CTEs, foreign keys; integers by width,
`real` and `double`, `numeric`, text and `char(n)`, `bytea` — large
values stored out of line —, date, time and timestamps, intervals,
`inet` / `cidr`, `uuid`, enums and arrays) with a cost-based optimizer, its own system catalog
(`nc_catalog`), and a server that speaks nucleus's own protocol (`ncp`;
results stream, read a batch at a time through a cursor, or columnar —
straight into numpy, Arrow, pandas and polars) —
with drivers for Rust, C, C++, Python, JavaScript/TypeScript and Java, and
the `ncsql` shell — and, optionally, PostgreSQL's.  It is modular
(§M14.19): every list is a registry, types are modules with abilities the
engine asks before relying on them (`uuid` the first), orderings,
operators and casts can be taught in SQL, and modules are folders built
to leave the repository — PostgreSQL compatibility is one,
[`modules/pg-compat`](modules/pg-compat); writing one:
[`examples/extension`](examples/extension).  Identity and access is in
(§M14.25): users, roles, privileges, row-level security, SCRAM-SHA-256
logins — the password never crosses the wire — tokens for programs and
agents, TLS and client certificates ([`modules/tls`](modules/tls)),
OpenID Connect ([`modules/oidc`](modules/oidc)), LDAP
([`modules/ldap`](modules/ldap)), access rules by where and how a client
connects, protection (locked logins, limits, budgets) and a hash-chained
audit log.  So is the SQL analyzer (§M14.9): `EXPLAIN ANALYZE`, statement
statistics, a linter (`ANALYZE QUERY`) and an index advisor.  AI models
pick it up through `nucleus mcp`, a Model Context Protocol server
([`modules/mcp`](modules/mcp)); comments on tables and columns explain
the schema, errors come with a hint, and [`docs/llm.md`](docs/llm.md) is
the dialect in brief (§M14.10).  Large data is in (§M14.11):
results stream and come as columns (straight into numpy, Arrow, pandas,
polars), `COPY` and `load` bring data in, transactions and operators
past their memory spill to disk, and [`docs/benchmarks.md`](docs/benchmarks.md)
measures it all against PostgreSQL.  Analytics is in (§M14.20):
scans, filters and aggregates run a batch at a time through kernels
compiled per CPU (AVX2, AVX-512, NEON) and on every core, with the
results of a single row-at-a-time pass; a table can be stored as
compressed column segments (`WITH (storage = column)`).  On TPC-H (SF 1)
column tables take 0.24× PostgreSQL's time in the geometric mean (row
tables 0.66×), on ClickBench (a million rows) 0.10× (row 0.82×), loads
faster than PostgreSQL's, every result the same.  Vectors have begun (§M14.12): `vector(n)` with pgvector's
operators, exact nearest-neighbour search through SIMD kernels whose
results are the same on every CPU — the ten nearest of 100,000
768-dimension vectors in 36 ms, faster than numpy's brute force and 4.6×
pgvector's exact search (`docs/benchmarks.md`) —, a native float
array in every driver (in Python an `(n, d)` numpy array or Arrow
`fixed_size_list`), pgvector's own clients over the PostgreSQL
protocol, and estimates for a distance bound — or any condition the
statistics cannot estimate — counted on rows `ANALYZE` keeps; an HNSW
index (`CREATE INDEX … USING hnsw (v vector_cosine_ops)`, pgvector's
operator classes and options) whose storage was chosen by measuring
three models on the KV (`bench/ann/`), kept by every write without
conflicts and searched by `ORDER BY v <=> $1 LIMIT k` where that costs
less; on SIFT-1M through SQL, at pgvector's recall, half its time a
search and 2× its searches a second, 2× its inserts, and a search while
they come in 0.9 ms against its 1.6 — and filtered: `ef_search` counted
among the rows that pass, a filter's B-tree range searched exactly from
the vectors in memory, row-level security a filter like any other, so
recall holds from 50 % of the rows to 0.01 % (a 1 % filter 1.4 ms at
recall 1.0, pgvector's 14.5 ms at 0.987), and `PARTITION BY (tenant)`
keeps a graph per tenant (a tenth of the rows: 0.46 ms against
pgvector's 2.4), and quantized graphs keep `int8`, half or one-bit codes
(`int8`: a quarter of the memory, SIFT-1M built in 29 s against 55 and
searched faster at the same recall; on OpenAI's 1536-dimensional
embeddings built in 11.8 s against pgvector's 137 and searched in 0.63
ms against its 1.57 at recall 0.95), and sparse vectors (`sparsevec`,
pgvector's, a type of its own in every driver) are searched exactly; the
query vector may be a row's (`ORDER BY v <=> (SELECT v FROM t WHERE id =
$1)`, "more like this", 1.1 ms against pgvector's 1.5) and `LATERAL`
searches per row on the workers (1,000 rows' ten nearest in 154 ms
against pgvector's 1,442; `docs/benchmarks.md`); a hash join whose right side turns out
smaller than estimated looks its left side up instead (an adaptive
join).  Statistics have begun (§M14.27): sums of doubles kept exactly
and rounded once, so a parallel `sum` gives the same bits every run, and
`variance`, `stddev`, `skewness`, `kurtosis`, `corr` and linear
regression (`regr_*`) — PostgreSQL's digits over integers and decimals,
over doubles from exact sums —, and window functions (`OVER`, every
aggregate over any frame) over a parallel sort that spills past
`work_mem` and is left out when a primary key's scan already gives the
order, and exact quantiles (`percentile_cont`, `percentile_disc`,
`mode` and `median`, by selection), `width_bucket` and `histogram`,
and sketches that give the same answer every run
(`approx_count_distinct`, `approx_percentile`, `approx_top_k`, and the
`hll` and `ddsketch` types to keep and merge), and samples
(`TABLESAMPLE … REPEATABLE`, the same rows everywhere; `hash_unit` for
stable train/test splits; `reservoir_sample`), and tests for drift
named as scipy names them (`ks_test`, `auc`, `psi`, `js_divergence`,
`chisq_test`), and `SUMMARIZE t` — every column's range, distinct
count, mean, quartiles and NULLs in one scan (also an MCP tool).
Full-text search has begun (§M14.21): text analyzers registered by name
— Unicode words, Porter's stemmer as Lucene has it, Snowball's stemmers
and stop words for thirteen languages (Hungarian among them), trigrams
— `tokenize(text, analyzer)` (FiQA's 44 MB in 0.3 s, PostgreSQL's
`to_tsvector` 1.4 s), and `body @@ 'stock AND "market cap" -crypto
invest*'` in Lucene's query syntax (phrases, prefixes, fuzzy terms),
4–9× PostgreSQL's speed without an index — and a text index (`CREATE
INDEX … USING text (body) WITH (analyzer = 'english')`) exact in every
snapshot, its counts and statistics those of the snapshot's rows,
4–8× PostgreSQL's GIN on terms and 460× on phrases.  Next are BM25
ranking and hybrid search
(§M14.21) and the rest of the
developer platform (§M14.12–§M14.28: columnar storage and partitions, vectors, full-text and hybrid search, embeddings, embedded editions for Python, JavaScript, the
browser and phones, memory for AI models, branches, time travel, a B+tree
or an LSM tree chosen per table, …), then
the document model (§F2).  See
[PLAN.md](PLAN.md) for the roadmap and [DOCS.md](DOCS.md) for what exists
today.

## Using it

```sh
nucleus kv ./mydb put greeting "hello world"
nucleus kv ./mydb get greeting
nucleus kv ./mydb            # interactive shell: begin, put, get, scan, commit, …
```

SQL, and a server that nucleus's drivers ([`drivers/`](drivers), with
[`examples/`](examples) in every language) and `ncsql` connect to — and,
with `--pg-compat`, `psql` and PostgreSQL drivers too:

```sh
nucleus sql ./mydb -c "CREATE TABLE t (id INT PRIMARY KEY, name TEXT)"
ncsql ./mydb                            # SQL shell (same as `nucleus sql`)
nucleus server ./mydb                   # nucleus protocol on 127.0.0.1:20035
ncsql -c '\d t'                         # the shell, connected (user and password: nucleus)
nucleus server ./mydb --pg-compat       # also PostgreSQL's protocol on 5432
PGPASSWORD=nucleus psql -h 127.0.0.1 -U nucleus -c '\d t'
```

For an AI assistant (any MCP client), point it at `nucleus mcp`:

```json
{ "mcpServers": { "nucleus": { "command": "nucleus", "args": ["mcp", "./mydb"] } } }
```

nucleus describes itself in its own catalog, `nc_catalog`
(`SELECT * FROM nc_catalog.tables`); `pg_catalog` exists for PostgreSQL
clients (DOCS.md §8.11).

As a Rust library: `nucleus_kv::Database::open(dir, Options::default())`,
then `begin` / `get` / `scan` / `put` / `commit` (see DOCS.md §6).

The repository: `crates/` is the core, `modules/` the modules (each built
to leave the repository as its own package — PostgreSQL compatibility is
`modules/pg-compat`), `editions/` the builds that choose them, `drivers/`
the language drivers (DOCS.md §1).

## Building

```sh
cargo test --workspace
```

The toolchain is pinned in `rust-toolchain.toml`; rustup installs it on
first use.  nucleus builds for Linux, macOS, Windows and
[d-os](https://github.com/davidsarosi92/d-os) (PLAN.md, "Build targets").

## License

MIT — see [LICENSE](LICENSE).
