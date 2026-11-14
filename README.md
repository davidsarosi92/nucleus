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
`inet` / `cidr`, `uuid`, `json` — one binary type, read in place, with `jsonb`'s operators, SQL/JSON paths and the standard's `JSON_VALUE` / `JSON_QUERY` / `JSON_OBJECT` —, enums and arrays) with a cost-based optimizer, its own system catalog
(`nc_catalog`), and a server that speaks nucleus's own protocol (`ncp`;
results stream, read a batch at a time through a cursor, or columnar —
straight into numpy, Arrow, pandas and polars) —
with drivers for Rust, C, C++, Python, JavaScript/TypeScript, Java and Swift, and
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
pgvector's, a type of its own in every driver) are searched exactly —
through an inverted index, `USING sparse` (SPLADE on BEIR: nDCG@10 at
or above pgvector's approximate graph, NFCorpus 0.343 against 0.321); the
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
Full-text and hybrid search (§M14.21): text analyzers registered by name
— Unicode words, Porter's stemmer as Lucene has it, Snowball's stemmers
and stop words for thirteen languages (Hungarian among them), trigrams
— `tokenize(text, analyzer)` (FiQA's 44 MB in 0.3 s, PostgreSQL's
`to_tsvector` 1.4 s), and `body @@ 'stock AND "market cap" -crypto
invest*'` in Lucene's query syntax (phrases, prefixes, fuzzy terms),
4–9× PostgreSQL's speed without an index — and a text index (`CREATE
INDEX … USING text (body) WITH (analyzer = 'english')`) exact in every
snapshot, its counts and statistics those of the snapshot's rows,
7–14× PostgreSQL's GIN on terms and 550× on phrases — and BM25 ranking,
`ORDER BY score(body @@ q) DESC LIMIT 10`, the first rows found by the
index from its postings: Anserini's nDCG@10 on BEIR to within 0.006,
ranked as well as ParadeDB's Tantivy-based BM25 or better and faster
on every corpus (0.34 ms a FiQA query, theirs 1.6; 0.16 ms on SciFact,
theirs 0.91 — `score()` reads the scores the ranking computed), with
phrases within a slop (`"a b"~3`) and `highlight(body @@ q)` /
`snippet(body @@ q)` (a FiQA query with its rows' snippets 0.97 ms,
ParadeDB's 2.2); on MS
MARCO's 8.8 million passages block-max MaxScore over per-block impacts
ranks a query in 1.8 ms against ParadeDB's 5.5, at a higher nDCG@10 (0.2605, theirs 0.2578), the index built on every core in 42 s (theirs 68)
— and with other conditions, row-level security or
HNSW's distance over the text's matches, one ranking for every index
(a FiQA query with a filter passing a tenth of the rows 0.70 ms, 134×
the read-everything path), and the rows a condition finds read by a merge
with the table's scan (a FiQA term's 8,743 rows 2.6× faster) — and
hybrid search in one query, `ORDER BY rrf(score(body @@ q), emb <=> $1)
DESC LIMIT 10` or `convex(…, α)`, each ranking's first rows from its
index (BEIR: fused above text and vectors alone, 1.3–2.1 ms a SciFact,
NFCorpus or FiQA query, 1.8–5.0× ParadeDB's RRF) — on a row, column or
partitioned table alike, a partitioned table's scores its partitions'
together.  With `--pg-compat`,
PostgreSQL's own text search works as written —
`to_tsvector('english', body) @@ to_tsquery('english', 'cat & !dog')`,
evaluated as PostgreSQL does (5,407 of its answers checked, its types and
their binary forms on PostgreSQL's wire too) and served
by the text index (BEIR: as fast as PostgreSQL's GIN index for all
words, faster for any), with `ts_rank`, `ts_rank_cd` and `ts_headline`
giving PostgreSQL's values (a search page, ten best by `ts_rank` with
their headlines, 1.3–1.4× GIN's speed).  Embeddings are made by the
database (§M14.22): `embed(body, 'm')` with a model of
OpenAI-compatible servers, Cohere, Voyage — or run inside nucleus on its
own kernels, `CREATE MODEL bge USING local WITH (huggingface =
'BAAI/bge-small-en-v1.5')`, its files kept in the database (the same
tokens as Hugging Face's tokenizers, vectors at cosine ≥ 0.9999998 to
ONNX Runtime's, 3–4× fastembed's speed) — and kept current by the table: `v vector
GENERATED BY EMBEDDING (title || ' ' || body) USING bge`, made in the
background through a queue in the database that retries an outage,
with queries choosing to include, skip or wait for rows whose vectors
are pending, and moved to another model online (`ALTER COLUMN v SET
MODEL e5`: the new vectors made beside, taken in one transaction).  `chunks(body, 'markdown', 200, 20, 'bge')` cuts documents by
sentences, tokens or Markdown sections, sized in the model's own tokens,
and `CREATE TABLE parts CHUNKS OF docs (body) BY markdown SIZE 200 USING
bge` keeps a table of them current with the documents (an edited
paragraph re-embeds only its chunk).  A model's limits
(`requests_per_minute`, `tokens_per_minute`, `batch_tokens`, `parallel`)
pace its calls, and what each statement spent on models — calls, tokens,
cache hits, time — is in `EXPLAIN ANALYZE` and the statement statistics.
On BEIR, documents loaded into a server rank as
fastembed's vectors of the same model do, and stay in sync — bit for bit
— through edits, deletes and a killed server (§M14.22).  And it runs
inside a program, no server (§M14.23): `nucleus.connect("file:app.db")`
in Python or JavaScript opens a database in a directory (or
`":memory:"`) in the process, with the API a server's connection has —
the drivers speak the same protocol to it through a pipe in the process,
so a program moves to a server by changing its connection string alone
(`SELECT 1` in 10.5 µs from Python, 30 over loopback TCP) — and in a
browser page, the engine in WebAssembly with its files in OPFS
(`connect("file:app.db")` again; a first answer about 100 ms after the
page starts, the engine 1.65 MB to download), and in iOS and Android apps
(a Swift package and a Kotlin library; the engine adds 7.0 MiB to an iOS
app, 7.45 MiB to an Android one on arm64).  Models keep memories in it
(§M14.24): `CREATE MEMORY STORE notes USING bge` makes a table of
memories — whose, what kind, the text and its vector, confidence, when
valid, when it expires, its source — `REMEMBER 'prefers short answers'
FOR 'ann' IN notes` merges a near-duplicate or adds it (`REPLACING id`
keeps the old one as history), `SELECT * FROM recall('notes', 'ann',
'how to answer')` ranks a subject's current memories by text and vector
together — under a millisecond among 100 000 memories in the process,
many questions in one statement through `LATERAL` —, all under the
caller's privileges and row-level security, each answer carrying the
commit it read so a citation reads back exactly what was cited
(`SET request_id` records what a generation read);
rows expire by a column (`ALTER TABLE t SET EXPIRY (expires_at)`),
`FORGET SUBJECT 'ann'` erases a subject's rows from every table whose
columns are marked as saying whose a row is — all of them or none —,
scrubs every file of their bytes (old versions, index pages, free pages,
the log) and searches the files to prove none is left, recording the
erasure without the subject's data — and reaches its backups too:
`BACKUP TO '/backups/mon'` (online, without index pages; `nucleus
restore` rebuilds them) seals each row of a subject under that subject's
own key, kept apart (`<dir>.subject-keys`), which `FORGET` destroys, so
a restore of an older backup leaves the subject out —, and `nucleus mcp
--memory` gives an agent remember, recall, forget and cite
([`docs/memory.md`](docs/memory.md) walks through an assistant built on
all of it).  It reads the past (§M14.14): with
`nucleus server --history 7d` every commit's time is kept, `BEGIN READ
ONLY AS OF TIMESTAMP now() - interval '1 hour'` reads the whole
database as it was then — its tables' shapes too —, `SELECT … FROM t
AS OF TIMESTAMP '…'` one table beside the present, and `CREATE TABLE
t2 AS SELECT * FROM t AS OF …` restores; checked against a logged
workload through every index kind, at no cost to commits measurable.
Next is the rest of the
developer platform (§M14.12–§M14.28: columnar storage and partitions, vectors, full-text and hybrid search, embeddings, memory for AI models, branches, a B+tree
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

Inside a program, without a server (the embedded edition,
[`editions/embedded`](editions/embedded)) — the same code runs against a
server when the string names one:

```python
import nucleus                                   # the nucleus-db wheel for this platform
db = nucleus.connect("file:app.db")              # or ":memory:", or "nucleus://user:pw@host:20035/nucleus"
db.execute("CREATE TABLE notes (id INT PRIMARY KEY, body TEXT, v vector(3))")
```

```ts
import { connect } from "nucleus-db";            // Node: with the addon (drivers/js/native);
const db = await connect("file:app.db");         // a browser: the engine in WebAssembly, kept in OPFS
```

As a Rust library: `nucleus_kv::Database::open(dir, Options::default())`,
then `begin` / `get` / `scan` / `put` / `commit` (see DOCS.md §6); SQL
sessions through `nucleus_embedded::Database` (DOCS.md §11).

The repository: `crates/` is the core, `modules/` the modules (each built
to leave the repository as its own package — PostgreSQL compatibility is
`modules/pg-compat`), `editions/` the builds that choose them, `drivers/`
the language drivers (DOCS.md §1).

## Building

```sh
cargo test --workspace
```

The toolchain is pinned in `rust-toolchain.toml`; rustup installs it on
first use.  Fuzz targets for every input that crosses a trust boundary
are in `fuzz/` (a nightly toolchain and `cargo-fuzz`; `fuzz/README.md`).  nucleus builds for Linux, macOS, Windows and
[d-os](https://github.com/davidsarosi92/d-os) (PLAN.md, "Build targets").

## Security

Report vulnerabilities privately — see [SECURITY.md](SECURITY.md).

## License

MIT — see [LICENSE](LICENSE).
