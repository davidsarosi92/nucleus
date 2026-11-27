# Acknowledgements

nucleus is its own design, written in its own code — but little in a
database is new, and much of nucleus stands on what other systems,
papers and libraries worked out first.  This file credits them: what
idea was taken, from where, and where in nucleus it landed.  These are
**ideas** (algorithms, layouts, a feature's meaning), re-implemented
here, not code; the few files that did come from elsewhere are listed,
with their licences, in [`THIRD_PARTY.md`](THIRD_PARTY.md).

Section ids (`§M14.34 G3`, …) point to the step in [`PLAN.md`](PLAN.md)
where the borrowing, its measurements, and what was not taken are
recorded in full.  New borrowings are added here as they are made
(`CLAUDE.md`, "Credit what is taken").

PostgreSQL deserves a line of its own: beyond the ideas below, its
answers and errors are the reference most of nucleus's SQL tests are
checked against.

## SQL planner and optimizer

- **PostgreSQL `CREATE STATISTICS`** — column-group distinct counts and functional dependencies in estimates. §M14.20 V12.
- **Yan & Larson, eager aggregation** — grouping by the join key below the join. §M14.20 V13.
- **Neumann & Kemper, "Unnesting Arbitrary Queries" (BTW 2015); Galindo-Legaria & Joshi, SQL Server's Apply removal (SIGMOD 2001)** — a scalar aggregate subquery rewritten as a grouped join. §M14.33 (`unnest`).
- **Oracle 12c adaptive plans; SQL Server 2017 adaptive joins** — hash join or index lookups chosen at run time at the cost model's threshold. §M14.12 V3; §M14.34 G1.
- **Oracle dynamic sampling, SQL Server sampled statistics, PostgreSQL's default selectivities** — estimates from a sample kept at `ANALYZE`, PostgreSQL's constants as the fallback. §M14.12 V3.
- **PostgreSQL `tsmatchsel` / `tsquery_opr_selec`** — `@@` selectivity from the text index's exact counts. §M14.21.
- **PostgreSQL `plancache.c` (and MySQL's and Oracle's shared cursors); Oracle's soft parse** — per-session plan cache keyed by text, invalidated with its relations. §M14.20 W8.
- **SQLite's schema cookie** — one counter bumped by every schema change, made snapshot-aware. §M14.20 W8.
- **SQL Server's and Oracle's `COUNT(DISTINCT key)` simplification** — `DISTINCT` over a one-column primary key dropped. §M14.33 S12.
- **PostgreSQL InitPlan; SQL:1999 `LATERAL` with PostgreSQL 9.3's meaning** — an uncorrelated subquery evaluated once; a lateral join as a parameterized nested loop. §M14.12 V7.1.
- **PostgreSQL `make_sort_input_target`** — costly output expressions computed after `LIMIT`. §M14.21 T8b.
- **PostgreSQL pathkeys; DuckDB's and Umbra's order properties** — a sort skipped where the primary key's order serves. §M14.27.
- **PostgreSQL parallel `SubPlan`; `proparallel`; HyPer / Umbra's dependent join as a pipeline step** — correlated subqueries and lateral functions run on the workers. §M14.20 W6; §M14.15 J3c.
- **PostgreSQL's index access method API (`IndexAmRoutine`)** — the shape of `IndexMethod`. §M14.12 V4b.
- **PostgreSQL expression indexes (`indexprs`)** — the expression's text, type and columns kept in the index record. §M14.15 J4a.
- **PostgreSQL partitioned indexes and Merge Append; Elasticsearch per-shard statistics** — BM25 statistics summed over a partitioned table. §M14.21 T8c; §M14.24 R3.

## Recursive queries and graphs

- **SQL:1999 `SEARCH` / `CYCLE`, as PostgreSQL 14 has them** — written as the rewrite PostgreSQL's documentation gives as their meaning. §M14.34 G2.
- **DuckDB's `USING KEY` (Hirn & Grust, "A Fix for the Fixation on Fixpoints", CIDR 2023; DuckDB 2.0's meaning)** — a recursive query's rows kept as a table by key. §M14.34 G3.
- **HyPer / Umbra's iterations; Datalog engines and Soufflé** — each round chooses its join by the rows the last one gave. §M14.34 G1.
- **SQL/PGQ (ISO/IEC 9075-16:2023)** — `CREATE PROPERTY GRAPH`, `GRAPH_TABLE`, its patterns, labels and defaults, and what they mean. §M14.34 G4.
- **PostgreSQL 19 beta's SQL/PGQ, as its documentation describes it** — `GRAPH_TABLE` rewritten into joins, one per choice of element tables; matches as joins match. §M14.34 G4.
- **DuckPGQ (ten Wolde, Singh, Szárnyas & Boncz, CIDR 2023)** — quantified edges, `ANY SHORTEST` and `path_length` over recursive SQL. §M14.34 G4.

## Executor and vectorization

- **Graefe's Volcano** — the first executor model. §M9.
- **HyPer's morsel-driven parallelism (Leis et al., SIGMOD 2014); DuckDB's thread-local hash tables** — pipelines over morsels on the workers, one table per worker. §M14.20 W6; §M14.33; the perf phase's reads by key, step 2 (morsels sized by a row's cost: two leaves for a pipeline of lookups).
- **Kuiper et al., "Robust External Hash Aggregation in the Solid State Age" (ICDE 2024, DuckDB); ClickHouse two-level tables** — one memory bound for an aggregate, groups spilled by hash parts. §M14.20 W6.
- **DuckDB's salted hash table and perfect hashing; ClickHouse two-level tables** — `keytable`. §M14.20 V13.
- **DuckDB join-filter pushdown** — a hash join's key range, list or Bloom filter narrowing the probe side's scans. §M14.20 V13.
- **Chen & Ailamaki, prefetching in hash probes** — a batch's lookups done before its output. §M14.20 V13.
- **Expression templates (numeric libraries)** — decimal arithmetic fused over typed vectors. §M14.20 V13.
- **DataFusion `GroupsAccumulator`; ClickHouse `sizeOfData`** — typed per-group state, counted against `work_mem`. §M14.20 W6.
- **Calcite `AggregateExpandDistinctAggregatesRule`; Spark `RewriteDistinctAggregates`; DuckDB's and ClickHouse's lone distinct aggregate** — `DISTINCT` aggregates as two groupings. §M14.20 W6; §M14.33 S1.
- **PostgreSQL's bounded (top-N) sort** — a heap below `LIMIT`. §M14.20 W6.
- **Normalized sort keys (System R; Raasveldt, DuckDB, 2021); PostgreSQL abbreviated keys** — keys compared as bytes, an 8-byte prefix as a number. §M14.20 W2/W6; §M14.27.
- **Merge Path (Odeh, Green, Mwassi, Shmueli & Birk, IPDPS 2012); DuckDB's parallel merge sort** — merges split by key range. §M14.27.
- **LSD radix sort (ClickHouse `RadixSort.h`, DuckDB)**, pdqsort beyond it. §M14.27 S8a.
- **Late materialization (Velox lazy vectors; DuckDB's and ClickHouse's `PREWHERE`)** — filter columns first, the rest for rows kept. §M14.33.
- **PostgreSQL `ExecQual`; DuckDB and Velox selection vectors** — conjunctions a term at a time. §M14.33 issue #2.
- **DuckDB's probe that yields a vector and keeps its place** — a hash join's output not held whole. §M14.33.
- **LLVM hot/cold splitting** — cold paths of `Expr::eval` outlined. §M14.20 W6.
- **DuckDB's parallel CSV reader; "Instant Loading"** — `COPY` parsed in parallel, overlapped with storing. §M14.20 W3.
- **PostgreSQL bitmap heap scans; Lucene `advance`** — found keys sorted, the table read once in their order. §M14.21 T5a.

## Storage: KV, B+tree, LSM, branches

- **FoundationDB; CockroachDB and TiDB** — SQL and other layers on an ordered transactional KV. Principle 1.
- **LMDB and copy-on-write B-trees** — a copy-on-write B+tree with logical redo; checkpoints frozen per epoch. §M4; §M14.20 W4.
- **LeanStore key heads (Leis et al., ICDE 2018)** — four key bytes in each slot, searched as integers. §M14.33 issue #1.
- **Big-reader locks (Linux `brlock`, per-CPU rw-semaphores) and BRAVO (Dice & Kogan, USENIX ATC 2019)** — a reader lock a stripe, a writer taking every stripe: `striped::Striped`, the store's tree lock. Perf phase, point reads.
- **Optimistic, version-checked reads (LeanStore, Leis et al., ICDE 2018; Umbra's versioned latches)** — a page read without its latch, checked by a version: in safe Rust as a thread's copy of a tree's root, good while its frame's owner and version are unchanged (`BufferPool::read_copy`). Perf phase, point reads.
- **Bayer & Unterauer, prefix B-trees (1977); Graefe, *Modern B-Tree Techniques*** — a node's shared prefix stored once. §M14.20 V5.
- **Chen, Gibbons & Mowry, pB+-trees (SIGMOD 2001)** — a leaf's cache lines touched together. §M14.33 issue #1.
- **Group prefetching (Chen, Ailamaki, Gibbons & Mowry, ICDE 2004); AMAC (Kocberber, Falsafi & Grot, VLDB 2015); MySQL MRR** — `BTree::seek_group`. §M14.33 issue #1.
- **PostgreSQL `io_combine_limit`** — runs of pages read into one buffer. §M14.33.
- **PostgreSQL `nbtsort`** — sorted bulk loads packed into leaves. §M14.20 W2.
- **PostgreSQL's and SQLite's file extension ahead of writes** — §M14.20 W7.
- **PostgreSQL `heap_deform_tuple` and `attcacheoff`; SQLite serial types** — row decoding by fixed lengths and cached offsets. §M14.20 W7; §M14.33 issue #2.
- **SQLite overflow chains; PostgreSQL per-type `STORAGE`** — large values kept with their row. §M14.12 V7.2.
- **PostgreSQL `wal_level = minimal` / `COPY FREEZE`; CockroachDB `AddSSTable`** — bulk loads into fresh ranges skip the WAL. D16; §M14.20 W4.
- **PostgreSQL's checkpointer** — sorted buffer writes under a share lock. §M14.20 W4.
- **`fdatasync` (PostgreSQL's and SQLite's default); Apple `F_FULLFSYNC` / SQLite `fullfsync`** — the sync policy. D17; §M14.28.
- **ARIES redo by page; PostgreSQL recovery prefetch** — replayed writes applied sorted by key. §M14.28 issue #14.
- **O'Neil, Cheng, Gawlick & O'Neil, "The Log-Structured Merge-Tree" (1996); LevelDB and RocksDB** — the LSM engine: column families on one WAL, ingested runs, range deletions, rate limiting, the block cache, compaction filters, version edits. §M14.28.
- **Dostoevsky (Dayan & Idreos, SIGMOD 2018); Cassandra 5's Unified Compaction Strategy** — one parameterized merge policy, lazy leveling. §M14.28 L2.
- **Apache Parquet's split-block Bloom filter** — per-run filters. §M14.28 L2.
- **bLSM (Sears & Ramakrishnan, SIGMOD 2012); MyRocks (Matsunobu, Dong & Lee, VLDB 2020); WiredTiger's and CockroachDB's LSM histories; the RUM conjecture (Athanassoulis et al., EDBT 2016)** — merge scheduling, and the engine chosen per table. §M14.28.
- **ZFS births and dead lists** — copy-on-write branches in one file. §M14.13.
- **ZFS / btrfs `send`; CockroachDB incremental backups** — incremental backups by birth. §M14.18 O3.
- **Harris's economic order quantity (1913)** — index fold and seal sizes chosen by cost. §M14.12 V4; §M14.21.
- **CLOCK eviction** — the buffer pool and the LSM block cache. §M2; §M14.28.
- **TinyLFU's doorkeeper (Einziger, Friedman & Manes, 2017)** — a query kept in the plan cache the second time it is seen. §M14.20 W8.

## Transactions, time travel and testing

- **FoundationDB's read-conflict check** — serializable isolation by read-set validation. D12.
- **FoundationDB's and TigerBeetle's deterministic simulation** — I/O, time and randomness behind a seeded simulator. Principle 4.
- **PostgreSQL commit timestamps; CockroachDB `AS OF SYSTEM TIME`; Oracle and Snowflake per-table `AS OF`; SQL:2011 `FOR SYSTEM_TIME`** — time travel. §M14.14.
- **Jepsen's Elle (Kingsbury & Alvaro, VLDB 2020); Adya's anomalies; elle-cli** — the list-append checks in CI. §M14.29 S5.
- **SQLancer (Rigger & Su): differential testing, TLP, NoREC** — the bug hunt. §M14.33.
- **SQLite's sqllogictest format** — the test kit's scripts. §M9.
- **libFuzzer / cargo-fuzz; SQLite's `dbsqlfuzz`** — fuzzing in CI. §M14.29 S2.
- **SQLite `SQLITE_MAX_EXPR_DEPTH`; DuckDB `max_expression_depth`** — expression depth bounded. §M14.29 S2.
- **Benchmarking method: Raasveldt, Holanda, Gubner & Mühleisen, "Fair Benchmarking Considered Difficult" (DBTest 2018); Georges, Buytaert & Eeckhout (OOPSLA 2007); Mytkowicz et al. (ASPLOS 2009); Uta et al. (NSDI 2020); Kalibera & Jones (ISMM 2013); pyperf and hyperfine** — §M14.31; §M27.
- **YCSB's scrambled Zipfian** — LSM read workloads. §M14.28 L5.
- **PostgreSQL's subtransactions; InnoDB's undo records by savepoint** — savepoints as an undo note per savepoint, kept per key. Issue #24.
- **PostgreSQL's sequences (`SEQ_LOG_VALS`, values logged ahead and skipped after a crash); Oracle's sequence `CACHE`** — a sequence's value outside transactions, a reservation written ahead and grown while it is fast. Issue #23.

## Compression and encodings

- **LZ4's block format, its decoder's wild copy and compressor's match extension** — the `lz` codec. §M14.20 V5/W7; §M14.28 L4b.
- **Apache Arrow's layout; DuckDB's and ClickHouse's column writers** — columnar results and segments. §M14.11; §M14.20 W3b.
- **ClickHouse's sparse primary index (marks)** — point lookups in column tables. §M14.20 W6.
- **DuckDB's decimal widths** — decimals stored in 6 or 10 bytes. §M14.33 issue #2.
- **xxHash64 (Collet)** — the stable sketch hash. §M14.27 S6.
- **CRC-32C: Adler's `crc32c.c`; Intel's iSCSI CRC paper (2011)** — three interleaved streams. §M14.20 W5.
- **SplitMix64 (Steele, Lea & Flood, 2014); FNV-1a** — generators and ids.

## Text search (§M14.21)

- **Lucene and Tantivy** — 128-posting bit-packed blocks, tiered merges, delete bitsets, indexing threads. T3–T5b.
- **SQLite FTS5** — tombstones in newer segments, the trigram analyzer, `highlight()`. T1, T4, T6.
- **PostgreSQL's GIN pending list** — upkeep done by the writers (HNSW too). T4.
- **Lucene `BM25Similarity`; Kamphuis, de Vries, Boytsov & Lin (ECIR 2020)** — scoring with exact lengths. T5.
- **Turtle & Flood's MaxScore (1995) as Lucene 10's `MaxScoreBulkScorer`; Ding & Suel's block-max bounds (SIGIR 2011); PISA's and JASS's quantized sums** — windowed block-max MaxScore. T5b.
- **Lucene's query syntax and `SloppyPhraseMatcher`; Tantivy's split-word phrases** — T2, T6.
- **Lucene `ScoreDoc`, Elasticsearch `_score`, ParadeDB `pdb.score`** — the score as a column. T5c.
- **UAX #29 word breaking; Porter (1980); Lucene's `EnglishAnalyzer`; Snowball 3.0.1** — analyzers. T1.
- **Reciprocal rank fusion (Cormack, Clarke & Büttcher, SIGIR 2009); convex fusion (Bruch, Gai & Ingber, TOIS 2024)** — T7a.
- **Elasticsearch `sparse_vector`, OpenSearch neural sparse; SPLADE (Formal et al., 2021)** — the sparse index. T7b.
- **PostgreSQL `tsvector_op.c`, `tsrank.c`, `ts_headline`** — reproduced in pg-compat. T8.
- **Anserini and BEIR** — the reference baselines. T5.

## Vector search (§M14.12)

- **HNSW (Malkov & Yashunin, 2018); hnswlib's neighbour heuristic** — V4.
- **ParlayANN batch inserts (Manohar et al., 2024); NN-descent's local join (Dong et al., 2011)** — V4.
- **FreshDiskANN (Singh et al., 2021)** — a fixed graph beside a fresh delta; delete consolidation. V4.
- **Bentley & Saxe's logarithmic method (1980)** — patch levels merged. V4b.
- **pgvector** — the `vector` / `sparsevec` types, operators, limits and operator classes; iterative widening and scans. V1–V6b.
- **Lucene's exact fallback under a filter; Weaviate `flatSearchCutoff`; ACORN (Patel et al., 2024)** — filtered search. V5.
- **Milvus partition keys; Weaviate and Qdrant tenants** — one graph per tenant. V5.
- **Qdrant, Elasticsearch and Weaviate rescoring; Lucene's int8 corrective terms; FAISS and USearch codecs** — quantized graphs. V6a.

## JSON and documents

- **The Variant format (Spark, Delta, Iceberg, Parquet); MySQL's binary JSON** — node headers and per-container offset widths. §M14.15 J1.
- **RFC 8785 (JCS) numbers; RFC 8259 / I-JSON** — J1.
- **PostgreSQL's `jsonpath` grammar and executor** — SQL/JSON paths. J3a.
- **PostgreSQL GIN `jsonb_path_ops` and `jsonb_ops`** — the containment index. J4b.
- **pg_jsonschema's names; the `jsonschema` crate's compiled keywords** — schema checks. J5.
- **PostgreSQL `jsonb_extract_path`** — a `->` chain fused. §M15 D1.
- **PostgreSQL `INSERT … ON CONFLICT` (its documentation)** — the upsert's form and meaning: arbiters inferred from a key's parts, `excluded`, a row affected twice refused (`21000`), the update policy's `USING` an error. §M15 D2.
- **MongoDB (its documentation)** — a collection's shape: every document keyed by `_id`, one made where absent, any id but an array; a collection's JSON Schema validator (`WITH (schema = …)`). §M15 D3; dotted paths naming an index's fields (`CREATE INDEX ON c (a.b.c)`). §M15 D4. Its multikey index (an entry per array element, a field's array expanded) and MySQL's multi-valued index (its documentation): §M15 D5. Its query filter documents — the operators and their matching rules (arrays unwrapped, no comparison across types), which SQL/JSON's lax mode (SQL:2016; PostgreSQL's `jsonpath`) shares — chosen as nucleus's filter front end, compiled to lax path predicates: §M16 (D22). Its update operators (`$set` … `$push` with `$each` / `$position` / `$sort` / `$slice`, `$pull` by a condition), upserts, projections (`$slice`) and its sort order (the order of types, arrays by their least or greatest element, keys generated as a compound multikey index's, parallel arrays refused), as its documentation describes them and MongoDB 8 answers: §M16 Q3 (`json::document`).
- **MongoDB `_id` rules and multikey indexes; MySQL multi-valued indexes; Oracle 23ai JSON collections; DocumentDB / FerretDB** — the collection design. §M15.

## Wire protocol, drivers and compatibility

- **PostgreSQL's protocol v3 and catalogs** — an optional compatibility module only. §M12; D13/D14.
- **libpq 14's pipeline mode; Redis's reply gathering** — pipelining. §M14.20 W8.
- **Oracle prefetch / JDBC fetch size** — rows read ahead while the client fetches. §M14.20 W8.
- **PGlite** — the embedded engine behind its own protocol. §M14.23.
- **rustls / quinn-proto's sans-IO design** — `Exchange` for WASM. §M14.23.
- **SQLite's `opfs-sahpool`; wa-sqlite's access-handle pool** — the OPFS slot pool. §M14.23.
- **Realm, Couchbase Lite, DuckDB Swift** — a core with a C API under the mobile SDKs. §M14.23.
- **Standards** — RFC 5802 / 7677 SCRAM, RFC 5929, RFC 4616, RFC 7628, RFC 9562 UUIDs.

## Statistics and numerics (§M14.27)

- **Neal's superaccumulator (2015)** — the exact accumulator; Shewchuk's expansions as its test oracle. S1, S14.
- **PostgreSQL `numeric_stddev_internal`, `float8_regr_accum`, `nodeWindowAgg.c`, `orderedsetaggs.c`** — numeric results, semantics and errors. S2–S5.
- **DuckDB** — skewness and kurtosis, `median`, `SUMMARIZE`. S2, S5, S8b.
- **Leis, Kundhikanti, Kemper & Neumann, window functions (VLDB 2015); DuckDB `WindowSegmentTree`** — segment trees. S4.
- **Knuth's Algorithm D (as Hacker's Delight's `divmnu`)** — exact division. S3.
- **Alabi et al. (2012)** — selection for spilled quantiles. S5.
- **HyperLogLog++ (Heule, Nunkesser & Hall, EDBT 2013) with Ertl's estimator (2017); DDSketch (Masson, Rim & Lee, VLDB 2019); Space-Saving (Metwally, Agrawal & El Abbadi, 2005) and mergeable Misra–Gries (Agarwal et al., PODS 2012)** — sketches. S6.
- **PostgreSQL's `TABLESAMPLE`; Li's Algorithm L (1994); Tirthapura & Woodruff (DISC 2011); Lemire's bounded draw; repeatable hash splits** — sampling. S7.
- **SciPy, Numerical Recipes and scikit-learn's statistical tests; Siddiqi's PSI** — drift measures. S8a.

## Embeddings and ML kernels (§M14.22)

- **GotoBLAS / BLIS (Goto & van de Geijn, 2008)** — the matrix product. E3b.
- **Eigen's / XLA's rational `erf`; Cephes' `expf`** — E3b (their coefficients: `THIRD_PARTY.md`).
- **Hugging Face `tokenizers`; SentencePiece** — tokenizers. E3a.
- **text-embeddings-inference** — token-budget batches without padding. E3b.
- **Weinberger et al., feature hashing** — the `hashing` model. E2b.
- **LangChain's `CacheBackedEmbeddings`** — the embedding cache. E2b.

## Security and encryption

- **RFC 8439 ChaCha20-Poly1305, poly1305-donna's limbs; Bernstein's and Goll–Gueron's vectorized ChaCha20** — §M14.22 E2a; §M14.30 K2a.
- **AES-GCM-SIV (RFC 8452), RFC 5297 SIV, Rogaway–Shrimpton DAE, RFC 3394 key wrap; Gueron & Lindell; OpenSSL / BoringSSL POLYVAL; HKDF (RFC 5869)** — page and WAL sealing. §M14.30.
- **STREAM; Tink streaming AEAD; RocksDB `EncryptedEnv`'s place** — sealed LSM files. §M14.28 L4e.
- **Crypto-shredding (Verraes; Akka; Axon)** — per-subject keys destroyed by `FORGET`. §M14.24 R6.
- **Apple Advanced Data Protection; Signal's PIN-derived keys; pgsodium / Supabase Vault** — keys held by users and outside the data. §M14.30; §M14.22.
- **OWASP's PBKDF2 rounds** — §M14.30 K1a.

## AI memory (§M14.24)

- **Snowflake / BigQuery column tags** — subject columns. R2.
- **MongoDB's TTL cadence; SQLite `secure_delete`; Hyperscan / ripgrep's rarest byte** — expiry and erasure. R1–R2c.
- **pgAudit** — extended to what was returned. R5.
- **LongMemEval (Wu et al., ICLR 2025)** — the benchmark. R1.

## Operations and tooling

- **Prometheus / OpenMetrics, OpenTelemetry (OTLP), W3C `traceparent`; ClickHouse `ProfileEvents`; CockroachDB health endpoints** — metrics and tracing. §M14.18 O1–O2.
- **postgres_exporter / mysqld_exporter mixins** — alert rules. O2.
- **pgBackRest; restic / Borg; WAL-G / Barman; AWS SigV4** — backups. O3–O4.
- **CockroachDB online schema changes and F1 (Rae et al., VLDB 2013); gh-ost / pt-osc; PostgreSQL `CREATE INDEX CONCURRENTLY`, `SET ACCESS METHOD`; MySQL `ENGINE=`** — online type and engine changes. O5; §M14.28 L3b.
- **PostgreSQL `CREATE INDEX CONCURRENTLY` (as its documentation describes it); F1's write-only state (Rae et al., VLDB 2013)** — an index kept by writers before it is read, filled in parts, then made ready. Issue #18 (`online_index`).
- **PostgreSQL's binary-coercible type changes and `NOT VALID` / `VALIDATE CONSTRAINT` (as its documentation describes them); F1's states writers obey before the data is known to (Rae et al., VLDB 2013)** — a column type that keeps the values changed in the catalog alone, a narrower one checked online after writers obey it, and `NOT NULL` and checks the same way. Issue #18 steps 2 and 3 (`validating`).
- **PostgreSQL 11's fast `ADD COLUMN` (a non-volatile default kept as the attribute's missing value, as its documentation and release notes describe it)** — a column added without rewriting the rows, which read the kept value. Issue #18 step 4 (`Table::missing`).
- **PostgreSQL's `DROP COLUMN` (a dropped attribute left in the stored tuples, `attisdropped`, as its documentation describes it)** — a column dropped without rewriting the rows, which keep its place for readers to pass over. Issue #18 step 2b (`Table::gone`).
- **`pg_dump`'s dependency sort; Flyway checksums; dbmate; golang-migrate / sqlx names; Rails `schema_migrations`; sqlc** — migrations and code generation. §M14.16.
- **rustup's, Deno's and DuckDB's install scripts; shell.duckdb.org, the PGlite REPL, SQLite fiddle** — install and playground. §M14.17.

## Projects

PostgreSQL <https://www.postgresql.org> · DuckDB <https://duckdb.org> ·
ClickHouse <https://clickhouse.com> · SQLite <https://sqlite.org> ·
RocksDB <https://rocksdb.org> · LevelDB <https://github.com/google/leveldb> ·
LMDB <http://www.lmdb.tech> · CockroachDB <https://www.cockroachlabs.com> ·
FoundationDB <https://www.foundationdb.org> · TigerBeetle <https://tigerbeetle.com> ·
Velox <https://velox-lib.io> · Apache DataFusion <https://datafusion.apache.org> ·
Apache Calcite <https://calcite.apache.org> · Apache Spark <https://spark.apache.org> ·
Apache Arrow <https://arrow.apache.org> · Apache Parquet <https://parquet.apache.org> ·
Apache Lucene <https://lucene.apache.org> · Tantivy <https://github.com/quickwit-oss/tantivy> ·
ParadeDB <https://www.paradedb.com> · Snowball <https://snowballstem.org> ·
Unicode <https://www.unicode.org> · pgvector <https://github.com/pgvector/pgvector> ·
hnswlib <https://github.com/nmslib/hnswlib> · FAISS <https://github.com/facebookresearch/faiss> ·
Qdrant <https://qdrant.tech> · Weaviate <https://weaviate.io> · Milvus <https://milvus.io> ·
Elasticsearch <https://www.elastic.co/elasticsearch> · Soufflé <https://souffle-lang.github.io> ·
Jepsen / Elle <https://jepsen.io> · SQLancer <https://github.com/sqlancer/sqlancer> ·
xxHash <https://github.com/Cyan4973/xxHash> · LZ4 <https://lz4.org> ·
PGlite <https://pglite.dev> · pgBackRest <https://pgbackrest.org> · restic <https://restic.net> ·
Prometheus <https://prometheus.io> · OpenTelemetry <https://opentelemetry.io> ·
sqlc <https://sqlc.dev> · gh-ost <https://github.com/github/gh-ost> ·
Hugging Face tokenizers <https://github.com/huggingface/tokenizers> ·
SentencePiece <https://github.com/google/sentencepiece> · BLIS <https://github.com/flame/blis> ·
Eigen <https://eigen.tuxfamily.org> · Cephes <https://www.netlib.org/cephes/> ·
SciPy <https://scipy.org> · DDSketch <https://github.com/DataDog/sketches-java>
