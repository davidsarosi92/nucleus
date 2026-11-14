# nucleus — work plan

> **What this file is:** the forward-looking roadmap for nucleus.  Each
> milestone has a status, a short design sketch, and explicit **"done when"**
> criteria, so a fresh session (human or AI) can pick up the thread cleanly.
>
> **What `DOCS.md` will be:** the *current state* — what already exists, its
> API and its quirks.  Once a milestone ships, its design notes graduate into
> DOCS.md and the entry here shrinks to a one-line "✅ shipped → DOCS §x"
> pointer.

Status legend: ⬜ not started · ◐ in progress · ✅ shipped · ⏸ parked

---

## Contents

| Section | Purpose |
|---------|---------|
| Vision | What the system grows into |
| Architecture | The layers and their boundaries |
| North-star principles | The rules that keep later phases from needing a rewrite |
| Build targets | Platforms nucleus must build for |
| Decisions | What has to be settled before the first line of code |
| Status snapshot | Phases + milestones table |
| §F0 – §F4 | Phases and milestones in detail (the developer platform, §M14.9–§M14.26, sits between F1 and F2) |
| Changelog | Changes to this plan |

---

## Vision

nucleus is a **layered, multi-model database** whose core (the "nucleus") is
a **transactional, ordered key-value engine**.  Everything else is built on
top of it:

1. **SQL** — a relational model that maps rows and indexes onto key-value pairs.
2. **NoSQL** — document (and later other) models on the *same* KV core, with
   the same transaction machinery.
3. **Distributed** — the local KV engine is replaced by a partitioned,
   replicated (Raft) KV layer; the SQL/NoSQL layers above it **do not
   change**, only the implementation of the KV contract does.
4. **Above** — federation: the nucleus query engine can also reach external
   systems (other databases, files, other nucleus clusters).

This is the proven pattern of FoundationDB ("layers on top of an ordered
transactional KV") and CockroachDB/TiDB ("SQL on top of a distributed KV").
The reason to commit to it *now*: if the KV contract is chosen well, phase 3
is an implementation swap, not a rewrite.

**Deployment modes.**  nucleus is a general-purpose database, and every
phase must keep all of these working:

- **Embedded library** — linked into an application, like SQLite or RocksDB
  (from F0, Rust); from §M14.23 also inside Python and JavaScript
  processes, in the browser (WASM) and on phones, in memory or on a file;
  from §M16.7 kept in sync with a server.
- **Standalone SQL server** — start it, connect with `ncsql` or nucleus's
  drivers over its own protocol, `ncp` (from §M14.7, D14); with
  `--pg-compat` also `psql` and PostgreSQL drivers, as to a plain
  PostgreSQL (from §M12).
- **Standalone document server** — used like a NoSQL database, through its
  own client protocol (from M16).
- **Cluster** — the same SQL and document interfaces over the distributed
  KV (F3).

**First real consumer: [d-os](https://github.com/davidsarosi92/d-os).**  d-os is
a hobby operating system that will move its settings store onto nucleus
(§M6.5).  It is a consumer, not a special case: it uses the public API only,
and later it can use nucleus in any of the modes above (see §M6.5 "Beyond
settings").

## Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│  Interfaces: embedded API · ncsql · ncp · PostgreSQL compat      │  F1+
├──────────────────────────────────────────────────────────────────┤
│  Query: parser → binder → logical plan → optimizer →             │
│         physical plan → executor          (shared by SQL + doc)  │  F1, F2
├────────────────────────────┬─────────────────────────────────────┤
│  Relational model          │  Document model (+ others later)    │
│  catalog, row encoding,    │  collections, JSON encoding,        │
│  indexes                   │  path indexes                       │  F1, F2
├────────────────────────────┴─────────────────────────────────────┤
│  ════════ KV CONTRACT (the "nucleus contract") ════════          │
│  get / put / delete / range scan · transaction · snapshot · ts   │  F0
├──────────────────────────────────────────────────────────────────┤
│  Local implementation (F0)     │  Distributed implementation (F3) │
│  MVCC · WAL · B+tree · buffer  │  ranges · Raft · HLC ·           │
│  pool · pager                  │  distributed txn · placement     │
├──────────────────────────────────────────────────────────────────┤
│  Platform: file I/O · time · randomness · network — ALL injectable│  F0
└──────────────────────────────────────────────────────────────────┘
```

**Layering rule:** a layer only sees the interface of the layer directly
below it.  The SQL layer never touches the B+tree or the WAL.

## North-star principles

These rules are expensive now and very cheap later.  If a milestone would
violate one, amend it here first, with a rationale.

**Fidelity to these principles comes first.**  When a feature and a
principle conflict, the principle wins; work that brings existing code in
line with the principles is scheduled before new features (that is why
§M14.19 came first); and every milestone's design is checked against them
before it starts.

1. **Everything goes through the KV contract.**  Data models and the query
   layer only know the KV API.  This is what lets a distributed engine
   replace the local one in phase 3.
2. **Order-preserving (memcomparable) key encoding from day one.**  Bytewise
   key comparison = logical ordering.  Range scans, indexes, and later
   range-based partitioning all rely on it.
3. **Abstracted timestamps.**  The transaction timestamp is a logical `u64`
   counter for now, but it is wrapped in a type so an HLC (hybrid logical
   clock) can replace it later.
4. **Deterministic simulation testing.**  File I/O, time, randomness and
   (later) network are only reachable through injectable interfaces.  Tests
   run against a simulated platform that injects faults (torn writes, fsync
   failures, a crash at any point) and is reproducible from a seed.
   (FoundationDB / TigerBeetle style.)  This is what makes Raft and
   distributed transactions debuggable at all.
5. **Correctness before performance.**  Every durability-related milestone
   has crash tests and invariant checks in its "done when".
6. **Versioned formats.**  On-disk format, WAL records, key encoding and
   wire protocol all carry a version number; every page and every WAL record
   carries a checksum.
7. **Embedded first, server second.**  The core is a library (like SQLite);
   the server and the network protocol are a thin layer on top.
8. **Metadata is data.**  The catalog, schemas, and later the range
   descriptors live in the KV space under their own key prefix — there is no
   separate metadata store that would later need replicating.
9. **General-purpose core, consumer-agnostic.**  No consumer-specific code or
   assumptions in the core crates.  Integrations (such as d-os) live in
   separate crates under `integrations/` and depend only on the public API.
   If an integration needs something the public API cannot do, the API is
   extended for everyone, never bypassed.
10. **Portable and frugal.**  Every target in the table below builds from
    this repo (see "Build targets").  32-bit is included: no assumption of a
    64-bit `usize` or of mapping large files.  The real platform layer uses
    only plain file operations (on POSIX: `open`, `pread`, `pwrite`, `fsync`,
    `rename`, `ftruncate`; no `io_uring`, no `O_DIRECT`, `mmap` optional),
    with one implementation per OS family behind the platform interface.
    Memory use (buffer pool, caches) is bounded and configurable.
11. **One system, many services; compatible, not borrowed.**  SQL,
    documents, streams, vectors, live queries and whatever comes next are
    services of one nucleus: one protocol (`ncp`), one set of drivers, one
    shell, one catalog (`nc_catalog`), one login, one transaction across
    them all.  Other systems' protocols and catalogs (PostgreSQL, MongoDB)
    are supported as optional compatibility modules, so existing tools
    work — but nucleus's own interface comes first and is never shaped by
    them (D13, D14, D8).
12. **Modular: any part can be replaced or extended at any time** (as in
    d-os).  Every component sits behind an interface and is *registered*,
    not wired in: the platform (real, simulated), the KV implementation
    (local, later distributed), the client protocols (`ncp`, PostgreSQL and
    MongoDB compatibility), the data models and services (SQL, documents,
    streams, vectors), and inside them types, functions, table functions,
    catalog views, aggregates, authentication.  Adding one is a new
    implementation and its registration — never an edit of the core to
    wire it in; replacing one keeps every other part unchanged.  Interfaces
    are stable and versioned (principle 6) and documented for extension
    authors.  Where the code still has a hard-coded list, §M14.19 turns it
    into a registry.

## Build targets

nucleus must build for all of these.  Build artifacts are **not** stored in
this repo (where releases are published is decision D9); the repo only has
to be able to produce them.

| Platform | Rust targets | Notes |
|----------|--------------|-------|
| Linux | `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, and the same two as `-musl` (static) | the reference platform for development and benchmarks |
| macOS | `aarch64-apple-darwin`, `x86_64-apple-darwin` | `fsync` does not flush the drive cache on macOS — durability needs `fcntl(F_FULLFSYNC)` |
| Windows | `x86_64-pc-windows-msvc` | no POSIX file API — its own platform implementation (`FlushFileBuffers` for durability, `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` for atomic rename, offset reads/writes via `OVERLAPPED`) |
| d-os | `x86_64`, `aarch64`, `i686` `-unknown-linux-musl`, static | runs through the d-os Linux ABI; the only 32-bit target, so it is the one that catches `usize` assumptions |
| Browser (from §M14.23) | `wasm32-wasip1` | the embedded edition only: no threads (sessions stepped, `nucleus_server::Exchange`) — storage in a pool of OPFS files opened ahead (`nucleus_platform::slots`) or in memory; the standard library through WASI, which the driver provides in the page (fifteen functions: clocks, randomness, the console) |
| iOS, Android (from §M14.23) | `aarch64-apple-ios`, `aarch64-apple-ios-sim`, `aarch64-linux-android`, `x86_64-linux-android` | the embedded edition only; durability per platform (`F_FULLFSYNC` on iOS) |

Every target runs the same platform conformance tests, including the
durability ones (principle 5): "it compiles" is not "it is supported".
Where a target cannot run its tests in CI (d-os needs its QEMU harness), it
is at least cross-built in CI and tested in its own environment before a
release.

### Packaging and install

Every release is installable on every platform, not just buildable.  One
release = the same binaries (`nucleus` CLI, and from M12 `nucleus-server`)
packaged per platform:

| Platform | Package (first) | Later |
|----------|-----------------|-------|
| Linux | `.tar.gz` per target | `.deb` / `.rpm`, container image |
| macOS | `.tar.gz` per target | Homebrew formula |
| Windows | `.zip` | installer (MSI / winget) |
| d-os | `.tar.gz` in the **d-os app format** (below), one per d-os arch | installable through the d-os package frontend once it exists (d-os §M45) |

**The d-os app format** (what d-os `app install` expects, d-os §M93): the
archive has one top directory `nucleus-<version>/` (d-os reads the version
from that name), static musl binaries under `bin/`, and a `.dos-app` manifest
in the top directory declaring the programs (`program=`, `link=` into
`/bin`) and, for server mode, the service (`dir=` for the data directory,
`service=nucleus-server …`).  Installed this way, nucleus lands in
`/mnt/apps/nucleus/<version>/`; an update installs beside the old version
and `app use` rolls back.  The manifest and the packaging script live in
`integrations/d-os/` (principle 9); CI produces the three d-os archives
alongside the other platforms' packages.

Installing nucleus on d-os as a database (CLI, SQL or document server) is
independent of §M6.5: a user can install any release on any d-os machine.
§M6.5 is d-os itself using nucleus for its settings.

## Decisions

These have to be settled before §F0/M0.  Each has a proposal; settled ones
are marked ✅.

| # | Question | Proposal | Alternatives / notes |
|---|----------|----------|----------------------|
| D1 | Language | ✅ **Rust** (settled 2026-10-16) — memory safety + concurrency (buffer pool, MVCC, Raft) | rejected: C, Zig, Go |
| D2 | First storage engine | ✅ **B+tree** (copy-on-write, or in-place + WAL) | LSM tree: better for writes, but compaction adds complexity; can come later as a second engine behind the KV contract — now planned per table (§M14.28) |
| D3 | Concurrency model | ✅ **Threads + locks** in the engine, async only in the network layer | thread-per-core (shared-nothing) — revisit in phase 3 |
| D4 | SQL dialect | ✅ **PostgreSQL-like** subset (clients: D14) | familiar to most developers; with the optional PostgreSQL protocol it also gives existing clients (psql, drivers) |
| D5 | Isolation | ✅ **Snapshot Isolation** first, **Serializable (SSI)** later | — |
| D6 | File layout | ✅ **One data file + one WAL directory** per database | single file (SQLite-style) |
| D7 | License | ✅ **MIT** (came with the GitHub repo) | — |
| D8 | Document client protocol (needed by M16) | ✅ **`ncp` with document messages** (2026-10-17), through nucleus's drivers — one protocol and toolset for every service (principle 11); a MongoDB wire subset as an optional compatibility module (as `--pg-compat` is for SQL), so existing tools work too | the MongoDB wire protocol as the main interface (existing tools at once, but a borrowed identity); HTTP/JSON |
| D9 | Where release builds are published | **GitHub Releases** attached to tags, built by CI | a separate artifact store; not in this repo either way |
| D10 | Storage engine shape | ✅ **Copy-on-write B+tree + logical WAL** (2026-10-17): checkpoints are atomic pager commits of a COW tree; the WAL holds batches of key writes; recovery = replay after the checkpoint; no undo | ARIES-style physiological logging with in-place updates — more write-efficient, far more complex; revisit only if checkpoint write amplification hurts |
| D11 | Tree concurrency | ✅ **One writer or many readers** per tree (`RwLock`); MVCC scans re-seek between batches (2026-10-17) | latch crabbing / B-link trees, when benchmarks show writer contention |
| D12 | Serializable isolation (§M14) | ✅ **Read-set validation** (FoundationDB style): a commit fails if a key or range it read was written after its snapshot | Cahill SSI (fewer aborts, much more state); FDB's approach also distributes in F3 |
| D13 | System catalog naming (§M14.5) | ✅ **nucleus's own `nc_catalog`** (2026-10-17), in nucleus's terms (table ids, key prefixes, statistics); `pg_catalog` / `information_schema` only as a compatibility layer on top, because PostgreSQL clients hard-code those names — nucleus is PostgreSQL-compatible, not PostgreSQL | expose only `pg_catalog` (simpler, but makes PostgreSQL's names nucleus's identity) |
| D14 | Client protocol (§M14.7) | ✅ **nucleus's own protocol, `ncp`** (2026-10-17): little-endian frames, nucleus's typed value encoding on the wire, port 20035; nucleus's own drivers (Rust, C, C++, Python, JS/TS, Java).  The PostgreSQL protocol becomes an optional module (`--pg-compat`, off by default), frozen: kept working for psql and existing tools, not extended for tool-specific catalog queries | the PostgreSQL protocol as the only client protocol (every PostgreSQL tool works, but nucleus stays a PostgreSQL look-alike); dropping it entirely (no psql, no PostgreSQL tools) |
| D15 | Versioning | ✅ **Each format versioned on its own, one release version** (2026-11-13, before the first embedded release, §M14.23): the server, CLI, Rust crates and the embedded library share one semantic version (`Cargo.toml`; `nce_version()`); each **format** carries its own version, checked on open — the store's pager header (`FORMAT_VERSION`, oldest read `OLDEST_FORMAT_VERSION`; the WAL is the store's and opens under it), each catalog record kind (its `VERSION` byte), the row encoding (inside the store's version) and the client protocol (`ncp`'s Hello version, negotiated); a build reads every older format and refuses a newer one with a message naming both; drivers are versioned per language and name the protocol version they speak; a release note lists every format change.  Publishing (crates.io, PyPI, npm, Maven Central) follows the first tagged release | a single version for everything, formats included (a format change would force a release of every driver) |
| D16 | Bulk loads without the WAL (§M14.20 W4) | ✅ **Yes, switchable** (2026-11-02): writes into a range a transaction declared *fresh* (a new table's or index's keys, which no one else sees before it commits) skip the WAL, and its commit makes them durable by a checkpoint before the commit record; they are written as final versions, not intents a vacuum rewrites (PostgreSQL's `wal_level = minimal` and `COPY FREEZE`, CockroachDB's `AddSSTable`).  A setting turns it off; it is off by force when anything needs the whole WAL — replicas (§M17.5), point-in-time restore (§M14.18), §F3 | every write logged (the safe, slower default of a replicated system) |
| D17 | Sync policy (§M14.20 W1) | ✅ **`sync = ordered` by default, `full` as a setting** (2026-11-02): `ordered` asks the OS for ordered writes — on macOS `F_BARRIERFSYNC` — so the database is never corrupt and a crash of the process or the OS loses nothing, but a power loss may lose the last commits; `full` flushes the drive's cache at each commit (`F_FULLFSYNC`), so a commit survives power loss.  Where the OS has no cheaper ordering (Linux's `fdatasync`), both are the same.  PostgreSQL's and SQLite's defaults on macOS are the weaker of the two | `full` always (today's behaviour: every commit and checkpoint pays a drive-cache flush) |
| D18 | Format changes for speed (§M14.20 W7) | ✅ **Now** (2026-11-02), each with its version bumped and the older format still read (principle 6): column offsets in the row format, column chunks as extents, a batch's WAL record laid out as the tree applies it | later, behind a migration |

---

## Status snapshot

| Phase | Milestone | Goal | Status |
|-------|-----------|------|:------:|
| **F0 — Core** | M0 | Project skeleton, build, test harness, simulated platform | ✅ DOCS §3 |
| | M1 | Pager + on-disk format | ✅ DOCS §4.1 |
| | M2 | Buffer pool | ✅ DOCS §4.3 |
| | M3 | B+tree | ✅ DOCS §4.4 |
| | M4 | WAL + recovery | ✅ DOCS §4.5 |
| | M5 | Transactions + MVCC | ✅ DOCS §5 |
| | M6 | Stabilise the KV contract + embedded API + CLI | ✅ DOCS §6 (d-os run pending) |
| | M6.5 | First embedded consumer: d-os settings store | ◐ nucleus side done (DOCS §7); d-os side pending |
| **F1 — SQL** | M7 | Key encoding + catalog | ✅ DOCS §8.1 |
| | M8 | SQL parser + AST | ✅ DOCS §8.3 |
| | M9 | Binder, planner, Volcano executor | ✅ DOCS §8.4 |
| | M10 | Joins, aggregation, secondary indexes | ✅ DOCS §8.5 |
| | M11 | SQL transactions + constraints | ✅ DOCS §8.7 |
| | M12 | Server + Postgres wire protocol | ✅ DOCS §9 |
| | M13 | Cost-based optimizer | ✅ DOCS §8.8 |
| | M14 | Serializable (read-set validation) | ✅ DOCS §5.2 |
| | M14.5 | SQL completeness: FK, FULL JOIN, EXCEPT/INTERSECT, CTEs, numeric and date/time types, `nc_catalog`, `psql` `\d` | ✅ DOCS §8.10–§8.11 |
| | M14.6 | `ncsql`: nucleus's own SQL shell and client | ✅ DOCS §6.2 |
| | M14.7 | The nucleus protocol (`ncp`); PostgreSQL protocol optional | ✅ DOCS §9–§10 |
| | M14.8 | Drivers: Rust, C, C++, Python, JS/TS, Java; examples | ✅ DOCS §11 |
| | M14.19 | Modularity — registries, extensible and evolvable types (`UUID` the first new one), an extension API (principle 12) | ✅ DOCS §1, §8.11–§8.12, §9 |
| | M14.26 | Core types: integers by width, `real`, `bytea` (with large values out of line), `time`, `char(n)`, `inet` / `cidr`, enums; `ncp` version 3 in every driver | ✅ |
| | M14.25 | Identity and access: users and roles, privileges on every object, authentication modules (SCRAM, tokens, certificates, OIDC, LDAP), access rules by connection context, row-level security, protection, audit | ✅ |
| | M14.9 | SQL analyzer: `EXPLAIN ANALYZE`, statement statistics, linter, index advisor | ✅ DOCS §8.13 |
| | M14.10 | AI access: MCP server, schema comments, hints in errors | ✅ DOCS §6.5, §8.14, `docs/llm.md` |
| | M14.11 | Large data foundations: streaming and columnar results (zero-copy into numpy / Arrow), bulk load, large transactions, spilling, the B+tree's read and write paths, benchmarks against PostgreSQL | ✅ DOCS §5.2, §8.5, §8.7, §10, `docs/benchmarks.md` |
| | M14.20 | Analytics: vectorized execution, SIMD kernels as a type capability, table partitioning, row / column / both storage chosen per table, column and partition (HTAP) | ✅ V1–V14; every benchmark target met (2026-11-06: TPC-H 0.25× column / 0.77× row, ClickBench 0.11× / 0.89×, loads 0.55–1.1×; `docs/benchmarks.md`, Targets) |
| | M14.27 | Statistics and numerical analysis on large data, for AI models and AI developers: a study first (moments, deviations, correlation, quantiles, sketches, drift), then the plan | ✅ study written (2026-11-05): exact power sums by one superaccumulator, window functions, exact quantiles by selection, order-free sketches; steps S1–S8 agreed (2026-11-06); S1 ✅ (one exact accumulator), S2 ✅ (moments), S3 ✅ (two variables), S4 ✅ (window functions, then the sort: `ORDER BY` 2–4× faster, a primary key's order left unsorted), S5 ✅ (ordered sets: exact quantiles by selection, 3–4× PostgreSQL's), S6 ✅ (sketches: HyperLogLog++, DDSketch, top-k, the same bytes every run), S7 ✅ (sampling: `TABLESAMPLE … REPEATABLE` by the key's hash, `hash_unit`, `reservoir_sample`, `random()`), S8a ✅ (drift and tests: `ks_test`, `auc`, `psi`, `js_divergence`, `chisq_test`, scipy's answers), S8b ✅ (`SUMMARIZE`, the MCP tool) |
| | M14.28 | B+tree or LSM tree, chosen per table (`WITH (engine = …)`) beside the row / column layout: a study first, then the plan | ◐ study written; L0 ✅; L1–L5 next |
| | M14.12 | Vectors: `VECTOR(n)` with every capability, similarity search, an ANN index, filtered vector search | ✅ V1, V2 (exact search 48 ms / 100k × 768, numpy 57), V2b (components by position, numpy / Arrow, pgvector's clients), V3 (estimates on rows `ANALYZE` keeps), V4 (the HNSW index's storage measured: (C)), V4b (the index in the engine: `USING hnsw`, `ORDER BY … LIMIT` through it; SIFT-1M through SQL at pgvector's recall half its time a search, 2× its inserts, a search while they come in 0.9 ms against 1.6), V5 (filtered search: `ef_search` counts passing rows, a B-tree range searched exactly, row-level security as a filter, `PARTITION BY` a graph per tenant — recall 0.93–1.0 from 50 % to 0.01 %, a 1 % filter 1.4 ms against pgvector's 14.5, a tenth's tenant 0.46 ms against 2.4), V6a (quantized graphs: `half`, `int8`, `binary` codes, the rows rescoring — `int8` a quarter of the memory, SIFT-1M built in 29 s against 55 and searched in 0.29 ms against 0.36 at the same recall), V6b (`sparsevec`: pgvector's type, distances, exact search — 100,000 SPLADE-like rows in 20 ms on the workers, pgvector 17.9 on three), V7 (the query vector from a subquery or a row and `LATERAL` — "more like this" 1.1 ms against pgvector's 1.5, 1,000 rows' neighbours 154 ms against 1,442; vectors read with their row; `sparsevec` in the six drivers and on pg-compat); *Done when* checked whole |
| | M14.21 | Full-text and hybrid search: BM25, analyzers as modules, text + vector + filters in one query — T1 analyzers (UAX #29 words, Porter, Snowball × 13, trigrams), T2 `@@` queries on rows, T3–T4 the exact text index, T5 BM25 (`score`, ranked by the index; Anserini's nDCG@10 on BEIR), T5a one ranking for every method (HNSW and text, filters, row-level security, text matches ranked by HNSW), T5b's found rows read by a merge with the table's scan, block-max MaxScore over impacts and the build on every core (MS MARCO: 2.0 ms a query, ParadeDB 5.5; built in 42 s, 231 before), T5c `score()` reading the ranking's scores (BEIR 2.5–6.8× faster, every corpus faster than ParadeDB; MS MARCO 1.8 ms), T6 phrases within a slop, `highlight` and `snippet` (with snippets 1.0–2.3× ParadeDB's speed), T7a `rrf` and `convex` in one plan (BEIR: fused above either ranking, 1.8–5.0× ParadeDB's RRF), T7b a sparse vector index, `<#>` exact (SPLADE on BEIR: nDCG@10 at or above pgvector's approximate HNSW), T8a PostgreSQL's `tsvector` / `tsquery` in pg-compat, the GIN pattern rewritten to the text index's `@@` (BEIR: 1.1–1.3× GIN's speed for all words, for any medians 1.2–3.9×), T8b `ts_rank`, `ts_rank_cd` and `ts_headline` as PostgreSQL computes them (5,100 of its answers alike; a search page 1.3–1.4× GIN's speed), the output computed after `LIMIT`, T8c the indexes on `column` and partitioned tables (a partitioned table's BM25 its partitions' together, the same nDCG@10), `tsvector` / `tsquery` as PostgreSQL's types on its wire, binary both ways (305 of its forms alike) done | ✅ |
| | M14.22 | Embeddings in the database: `embed()`, embedding columns kept current in the background, chunking | ✅ E1 (the study and steps E2–E8, 2026-11-12; local models on nucleus's own kernels), E2a ✅ (secrets: sealed by ChaCha20-Poly1305 under a key outside the data directory), E2b ✅ (`CREATE MODEL`, `embed()`, providers: `hashing` built in, OpenAI-compatible, Cohere, Voyage as a module), E3a ✅ (tokenizers: WordPiece and Unigram from `tokenizer.json`, the same ids as Hugging Face on 2.5 M texts), E3b ✅ (the encoder on nucleus's kernels: cosine ≥ 0.9999998 against ONNX Runtime, the same bits on any threads), E3c ✅ (`CREATE MODEL … USING local`, files kept in the database; 3.2× fastembed's speed on one core, 4.3× on ten), E4a ✅ (`GENERATED ALWAYS AS … STORED`), E4b ✅ (`GENERATED BY EMBEDDING`: the queue, rounds, retries, the circuit, pending rows), E5 ✅ (`ALTER COLUMN … SET MODEL`: the next model's vectors made beside the column, taken in one transaction), E6 ✅ (`chunks()`: UAX #29 sentences, word windows, Markdown sections; sizes in a model's tokens), E7 ✅ (`CREATE TABLE … CHUNKS OF`: chunks kept current in the writer's transaction, unchanged chunks keep their vectors), E8a ✅ (batches by tokens, rate limits by token bucket, parallel calls, each statement's model costs in `EXPLAIN ANALYZE` and its statistics), E8b ✅ (the *Done when* on BEIR: documents to ranked chunks by a local model in a server killed twice; nDCG@10 that of fastembed's vectors, the chunks and vectors the same bit for bit after edits put back) done | ✅ |
| | M14.23 | Embedded editions: in memory or on a file inside Python and JavaScript, the browser (WASM), iOS and Android | ✅ step 1 Python and JavaScript, step 2 the browser (WASM, OPFS), step 3 iOS and Android (2026-11-13); sync with a server is §M16.7 |
| | M14.24 | Memory for AI models: knowledge that is updatable, erasable, permissioned and auditable, read at generation time | ✅ (2026-11-14) study written (2026-11-13); R1 ✅ memory stores, `REMEMBER`, `recall()`, row expiry, MCP tools, driver helpers (LongMemEval_S turn R@10 0.819 against the paper's BM25 0.592); R2 ✅ `FORGET SUBJECT`, the scrub and the search (a planted subject's texts: 100 places after `DELETE` and a vacuum, 0 after `FORGET`); R3 ✅ leak tests over every recall path, BM25's statistics a tenant's own on a table partitioned by tenant; R4 ✅ `recall` under 1 ms in the process (100 000 memories), batched through `LATERAL`, a vacuum's crash flaw fixed; §M14.14 ✅ between; R5 ✅ citations (`as_of`), read records by request id (`nc_catalog.reads`), erasure through history and the log; R6 ✅ `BACKUP TO` / `nucleus restore` without index pages, each subject's rows sealed under its own key, which `FORGET` destroys (60 MiB backed up in 0.46 s, restored in 0.80 s); R7 ✅ `docs/memory.md`, the Python and JavaScript `memory` examples (run in CI), MCP `cite` and `forget_subject` |
| | M14.13 | Database branches (copy-on-write) | ⬜ |
| | M14.14 | Time travel: `AS OF` queries | ✅ 2026-11-13: commit times recorded (deferred puts: no cost measurable), `BEGIN … AS OF TIMESTAMP` or `COMMIT`, `SET read_as_of`, `FROM t AS OF …` / `FOR SYSTEM_TIME AS OF`, `CREATE TABLE … AS`, `nucleus server --history`, `nc_catalog.history`; checked against a logged workload through every index kind |
| | M14.15 | JSON type and functions | ◐ study written (2026-11-14): one binary `json` type with canonical bytes, PostgreSQL's operators and the SQL/JSON standard's, a containment index on the text index's postings, JSON Schema checks for models' outputs; J1 ✅ the type (parse faster than `serde_json` on two of three files and level on numbers, 2.8–11× PostgreSQL's `jsonb` and output 1.5–9×, stored 13–45% smaller); J2 ✅ PostgreSQL's operators and functions, 108 records checked against PostgreSQL 18 (`scripts/slt-pg.py`), a field read 3–4.4× faster; J3a ✅ SQL/JSON paths (`jsonpath`, `json_path_*`, `@?`, `@@`), 221 records checked against PostgreSQL 18, a filtered path 2.4–5.7× faster; J3b ✅ `JSON_VALUE` / `JSON_QUERY` / `JSON_EXISTS`, `IS JSON`, the constructors, 207 records checked against PostgreSQL 18, `JSON_VALUE` 3.3–4.7× faster; J3c ✅ `JSON_TABLE` with `NESTED PATH`, 67 records checked against PostgreSQL 18; J4a ✅ expression indexes for every type; J4b ✅ the containment index `USING json` (`@>`, `?`, `?|`, `?&`), 14–275× faster than a full scan on 200,000 events; J4c (against PostgreSQL's GIN) waits for Docker |
| | M14.16 | Migrations and code generation | ⬜ |
| | M14.17 | Easy start: packages, Docker, web console, browser playground | ⬜ |
| | M14.18 | Operations: full and incremental backups to local or object storage, point-in-time restore, automatic restore tests, metrics and tracing, health checks, alert rules | ⬜ |
| | M14.29 | Security and assurance from outside, each step at its time: `SECURITY.md` (now), fuzzing in CI (before §M14.23), OSS-Fuzz, an independent security audit through OSTIF (before the first production release), Elle's checks on one node | ◐ S1 ✅ `SECURITY.md` (2026-11-12), S2 ✅ fuzzing in CI (2026-11-13: ten targets, nine bugs fixed) |
| **F2 — NoSQL** | M15 | Document model | ⬜ |
| | M16 | Document queries (shared executor) + document server | ⬜ |
| | M16.5 | Streams: a transactional broker, changefeeds (CDC) | ⬜ |
| | M16.6 | Live queries: subscriptions to query results | ⬜ |
| | M16.7 | Sync: embedded databases on devices kept in sync with a server, offline first | ⬜ |
| | M17 | Further models (decide later) | ⏸ |
| | M17.5 | Read replicas, failover, cluster management | ⬜ |
| **F3 — Distributed** | M18 | Range partitioning (still single-node) | ⬜ |
| | M19 | Per-range Raft replication | ⬜ |
| | M20 | Cluster membership + placement | ⬜ |
| | M21 | Distributed transactions (HLC) | ⬜ |
| | M22 | Distributed query execution | ⬜ |
| | M23 | Rebalancing, fault tolerance, Jepsen-style tests | ⬜ |
| | M23.5 | Jepsen's own analysis of the distributed guarantees, published | ⬜ |
| **F4 — Above** | M24 | Federation / connectors | ⬜ |
| | M25 | Multi-region (changefeeds moved to M16.5) | ⏸ |

---

## §F0 — The core (transactional ordered KV)

The goal is an embeddable library that stores ordered key-value pairs
durably, crash-safely and transactionally.  This is the most important part
of the system — every later phase uses it.

### §M0 — Project skeleton ✅ (DOCS §1–§3)

- Cargo workspace, one crate per layer; `rustfmt` + `clippy`, GitHub Actions CI.
- CI builds every target in "Build targets" (Linux, macOS and Windows
  runners; the musl targets cross-built), and runs the tests on Linux, macOS
  and Windows, so a portability regression fails the build the day it is
  introduced.
- Platform implementations: POSIX (Linux, macOS, d-os) and Windows, plus the
  simulated one.
- Module boundaries following the architecture diagram (`platform`,
  `storage`, `txn`, `kv`, later `sql`, `doc`, `server`).
- **Platform interface** (file, time, randomness) with two implementations:
  real OS and simulated (in-memory, fault-injecting, deterministic from a seed).
- Property-based testing infrastructure.

**Done when:** `cargo build` + `cargo test` are green in CI; a sample test
runs on the simulated platform and is reproducible from a seed; a DOCS.md
skeleton exists.

### §M1 — Pager + on-disk format ✅ (DOCS §4.1–§4.2)

- 8 KiB pages (settled 2026-10-17), page `n` at offset `n × 8192`; every
  page starts with a 24-byte header: CRC-32C, type, LSN, its own id
  (catches misdirected writes).
- Two header copies (pages 0 and 1) written alternately with a sequence
  number: an atomic commit point.  16 meta slots for the layers above.
- **Revised with §M3/§M4 (D10):** the durable state is immutable — only
  pages allocated since the last commit may be written; freed durable pages
  become reusable after the next commit; the free list lives in dedicated
  pages rewritten by each commit.  (The first version linked free pages
  through themselves and kept a 32-page reserve; copy-on-write made that
  unnecessary, and now nothing leaks.)
- `create` is atomic (temporary name + rename); a failed `sync` poisons.

**Done when:** pages round-trip; bad checksums, torn and misdirected writes
are detected; a crash at every I/O point of a workload, both crash modes,
reopens to the last (or in-flight) commit with every live page intact and
every page accounted for; mutations of the ordering rules fail that test.

### §M2 — Buffer pool ✅ (DOCS §4.3)

- Fixed frames, pin/unpin through `PageRef`, per-frame reader/writer
  latches, dirty tracking, clock eviction with write-back.
- No "no-steal" rule needed: a dirty page can be evicted any time, because
  the pager only lets pages outside the committed state be written.
  `PageRef::write` checks that rule up front.

**Done when:** a concurrent stress test (8 threads, real and simulated
file systems, thousands of evictions) loses nothing; pinned pages are never
evicted; a fully pinned pool says so.

### §M3 — B+tree ✅ (DOCS §4.4)

- Variable-length keys (≤ 1024 bytes) and values, overflow chains for large
  values; insert, delete, get, range iterators both ways.
- Split by bytes; merge or redistribute under a quarter page.
- **Copy-on-write (D10):** a node changed after a commit is copied, up to a
  new root, so committing the pager is an atomic checkpoint.
- **Concurrency (D11):** one writer or many readers (an `RwLock` around the
  tree) instead of latch crabbing; MVCC scans re-seek between batches, so
  no lock is held for long.  Finer latching waits for benchmarks.

**Done when:** a property test agrees with a `BTreeMap` over a million
random operations (CI, long tests) and over model-based workloads with
commits between epochs; the invariant checker (ordering, separators, equal
leaf depth, minimum fill, overflow lengths, page accounting) passes after
every epoch; the committed tree survives a crash at every I/O point.

### §M4 — WAL + recovery ✅ (DOCS §4.5–§4.6)

- WAL segments with checksummed, LSN-stamped records; group commit; torn
  tail cut on open.  Directory changes ordered so a crash only loses a
  suffix or a prefix of the log, never leaves a gap.
- **Logical redo + copy-on-write checkpoints (D10)** instead of ARIES: a
  WAL record is an atomic batch of key writes; a checkpoint commits the
  tree root and the WAL position with the pages; recovery replays the
  batches after the checkpoint.  No undo: uncommitted data never reaches
  the tree.

**Done when:** a crash at every I/O point of a workload with unsynced
batches piling up across segments and checkpoints recovers exactly the
state after some batch `k`, including every acknowledged one, with tree and
pager checks passing; ordering mutations fail it.

### §M5 — Transactions + MVCC ✅ (DOCS §5)

- Versions as store entries keyed `escape(key) 00 00 (MAX − ts)`; snapshot
  reads are one seek; tombstones for deletes.
- Snapshot isolation with first-committer-wins; writes buffered, applied at
  commit as one WAL batch carrying the commit timestamp; snapshots see only
  durable commits.
- GC at commit for the keys written, `vacuum` for the rest; watermark =
  oldest running snapshot minus an optional history window.

**Done when:** dirty reads, lost updates, non-repeatable reads and phantoms
do not occur under SI; write skew does and is documented (§M14); GC keeps
every version a running snapshot can see; concurrent transfers keep their
total; recovery and a crash at every I/O point keep every acknowledged
commit.

### §M6 — Stabilise the KV contract ✅ (DOCS §6; running on d-os pending)

- Finalise the KV API — **this is the interface the F3 distributed
  implementation will also implement**, so it gets thought through here:
  range operations, batches, snapshot reads at a given timestamp, error
  types (e.g. retryable conflict vs. permanent failure).
- Public embedded library API + documentation.
- Small CLI/REPL for KV commands (`put`, `get`, `scan`, `begin`, `commit`).
- First benchmark baseline.
- First release packages for every platform in "Packaging and install",
  including the d-os app format.

**Done when:** the KV API sits behind a trait that F1 code uses without
knowing any concrete type; DOCS.md describes it; the release archive installs
and the CLI runs on Linux, macOS, Windows and d-os (`app install
nucleus-<version>-d-os-<arch>.tar.gz`, then `nucleus` from the d-os shell).

### §M6.5 — First embedded consumer: d-os settings store ◐ (nucleus side done, DOCS §7; d-os side pending)

[d-os](https://github.com/davidsarosi92/d-os) keeps its settings in a key-value
store inside the kernel (`kernel/core/config.c`): built-in defaults, an
in-memory cache, change watchers, one layer per signed-in account.  Today it
persists them by rewriting a flat text file, `<volume>/d-os.conf`.  This
milestone makes nucleus that persistence backend.  It is the first time
nucleus runs under a real workload — before SQL exists.

**What stays in d-os.**  The kernel cache, the defaults and the watchers.
`config_init` runs before any disk or userland exists, and `config_get` is
called from early boot and from fault paths that must never block, so the
kernel keeps answering from memory.  Only loading and saving move to nucleus.

**Design.**

- A ring-3 service (a static musl Rust binary, run by the d-os service
  supervisor, shipped in the d-os app format of "Packaging and install")
  built on the nucleus **embedded API only** (principle 9).  Whether d-os
  bundles it in its system image or installs it as an app is d-os's call —
  settings are needed early in boot, which may favour bundling.  Its
  crate lives in this repo at `integrations/d-os/`, so nucleus CI builds it
  for all three targets; the kernel-side half stays in the d-os repo.
- Key layout: `cfg/machine/<key>` and `cfg/user/<uid>/<key>` — the d-os
  layers become key prefixes.
- Startup: once the service is up, the kernel overlays the stored values on
  its cache (what `config_attach_persistent` does with the file today).
  `config_save` writes through to the service; one save = one transaction,
  so a settings panel that changes several keys commits all or none.
- No service → the kernel keeps working on defaults and says honestly that
  nothing will survive a reboot (the same rule d-os already follows).
- History: old versions stay readable through MVCC snapshots (bounded
  retention), so "restore yesterday's settings" is possible.
- Migration and rescue: on first start an existing `d-os.conf` is imported;
  `export` writes the same text format back.  The text file stays readable
  from another OS, which a binary store alone would lose.

**Prerequisites.**

- nucleus: M6 shipped; on-disk format versioned and covered by a migration
  path (no breaking change without one).
- d-os: Rust `std` binaries run on musl (d-os §M44 — expected to be low risk,
  since unmodified Java and Docker already run); `fsync` on exFAT over
  virtio-blk really reaches stable storage — nucleus durability depends on
  it, so this is verified, not assumed.

**Done when:**

- On d-os (i386, x86_64, aarch64) settings saved through nucleus survive a
  reboot, including per-account layers.
- Crash test on d-os: the VM is killed during a save, many times — after
  restart the store holds either the complete old or the complete new set,
  never a mix.
- An existing `d-os.conf` imports on first start, and `export` reproduces it.
- Killing the service leaves d-os running on its cache and reporting that
  saves do not persist.

**Status (2026-10-17).**  The nucleus side is done: `nucleus-settings`
(crate `integrations/d-os`) with its line protocol, layers, atomic saves,
history (`DUMP … AT <ts>`), `IMPORT`/`EXPORT` of `d-os.conf`, packaged in
the d-os archive; a crash at every I/O point of a multi-key save keeps the
old or the new set whole.  The "done when" items that run on d-os itself
(kernel side, the three arches, VM-kill tests) are the d-os side's.

**The d-os switch-over.**  d-os moves its settings to nucleus once the "done
when" list above is green; the d-os PLAN tracks the switch on its side.
Until then `d-os.conf` remains the d-os default.

**Beyond settings.**  The same deployment modes as anywhere else: once M12
ships, programs on d-os can use nucleus as a Postgres-compatible system
database; once M16 ships, as a document store.  No d-os-specific server code
— d-os gets the same binaries as any other platform.

---

## §F1 — SQL layer

### §M7 — Key encoding + catalog ✅ (DOCS §8.1–§8.2)

- Order-preserving key encoding (NULL first; bool, bigint, double, text),
  self-delimiting for composite keys; versioned row encoding.
- Keyspace: catalog `00 01 <name>`, sequences, hidden row ids, statistics;
  data `01 <table> <index> <key>`.  The catalog lives in the KV store, so
  DDL is transactional.
- Types for now: `BOOLEAN`, `BIGINT` (all integer names), `DOUBLE
  PRECISION` (all float names), `TEXT` / `VARCHAR(n)`.  (Decimal,
  date/time and array types came in §M14.5.)

**Done when:** encoding round-trip and order preservation are
property-tested. ✅

### §M8 — SQL parser + AST ✅ (DOCS §8.3)

Hand-written recursive descent with Pratt precedence; DDL, DML, queries
with joins/grouping/ordering/subqueries/UNION, transactions, SET/SHOW,
EXPLAIN, ANALYZE; errors with SQLSTATE 42601 and a position.

### §M9 — Binder, planner, executor ✅ (DOCS §8.4–§8.6)

Binder (nested scopes, correlated subqueries, type inference with untyped
literals and parameters), rule-based optimizer (pushdown, hash joins,
access paths), Volcano executor, `EXPLAIN`.

**Done when:** a sqllogictest suite is green. ✅ (`crates/sql/tests/slt`,
eleven scripts.)

### §M10 — Joins, aggregation, indexes ✅ (DOCS §8.5–§8.6)

Nested-loop and hash joins (inner, left; right as swapped left; USING),
hash aggregation with DISTINCT aggregates, HAVING, ORDER BY with NULLS
FIRST/LAST, DISTINCT, UNION; secondary indexes with backfill and
maintenance; index range scans.  Merge join is not needed yet.

### §M11 — SQL transactions + constraints ✅ (DOCS §8.7)

`BEGIN`/`COMMIT`/`ROLLBACK`, autocommit with conflict retry, aborted
blocks (`25P02`); `PRIMARY KEY`, `UNIQUE` (concurrent duplicates conflict),
`NOT NULL`, `CHECK`, `DEFAULT`, `VARCHAR(n)`, `RETURNING`.  `FOREIGN KEY`
was parsed and refused (`0A000`) here; it shipped in §M14.5.

### §M12 — Server + Postgres wire protocol ✅ (DOCS §9; optional since §M14.7)

Protocol v3: startup, trust or cleartext password, simple and extended
query protocols, text and binary formats; `nucleus server`.

**Done when:** `psql` connects and the M9–M11 suite runs through it. ✅ The
whole slt suite runs over the wire; a real driver (the `postgres` crate)
exercises the extended protocol; CI runs `psql` against the server.

### §M13 — Cost-based optimizer ✅ (DOCS §8.8)

`ANALYZE` (rows, NULLs, distinct values, equi-depth histograms);
selectivity estimates; cost-based access paths; join reordering by dynamic
programming over up to 8 inner-join inputs.  Without statistics the
rule-based plan is kept.

### §M14 — Serializable ✅ (DOCS §5.2, D12)

Read-set validation (FoundationDB style) instead of Cahill SSI: keys and
consumed scan ranges are checked at commit against recent commits.

**Done when:** the write-skew test that "failed as documented" in M5 is
green under SERIALIZABLE. ✅ (And phantoms, in KV and SQL tests.)

### §M14.5 — SQL completeness ✅ (DOCS §8.1, §8.3, §8.10–§8.11)

The gaps that keep nucleus from passing for "a plain PostgreSQL" in
everyday use, closed before F2:

- `EXCEPT` / `INTERSECT` (`ALL` too), with PostgreSQL's precedence.
- Common table expressions: `WITH` (several, referring to earlier ones)
  and `WITH RECURSIVE` (`UNION [ALL]`, iterated to a fixed point).
- `FULL [OUTER] JOIN` (with `USING` merging columns by `COALESCE`).
- Types: `NUMERIC` / `DECIMAL` (exact, with `(p, s)`), `DATE`,
  `TIMESTAMP`, `TIMESTAMPTZ` (sessions in UTC), `INTERVAL`; decimal
  literals are `NUMERIC` as in PostgreSQL; date/time arithmetic and
  functions (`now()`, `extract`, `date_trunc`, …).
- `FOREIGN KEY` / `REFERENCES`: checked at the end of each statement;
  `ON DELETE` / `ON UPDATE` `NO ACTION`, `RESTRICT`, `CASCADE`, `SET NULL`,
  `SET DEFAULT`; race-free under snapshot isolation (the checks' reads are
  validated at commit, as serializable reads are).
- nucleus's own system catalog, `nc_catalog` (D13), and on top of it the
  `pg_catalog` / `information_schema` compatibility layer — tables,
  functions and syntax (arrays, regular expressions, functions in `FROM`,
  schema-qualified names, `::regclass`) — that `psql`'s `\d` family
  queries, so `\d`, `\d table`, `\d+`, `\dt`, `\di`, `\dn`, `\l` work.

**Done when:** each has an slt script whose expected output was checked
against PostgreSQL 16, and CI's `psql` runs `\d` against the server. ✅
(Scripts 12–19; `\df` and permissions remain out of scope.)

### §M14.6 — `ncsql`, the nucleus SQL shell ✅ (DOCS §6.2)

nucleus's own client, so working with it does not mean borrowing
PostgreSQL's (`psql` still works, D13): `ncsql` (= `nucleus sql`) opens a
local database or connects to a server, and its backslash commands read
`nc_catalog` — including what `psql` cannot show (table ids, key prefixes,
statistics).  The protocol side is a reusable crate, `nucleus-client`.  The
server gains `--log-queries`, to see what other tools send.

**Done when:** `ncsql` runs statements and its commands locally and
against `nucleus server`, tested end to end, and ships in every package. ✅

### §M14.7 — The nucleus protocol (`ncp`) ✅ (DOCS §9–§10)

nucleus's own client protocol (D14), specified in DOCS §10 so drivers can
be written from the document alone: frames, handshake and password, typed
or text results, typed parameters, nucleus's value encoding (numeric,
date/time, interval and array values travel exactly as stored), SQLSTATE
errors, the transaction status.  `nucleus server` speaks it by default
(port 20035); the PostgreSQL protocol starts only with `--pg-compat`.
`nucleus-client` (Rust) and `ncsql` move to it.

Servers and clients default to user `nucleus`, password `nucleus`, so they
work together unconfigured.

**Done when:** the slt suite runs over `ncp`, `ncsql` uses it, and psql
still works with `--pg-compat`. ✅

### §M14.8 — Drivers ✅ (DOCS §11)

Drivers in `drivers/`, each a small library with its own tests against a
running server (CI starts one): **C** (`libnucleus`, C99, POSIX and
Windows sockets), **C++** (header-only, over the C driver: RAII,
exceptions, `std::variant` values), **Python** (pure Python, DB-API 2.0
style), **JavaScript/TypeScript** (Node, Promise API, typings), **Java**
(plain Java 17+, no dependencies).  Values map to each language's natural
types (exact decimals, dates and times, arrays).  `examples/` has the same
quickstart in every language.

**Done when:** every driver passes the same conformance cases (types,
parameters, errors, transactions) against `nucleus server` in CI. ✅
(Publishing them to crates.io, PyPI, npm and Maven Central follows the first tagged release, D15.)

### Developer platform (§M14.9–§M14.26)

What makes nucleus worth choosing, before more data models: understanding
queries, being usable by AI models, holding large data and analysing it
fast, vectors, search and embeddings for AI applications, nucleus
embedded in any application, browser or phone, the memory AI models
know from — and the
things nucleus's design gives almost for free (branches and time travel
from copy-on-write storage and MVCC), the everyday tools (JSON,
migrations, code generation), an easy start, and what production needs.

### §M14.19 — Modularity: registries and an extension API ✅ (DOCS §1, §8.11–§8.12, §9)

**Order:** this comes first in the developer platform — principles before
features — then §M14.26 (core types), §M14.25 (identity and access),
then §M14.9 onward.  It
starts with a **principle audit**: the
code checked against all twelve principles, each gap listed here with its
fix.

**Principle audit (2026-10-17).**  Principles 1 (the SQL layer and the
server depend only on `nucleus-kv`), 2 (every key type is memcomparable
and self-delimiting), 7, 8 (catalog, sequences and statistics live in the
KV space) and 9 (d-os code only in `integrations/d-os`) hold.  The gaps:

| # | Gap | Fix | |
|---|-----|-----|:-:|
| 3 | `Timestamp(pub u64)`: code above the transaction layer can build and read the raw counter (the d-os integration does), so an HLC could not replace it without breaking them | the field private; timestamps are compared, printed and parsed, never computed with | ✅ |
| 4 | `now()` reads `SystemTime::now()` directly, and `transact`'s back-off sleeps on `std::thread` with a jitter from the thread id — neither goes through the platform, so a simulation's `now()` is the host's clock and its retries wait in real time | the KV contract gives the clock (`Kv::wall_clock`, from the platform; a distributed implementation answers from its HLC) and the back-off (`Kv::back_off`, the platform's `Clock::sleep` with jitter from its `Rng`); the simulated clock advances instead of sleeping | ✅ |
| 4 | the server uses `std::net` directly | scheduled: a network interface in the platform, with a simulated network, comes with §M19 (as principle 4 says, "later network") | — |
| 5 | the storage and transaction layers are crash-tested, the SQL layer is not: no test crashes the simulation in the middle of DDL and DML run through `Session` | a SQL crash test: random DDL and DML on the simulation, a crash at a random point, reopen — the catalog, every index and the rows agree with each other and with what was reported committed | ✅ |
| 6 | the page format, WAL batches, rows, catalog records and `ncp` carry versions; the **key encoding and the SQL keyspace layout do not**, so a change to either could not be told from data written before it | a SQL-layer format record at key `00 00` (keyspace layout and key encoding versions), written when a database is first used by the SQL layer and checked on every session: an older version is read (or upgraded), a newer one refused with a clear error; per-type encoding versions come with the `Type` trait below | ✅ |
| 10 | memory is bounded in the core (buffer pool), not in the executor: sorts, hash joins, hash aggregation and materialised CTEs hold their whole input | spilling in §M14.11: sorts, CTEs, recursion and writes' rows (L6a ✅); aggregation, `DISTINCT`, set operations (L6b ✅); joins (L6c ✅) — left: a recursive `UNION`'s rows seen | — |
| 11 | PostgreSQL compatibility is not optional inside the engine: `pg_catalog`, `information_schema` and the `pg_*` functions (`pg_typeof`, `pg_get_constraintdef`, `has_table_privilege`, …) are bound in every session, `ncp` ones included; the parser and binder special-case `pg_catalog`; and `nc_catalog` is looked up *through* `pgcatalog::lookup` | `nc_catalog` stands on its own; the PostgreSQL views, functions, type names and protocol are the module `modules/pg-compat`, registered per session — on in sessions of the PostgreSQL front end, off on `ncp`; the parser no longer knows `pg_catalog`, `oid` or `::regclass` | ✅ |
| 12 | hard-coded lists: the value types (`DataType`, and a `match` on it in encoding, casts, output, the binder, statistics, `ncp` and `pg`), the scalar functions (`Func`, about seventy, and `Func::lookup`), the aggregates (`AggFunc`), the table functions (`TableFunc`), the catalog views (`ncatalog::definitions`, the `pgcatalog` table), the front ends (the CLI calls `serve` / `serve_pg`), authentication (`Config::accepts`, one user) | the registries and the extension API, below — scalar functions, aggregates, table functions, schemas, catalog views and types (`nucleus_sql::ext`), front ends and authentication (`nucleus_server::{FrontEnd, Authenticator}`) | ✅ |

Principle 12 applied to what exists.  Today these are hard-coded lists or
`match`es that a new entry has to edit: scalar functions (`Func`) and
aggregates (`AggFunc`), table functions, the value types, the catalog
views (`nc_catalog`, the compatibility catalogs), the server's front ends
(chosen in the CLI), the authentication check.

- Each becomes a registry behind a trait: a function, type, table
  function, catalog view, protocol front end or authenticator is one
  implementation and one registration line; the built-in ones register
  the same way.
- **Types are modules, and they evolve.**  A type is one implementation of
  a `Type` trait: its names and modifiers, text input and output, casts,
  operators and how it unifies with others, its row and key encodings, its
  `ncp` tag and each driver's mapping, its statistics.  So types can be:
  - *added* — a new type is a module (and user-defined types: domains,
    enums, composites, built on the same trait);
  - *modified* — `ALTER TABLE … ALTER COLUMN … TYPE` with a conversion
    (`USING`), applied online in the background, the column readable
    throughout;
  - *updated* — every stored value carries its type's encoding version;
    a new version of a type's encoding is added beside the old one, old
    values stay readable, and they are rewritten lazily or by
    `nucleus upgrade` (tied to D15).
  The wire tags and catalog type ids are assigned by the registry, not
  hard-coded, with the built-in ones fixed for compatibility.
- **Design (recorded before building it).**
  - *Performance first:* the built-in types keep their native `Value`
    variants (no indirection on the hot paths), but each is also a
    registered `Type`, and the binder, DDL, optimizer and catalog ask its
    capabilities — never the enum — whether something is allowed.
  - *A registered type's values* are `Value::Ext(type, bytes)`: the type's
    implementation (interned once per name, so `DataType::Ext` stays a
    small `Copy` value) and its **canonical** bytes — equality and hashing
    are on those bytes, so they agree by construction (a law the kit
    checks); order, text form, casts and the rest go through the type.
  - *Persisted by name:* the catalog stores a registered type by name and
    encoding version; rows store its values tagged as such, keys with its
    order-preserving key encoding.  A database naming a type this build's
    registry lacks still opens — other columns read, the table can be
    dropped — and using that column fails with a clear error naming the
    missing type.
  - *On the wire:* `ncp` sends a registered type's values as name and
    binary form; drivers map the names they know (`uuid`) to native types
    and hand the rest over as name and bytes.
  - *Steps:* the `Type` trait and capabilities, built-ins registered
    (T1 ✅); registered types stored, compared and sent, with the
    `geopoint` sample (T2 ✅); `UUID` everywhere (T3 ✅ — `ncp` version 2,
    all six drivers, pg-compat; its columnar encoding comes with the
    columnar storage of §M14.20); the law kit (T4 ✅ — it found arrays'
    key encoding disagreeing with their order: key encoding version 2);
    teaching abilities in SQL — orderings by a mapping, `USING`, indexes in
    one, `COLLATE` (T5 ✅); operators and casts taught in SQL, and the
    parser reading any operator symbol (T5b ✅ — assignment and implicit
    casts applied unwritten come with T6; naming distances in SQL with
    §M14.12); encoding versions and `ALTER COLUMN … TYPE` (T6 ✅ — one
    transaction under MVCC, readers keep the old column; chunked
    conversion of very large tables in the background is §M14.11's;
    `INSERT`/`UPDATE` applying taught assignment casts is still open); the
    sample extension (T7 ✅ — `examples/extension`, the `geo` module and
    the guide for writing one).
  - *Left for later, on purpose:* converting a very large column in the
    background in chunks (§M14.11); `INSERT` / `UPDATE` applying taught
    assignment casts unwritten; naming distances in SQL (§M14.12, with
    nearest-neighbour indexes); arrays of registered types; a
    nearest-first `ORDER BY` using an index (the index in an ordering is
    kept current but sorting still sorts).
- **`UUID`, the first new type built this way** — a real type proving the
  path, not only a sample: 16 bytes; text input in the canonical form
  (and upper case, braces, no hyphens), output canonical lower case;
  casts to and from `text`; equality, hashing, a byte-wise total order
  and a fixed 16-byte key encoding, so it can be a primary key and be
  indexed; generators `gen_random_uuid()` (version 4) and `uuidv7()`
  (version 7, time-ordered — the recommended key, since new rows land at
  the end of the index instead of all over it); `uuid_extract_version`
  and `uuid_extract_timestamp`; a columnar encoding and statistics; an
  `ncp` tag and each driver's native type (`[u8; 16]` or the `uuid`
  crate's type in Rust, a 16-byte struct in C and C++, `uuid.UUID` in
  Python, a string in JavaScript, `java.util.UUID` in Java); and in
  `pg-compat`, PostgreSQL's `uuid` (oid 2950) in text and binary form.
- **Type capabilities.**  A type is not one thing but a set of abilities,
  and many are optional — a geographic point has a distance but no natural
  order; a JSON value has equality but no arithmetic.  Each ability is its own
  interface that a type implements or not, and the engine *asks* before it
  relies on one:

  | Capability | What it enables | If the type lacks it |
  |------------|-----------------|----------------------|
  | text input / output, row encoding | literals, storage, the text wire format | required of every type |
  | equality | `=`, `<>`, `IN`, `DISTINCT`, `GROUP BY`, set operations, `UNIQUE` | those are refused at bind / DDL time |
  | hashing (consistent with equality) | hash joins and hash aggregation | the planner uses sorting or nested loops |
  | total order | `<` …, `ORDER BY`, `MIN` / `MAX`, range predicates, merge joins, histograms | refused (`ORDER BY` a point is an error, not an arbitrary order) |
  | order-preserving key encoding | primary keys, B-tree indexes, index range scans | the type cannot be a key or B-tree-indexed |
  | operators and functions | `+`, `||`, `->`, type-specific functions — each registered, typed | not available |
  | distance / similarity | `ORDER BY a <-> b LIMIT k`, nearest-neighbour indexes | not available |
  | casts | to and from other types, each marked implicit, assignment or explicit, with `unify` rules | only the casts declared |
  | aggregation | `sum`, `avg` and type-specific aggregates | refused |
  | partial update | changing part of a value in place (an array element, a JSON path) | the whole value is replaced |
  | statistics | distinct counts, histograms, selectivity | the optimizer uses defaults |
  | batch kernels | operators, comparisons, hashing and aggregates over a whole column batch at once (SIMD), for the vectorized executor (§M14.20) | the executor falls back to one value at a time |
  | columnar encoding | compressed column segments (dictionary, run-length, delta, bit-packing) with min / max per segment | the column is stored plainly |
  | modifiers | `(n)`, `(p, s)`; which changes need no rewrite (`varchar(10)` → `(20)`) | none, or every change rewrites |
  | encoding versions and upgrades | updating the type's stored form (see above) | the encoding is frozen |
  | conversion for `ALTER COLUMN … TYPE` | changing a column to another type | only with an explicit `USING` |

  The engine is taught through these declarations, never by special
  cases: the binder, planner, executor, index layer, statistics, wire
  protocol and drivers each consult the capability they need, and a
  missing one gives a clear error naming it at bind or DDL time (not a
  surprise at run time).  `nc_catalog.types` (and `nc_catalog.operators`,
  `nc_catalog.casts`) list every type's capabilities, so people, tools and
  AI models can see what a type supports.
- **Laws, checked for every type.**  A capability comes with laws the
  engine relies on, and a test kit runs them, property-based, on every
  registered type — built-in or extension: equality is an equivalence;
  hashing agrees with equality; the order is total and agrees with
  equality; the key encoding's byte order is the type's order and decodes
  back; text output parses back to an equal value; every encoding version
  reads what it wrote, and upgrades preserve equality; casts round-trip
  where declared lossless.  A type that breaks a law is not registered.
- **Teaching a type new abilities.**  Capabilities are not fixed when a
  type is created: any type — built-in, extension or user-defined — can be
  taught one later, and unlearn it, in the language itself, as ordinary
  transactional DDL recorded in the catalog:

  ```sql
  -- an ordering derived by a mapping into an ordered type
  ALTER TYPE geopoint ADD ORDERING by_geohash BY geohash(value);
  -- an operator, a distance, a cast
  ALTER TYPE geopoint ADD OPERATOR <-> (geopoint, geopoint) RETURNS float8 AS haversine;
  ALTER TYPE geopoint ADD DISTANCE great_circle USING <->;
  ALTER TYPE geopoint ADD CAST TO text AS geopoint_text ASSIGNMENT;
  ALTER TYPE geopoint DROP ORDERING by_geohash;

  SELECT * FROM shops ORDER BY at USING by_geohash;
  CREATE INDEX ON shops (at USING by_geohash);
  ```

  - *By mapping* (the preferred way): an ability is derived from another
    type through a deterministic function — an ordering by `geohash(p)`, an
    equality or a hash by a canonical form, a key encoding by the mapped
    key's encoding, statistics by the mapped values.  Everything follows
    from the target type's own capabilities, so the laws hold by
    construction; the engine only requires the function to be immutable.
    A mapping that is not one-to-one gives an ordering with ties, which
    sorts and indexes (the row id breaks ties) but cannot be a type's
    default order or back a `UNIQUE`.
  - *By an implementation*: a comparison, hash or encoding written as a
    function — in SQL, or in a module's Rust code (loadable modules
    later).  The engine checks the laws on the existing data when it is
    taught, and the law kit runs it in tests.
  - *Several of a kind*: a type may have more than one ordering (or
    distance); one may be its default, used by a plain `ORDER BY`, the
    others are named with `USING`.  Collations are this same mechanism for
    `text`: `COLLATE` names a text ordering (byte order built in at
    first), instead of being ignored as today.
  - *In the language*: an operator a type or module brings is registered
    with its symbol and one of a fixed set of precedence classes, and the
    parser looks it up; functions, aggregates and casts are found by name.
    The grammar itself stays fixed — a module extends the vocabulary, not
    the syntax.
  - *Dependencies*: the catalog records what uses each ability (indexes,
    constraints, views, defaults).  Dropping one in use is refused (or
    `CASCADE`); redefining one rebuilds what depends on it, online.
  - *Visible*: `nc_catalog.orderings`, `nc_catalog.operators`,
    `nc_catalog.casts` list what each type has been taught, and the error
    for a missing ability names the abilities that would work instead
    (`geopoint has no default ordering; available: by_geohash, or a
    distance with <->`), for people, tools and AI models alike.
- The front ends share one session layer, so a new protocol is a module
  over `Session` (as `ncp` and `--pg-compat` already are); the server
  starts the modules a configuration names.
- An extension API for out-of-tree modules (Rust crates linked into a
  custom build first; loadable modules later), with DOCS for extension
  authors and a sample extension in `examples/`.
- **Modules live in one repository but are built to leave it.**  Every
  module, edition, driver and binding is a folder that could become its
  own repository and package at any time, without changes:
  - the layout — `crates/` the core (KV, storage, transactions, the SQL
    engine) and the public API crates; `modules/` the modules (vectors,
    text search, embeddings, columnar storage, `pg-compat`, …);
    `editions/` the build profiles that choose modules (server, embedded,
    browser, mobile); `drivers/` and `bindings/` the language packages;
  - a module depends on the core only through the public API crates
    (Cargo path dependencies that also carry a version, so the same
    manifest works published), never on internal crates or on files
    outside its folder; non-Rust packages have their own manifest and no
    imports from outside their folder;
  - each folder has its own README, tests, changelog, licence and
    version; its documentation lives in the folder, and DOCS links to it;
  - shared test material is a package too: the slt runner and suites and
    the type law kit are published test-kit crates a module depends on,
    not files it reads from the repository.
- **Checked in CI, not left to discipline:** a boundary check in both
  directions — core crates do not depend on modules, and modules depend
  only on the public API; and a *detachability test* — each module is
  copied into an empty directory and built and tested there from
  versioned dependencies alone (`cargo package` for Rust crates).  When a
  folder does leave, `git filter-repo --subdirectory-filter` keeps its
  history.
- **First piece: `pg-compat`** ✅.  The PostgreSQL protocol (was
  `crates/server/src/pg.rs`) and the `pg_catalog` / `information_schema`
  views (was `crates/sql/src/pgcatalog.rs`) are `modules/pg-compat`,
  connected through the public API only (`nucleus_sql::ext`,
  `nucleus_server::FrontEnd`) — the whole path tried on a real, existing
  part before the others follow.  The CLI is the edition
  `editions/nucleus`; the test kit (`crates/testkit`: the slt runner and
  the SQL layer's scripts) is the first shared test package;
  `scripts/check-boundaries.py` and `scripts/detach-test.sh` run in CI
  (job `modules`).

**Done when:** the principle audit's gaps are closed or scheduled; the
built-in functions, types, catalog views, front ends and authentication
are all registered through the public extension API; the sample extension
adds a function, a type and a catalog view without touching the core;
`modules/pg-compat` passes the detachability test and `psql` still works
through it; `UUID` is registered like any extension type, passes the law
kit and round-trips through SQL, `ncp`, every driver and `pg-compat`
(checked against PostgreSQL); and
a type's encoding is upgraded on a populated database (old and new values
read side by side, crash-tested) and a column's type changed online.  The
law kit runs on every built-in type, and a sample type with only some
capabilities (equality and distance, no order — `geopoint`) is refused
exactly where it should be (`ORDER BY`, primary key, B-tree index) and
works everywhere else.  Then it is taught an ordering by a mapping
(`by_geohash`) in SQL: `ORDER BY … USING by_geohash` and an index on it
work, dropping the ordering while the index exists is refused, and
redefining it rebuilds the index; and `COLLATE` resolves to a text
ordering.

### §M14.26 — Core types ✅ (DOCS §8.1, §8.2, §10, §11)

**Order:** right after §M14.19, whose type registry they are built on —
each one registered like `UUID`, with its capabilities declared and the
law kit run on it.  The everyday types an application expects, which the
type set still lacks (today `smallint` and `integer` are names for a
64-bit integer, `real` for a 64-bit float, and `char(n)` for `text`):

- ✅ **Integers by width**: `smallint` (16-bit), `integer` (32-bit),
  `bigint` (64-bit), each with its own range (`22003` outside it),
  overflow checked per width, and promotion rules for mixed arithmetic
  (the wider operand's type); integer literals are `integer` if they fit
  (PostgreSQL's rule).  The row encoding stores any integer in the fewest
  of 2, 4 or 8 bytes (row version 2); keys keep 8 for every width, so
  indexes serve comparisons across widths and widening keeps the keys.
  The catalog never recorded which integer name a column was declared
  with (all were `bigint`'s tag), so a database from before opens with
  those columns as `bigint`, values intact; `ALTER COLUMN … TYPE
  integer` narrows one, checking every value.  On `ncp` all three are
  still tag 2 (8 bytes) until the protocol version that brings this
  milestone's types; `pg-compat` sends oids 21 and 23 with their binary
  forms.
- ✅ **`real`** (32-bit float) beside `double precision`, with PostgreSQL's
  output rules for each (shortest round-trip digits, `e+XX` notation past
  15 or 6 digits, `-0`), range errors on input and on overflowing or
  underflowing arithmetic; stored in 4 bytes, keyed as a double.  On
  `ncp` it travels as a double until the protocol version that brings
  this milestone's types.
- ✅ **`bytea`** (out-of-line storage for values of every type over 2 000
  bytes): binary strings — hex and escape text forms
  (`'\xdeadbeef'`), `||`, `substring`, `length`, `position`, `encode` /
  `decode` (hex, base64), digests (`sha256`, `md5`); byte-wise order and
  a key encoding; native types in every driver (`Vec<u8>`, `bytes`,
  `Uint8Array`, `byte[]`, pointer and length in C).  Values larger than a
  page are stored out of line in chunks, so a column can hold files and
  model artefacts.
- ✅ **`time`** (time of day, microseconds), with `date + time`, intervals,
  `extract`, `make_time`, `localtime`.  (PostgreSQL has no `date_trunc`
  of a time — it casts it to an interval, whose `date_trunc` nucleus
  lacks yet; `time(p)` keeps µs, as timestamps do, until precision
  modifiers are stored.)
- ✅ **`char(n)`** with the standard's blank padding and comparison rules
  (columns declared `char(n)` before were text, and stay text).
- ✅ **`inet` and `cidr`**: IPv4 and IPv6 addresses and networks;
  containment operators (`<<`, `<<=`, `>>`, `>>=`, `&&`), `host`,
  `network`, `masklen`, `family`, `broadcast`; an order and key encoding
  in which a network's addresses are one contiguous range, so containment
  is an index range scan.  The access rules (§M14.25) and the audit log
  use them.
- ✅ **Enums**: `CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')`, ordered
  by declaration; `ALTER TYPE … ADD VALUE … BEFORE / AFTER` and
  `RENAME VALUE` without rewriting a row — values are stored by a sort
  key that leaves room between labels, so a new label fits anywhere.
- For each: text and binary forms, casts, the `ncp` tag, every driver's
  native type, statistics, a columnar encoding (§M14.20), and in
  `pg-compat` PostgreSQL's type with its oid and binary format.

**Done when:** each type passes the law kit; an slt script per type,
filled from PostgreSQL 16, covers input, output, casts, operators,
functions, ordering and errors; every type round-trips through `ncp`,
all six drivers and `pg-compat` (binary formats checked against
PostgreSQL); and a database created before this milestone opens with its
old `integer` columns intact.

*Shipped (2026-10-22):* all of it — slt scripts 23–29 filled from
PostgreSQL 18 (pg-compat's `02_types.slt` for the types through
`pg_typeof`), binary forms checked against PostgreSQL 18's `*send`
functions, `ncp` version 3 in all six drivers, old `integer` columns open
as `bigint`.  On the way: `ncp`'s own value encoder (storage encodings no
longer leak into the protocol), `INSERT … VALUES` bound against its
target columns, PostgreSQL's float output and range errors for doubles
too, `position(… IN …)` and `substring(… FROM … FOR …)`, `repeat()`, and
`ScalarFunction::implied_range` in the extension API.

*Left for later:* columnar encodings (they come with §M14.20); `time(p)`
and `timestamp(p)` precision (µs are kept until type modifiers are
stored); `date_trunc` of intervals (PostgreSQL's of a time goes through
one); arrays of enums; an oid per enum in pg-compat (they travel as text);
`sum(bigint)` as `numeric`, as PostgreSQL's (nucleus's is `bigint`, an
overflow an error); `inet ± integer` arithmetic.

### §M14.25 — Identity and access ✅

*Steps:* T1 users and roles ✅ (roles in the catalog, membership, `SET
ROLE`, SCRAM-SHA-256 verifiers, owners, logins from the catalog once
claimed, `nucleus init`); T2 privileges ✅ (tables and columns, schemas,
functions, types, the database; grant options, `PUBLIC`, default
privileges; sequences, streams, branches and modules get theirs when they
exist, and pg-compat's `relacl` / `\dp` is left for later); T3 row-level
security ✅ (policies under a barrier; vector, text and stream reads get
them as those arrive — §M14.12, §M14.21, §M16.5; the connection context
in `current_setting` comes with T5); T4 authentication methods (SCRAM on
the wire ✅ — `ncp` version 4 in every driver, and pg-compat's SASL;
tokens ✅ — read only, table scope, expiry, revocation, through every
driver and `psql`; schema and branch scopes when those exist, row, time
and cost budgets with T6; TLS ✅ — the `tls` module over rustls, `ncp`
by a handshake before Hello and pg-compat by `SSLRequest`, in every
driver (C with OpenSSL behind a build flag); client certificates ✅ —
mutual TLS, a certificate naming the user stands in for its password, in
every driver; SCRAM-SHA-256-PLUS ✅ — channel binding on both front ends
and in every driver; OIDC ✅ — the `oidc` module (JWTs by OAUTHBEARER,
static or discovered keys, claims as read-only settings, a role claim's
roles for the session) in every driver, against a test issuer;
pg-compat's OAUTHBEARER, which libpq 18 speaks, is left for later; LDAP
✅ — the `ldap` module, for roles without a password of their own, the
password asked for only over TLS); T4 done; T5 access rules and privileges
by connection context ✅ (ordered rules with READ ONLY / AS ROLE ceilings,
`GRANT … WHEN` memberships, the connection as read-only settings and in
`nc_catalog.sessions`, the PROXY protocol; the MCP and sync front ends get
the same rules when they exist, §M14.10, §M16.7); T6 protection ✅
(database settings in SQL, password rules and lifetime, failed logins
slowed and locked, connection limits, statement timeouts, token budgets
of rows, scan and rate — a cost budget comes with §M14.9's costs — and the
insecure start refused); T7 audit ✅ (a hash-chained log in the catalog,
written with what it records, statements by role or table on request; a
broken chain found). Left for later: pg-compat's OAUTHBEARER, `relacl` /
`\dp`, pruning the audit log and its stream (§M16.5), cost budgets
(§M14.9), the MCP and sync front ends' rules (§M14.10, §M16.7).

**Order:** right after §M14.19 and §M14.26 (whose `inet` / `cidr` the
access rules use), before the rest of the developer platform:
much of it builds on roles and row-level security — the MCP server's
write access (§M14.10), filtered vector search (§M14.12), hybrid search
(§M14.21), memory for models (§M14.24), sync (§M16.7).

Before this milestone there was one user and one password, given on the
server's command line, sent in the clear, and `--no-auth` admitted
anyone.  Roles, privileges, row-level security, SCRAM, tokens and TLS are in
now (the steps above); until access rules (T5) can require TLS, a client
that does not ask for it still connects in plain text.

- **Users and roles, in SQL and in the catalog** (principle 8):
  `CREATE ROLE`, `CREATE USER` (a role that may log in), `ALTER`, `DROP`;
  membership with inheritance (`GRANT role TO role`); an owner for every
  object; an administrator role.  `nc_catalog.roles`, `role_members` and
  `grants` show them (`pg_roles` and `pg_auth_members` as compatibility).
  `nucleus init` creates the first administrator with a password it asks
  for or takes from the environment.
- **Privileges on every object**: databases, schemas, tables, columns,
  functions, types, sequences, streams, branches, modules — `SELECT`,
  `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE`, `REFERENCES`, `USAGE`,
  `CREATE`, `EXECUTE`, `CONNECT`, `WITH GRANT OPTION`, and default
  privileges for objects not yet created.  Checked at bind time, and a
  refusal names the privilege that is missing and the object (`42501`).
- **Authentication methods are modules** (the authenticator registry of
  §M14.19):
  - passwords by SCRAM-SHA-256 — `ncp` gains a challenge–response
    handshake in its next protocol version (negotiated, D15), so a
    password never crosses the wire;
  - **tokens**: API keys for programs and AI agents — scoped (read only,
    a schema, a branch, row, time and cost budgets), expiring,
    revocable, stored hashed;
  - client certificates (mutual TLS);
  - OIDC / JWT single sign-on, with the token's claims mapped to roles and
    to the session's context;
  - LDAP.
  TLS for `ncp` and `pg-compat`, which an access rule can require.
- **Access rules: who may connect, from where, how.**  Ordered rules in
  the catalog, managed in SQL, the first match deciding: conditions on the
  source address (CIDR), the front end (`ncp`, `pg-compat`, MCP, sync),
  the database, the user or role, the authentication method and TLS; the
  outcome is to reject, or to admit with a given method.  Behind a load
  balancer the real client address is taken (PROXY protocol) only from
  proxies the configuration names.
- **Privileges by connection context.**  The same user may have different
  rights depending on where and how they connect:

  ```sql
  -- alice from 10.2.3.1: everything she has been granted
  CREATE ACCESS RULE alice_desk  FOR alice FROM '10.2.3.1/32' ALLOW;
  -- alice from 10.2.3.2: read only
  CREATE ACCESS RULE alice_kiosk FOR alice FROM '10.2.3.2/32' ALLOW READ ONLY;
  -- the admin role only from the office network, over TLS with a certificate
  GRANT admin TO alice WHEN (source <<= '10.2.0.0/16' AND auth = 'certificate');
  ```

  - a rule can set a *ceiling*: `READ ONLY`, or `AS ROLE r` (only what
    `r` may do) — always intersected with the user's own privileges, so
    a rule can narrow them but never widen them;
  - a role membership can carry a *condition* on the session's context
    (source address, front end, authentication method, TLS, a time
    window), so the role is active only when it holds;
  - the effective privileges are fixed at login, re-checked on
    `SET ROLE`, and shown — `nc_catalog.sessions` lists each session's
    active roles and the rule that admitted it (`\conninfo` too);
  - the context is visible to row-level security
    (`nucleus.client_addr`, `nucleus.auth_method`, …), so a policy can
    use it as well;
  - an address alone is weak identity (shared networks, NAT, spoofing on
    a local segment): rules can require TLS or a certificate as well, and
    the documentation recommends it for anything beyond read access.
- **Row-level security**, for multi-tenant and AI applications in which
  a user, or an agent acting for one, must see only their own rows:
  `CREATE POLICY` on a table (for `SELECT`, `INSERT`, `UPDATE`, `DELETE`)
  with predicates over the row and the session's context
  (`SET nucleus.tenant = …`, the roles, claims from the login, the
  connection context).  Policies apply everywhere a row is read — scans,
  indexes, joins, vector and text search (§M14.12, §M14.21), streams and
  live queries — and the planner adds them before optimizing, so no plan
  can leak a row.  MCP sessions (§M14.10) run as a role or token under the
  same policies.
- **Protection**: connection limits per user and role; rate limits on
  logins and statements; slowing, then locking, an account after failed
  logins; password rules and expiry; and the server refuses to listen
  beyond loopback with the default password or `--no-auth` unless told
  `--insecure`.
- **Audit**: logins (success or failure, source, method, the rule that
  matched), DDL, and every change to roles, privileges, rules, tokens and
  policies, always; statements optionally, per role or table.  Written to
  a log and, from §M16.5, a stream; hash-chained, so a gap or an edit
  shows.
- **Embedded editions and devices** (§M14.23, §M16.7): an embedded
  database has no login — the file's permissions guard it — with optional
  encryption at rest by a key the application supplies; synced devices
  authenticate with device-bound, revocable tokens.

**Done when:** privileges on every object kind and row-level security are
enforced in slt tests, with leak tests (an error, a plan or a search
ranking never reveals a hidden row); every authentication method works
with every driver (OIDC against a test issuer); access rules are tested
over real sockets from several source addresses — the same user with
full rights from one and read only from another, and a role active only
under its condition; the server refuses an insecure start; failed logins
are throttled; and the audit log records every login and every change of
access, with a broken chain detected.

### §M14.9 — SQL analyzer ✅ (DOCS §8.13)

nucleus has the catalog, the statistics and the optimizer in one place;
the analyzer turns that into answers:

- `EXPLAIN ANALYZE`: the plan with actual rows, loops and time per node
  beside the estimates (a large gap points at stale statistics).
- Statement statistics in `nc_catalog.statements`: per normalized
  statement (constants replaced) its calls, total and mean time, rows read
  and returned, errors; reset by a function.
- A linter (`ANALYZE QUERY <sql>`, `ncsql \lint`): full scans under a
  selective filter, foreign key columns without an index, cartesian
  products, `NOT IN` over a nullable subquery, comparisons that defeat an
  index, `SELECT *` in views later.  Each finding names the rule, the place
  and the fix.
- An index advisor: from the statement statistics and `ANALYZE` data, the
  indexes that would have paid off, with the plans they would give.

**Done when:** each lint rule and the advisor have slt cases, and `EXPLAIN
ANALYZE`'s actual row counts are checked against the results.

**Steps.**  A1 `EXPLAIN ANALYZE` ✅ (every node's rows, starts and time
beside the estimates, `TIMING OFF`; the time from `Kv::elapsed`, the
platform's monotonic clock); A2 statement statistics ✅
(`nc_catalog.statements`, `nc_statements_reset()`; in the process, by
database); A3 the linter ✅ (`ANALYZE QUERY`, `ncsql \lint`: five rules,
each with slt cases that fire and that stay quiet; `SELECT *` in views
waits for views); A4 the index advisor ✅ (`nc_catalog.index_advice`:
candidates from the recorded statements' full scans, tried as
hypothetical indexes, each statement counted for the one that serves it
best).  Left for later: `EXPLAIN ANALYZE` of writes, a cost model (and
with it cost budgets, §M14.25), statement statistics that survive a
restart or span a cluster, indexes for joins (with index nested loops).

### §M14.10 — AI access ✅ (DOCS §6.5, §8.14, `docs/llm.md`)

Make nucleus a tool AI models can pick up in seconds, and give them the
context they need:

- `nucleus mcp`: a Model Context Protocol server (stdio, over a local
  directory or a server) with tools to list and describe tables (from
  `nc_catalog`), run read-only queries (row and time limits), `EXPLAIN`,
  sample rows, and — when enabled — write statements.
- `COMMENT ON TABLE / COLUMN`, kept in the catalog and shown in
  `nc_catalog` and `\d`: the schema explains itself to people and models.
- Errors with a hint beside the SQLSTATE (the closest column or table
  name, the cast that would work), on the wire and in every driver.
- A compact, model-oriented reference of the dialect (`docs/llm.md`).

**Done when:** an MCP client lists, describes and queries a database end to
end in a test, and hints appear for the common error codes.

**Steps.**  B1 comments ✅ (`COMMENT ON TABLE / COLUMN`, in the table
record, nc_catalog, `\d`, pg-compat's descriptions); B2 hints ✅ (the
name meant or the names there are, the cast, by code for the rest; `ncp`
version 5 and pg's `H` field, every driver, ncsql); B3 `nucleus mcp` ✅
(the `mcp` module over stdio: list, describe, sample, read-only query
with row and time limits, explain, lint, writes when allowed; for a
directory or a server; end to end in the edition's tests); B4
`docs/llm.md` ✅ (the dialect, what is missing and what to use instead,
the catalog, hints, `EXPLAIN` and the linter, the common codes).  Left
for later: MCP over HTTP (Streamable HTTP) for remote clients, resources
and prompts, typed parameters (they are bound as literals), hints for
syntax errors.

### §M14.11 — Large data foundations ✅ (DOCS §5.2, §8.5, §8.7, §10, `docs/benchmarks.md`)

Today results are materialized, joins and sorts are in memory, and `ncp`
sends a whole result at once.  Before claiming large data:

- Streaming results: `ncp` sends rows as they are produced, with flow
  control; drivers iterate (cursors) instead of collecting.
- **Columnar results.**  A third `ncp` result format: batches in which
  each column is one contiguous, typed buffer (Arrow-compatible layout,
  with a validity bitmap for NULLs).  Drivers hand them over without
  building a value per cell — the Python driver as numpy arrays
  (`np.frombuffer`, zero-copy), Arrow tables, pandas and polars frames;
  JavaScript as typed arrays; Java as primitive arrays or buffers; C and
  C++ as pointers into the batch.  Today the Python driver builds one
  object per value, which costs far more than the query; this removes it.
- Bulk load: `COPY`-style loading (text/CSV and `ncp` batches) and batched
  inserts in the drivers, bypassing per-row statement overhead — and the
  columnar direction in reverse: a numpy array, Arrow table or DataFrame
  loaded into a table in one call.
- A memory-bounded executor: sorts, hash joins and aggregates spill to
  temporary files past a budget.
- Parallel scans over key ranges; page compression.
- **Benchmarks** (not urgent — performance work comes first, with
  micro-benchmarks beside it): a suite with PostgreSQL as the reference,
  on the same machine and data: TPC-H and ClickBench for analytics, a
  pgbench-like workload for transactions, load and point reads, and an
  ANN-recall suite for vectors (§M14.12).  It runs in CI to catch
  regressions, and its results are published with the commands to
  reproduce them.  Targets are set from the first run and recorded here.

Speed alone does not call for an embedded Python mode: with columnar
results the network adds little on one machine.  Starting without any
setup does, and that is §M14.23.

Vectorized execution and columnar storage (§M14.20) and range partitioning
(§M18) build on this.

**Done when:** a 100-million-row table loads, scans, sorts and joins within
a fixed memory budget; a 100-million-value column reaches numpy through
the Python driver without a per-value object, at a rate bound by the
network, not the driver; and the benchmark suite runs in CI against
PostgreSQL.

*How it holds (2026-10-27).*  Memory: every place a statement held its
input whole now stops at a budget and spills — transactions past
`txn_memory`, operators past `work_mem` —, each checked by running the same
work small and large (crates/txn/tests/large.rs, crates/sql/tests/spill.rs,
the crash tests).  Measured on one machine: a load, count, sort and
self-join peak at 917 MB with 2 million rows and 646 MB with 10 million —
the peak does not grow with the data (it is the budgets: cache, `work_mem`
per operator, `txn_memory`, which the size estimates undercount by about
half).  100 million rows were not run here for lack of disk (10 million
take 1.6 GB); CI's benchmark runs a million.  numpy: 10 million `bigint`
values reach numpy in 5.3 s, of which the server's scan alone takes 4.5 s
— the driver and the network add 0.8 s (80 ns a value), against 13.7 s for
rows; the bound is the server's scan, §M14.20's.  The benchmark suite
runs in CI against `postgres:18` (job "benchmarks against PostgreSQL").

**Steps.**  L1 streaming ✅ (the executor hands rows to a sink as it
produces them — `Session::run_into`; `ncp` version 6 sends them so, with
a fetch size, `S` after each batch and `F` / `c` from the client; the Rust
driver's `Cursor`; pg-compat streams simple queries and Execute without a
row limit; a cursor's waits are not statement time); L2 cursors in the
Python, JavaScript, Java, C and C++ drivers ✅ (Python and JavaScript
`stream`, the DB-API cursor lazy; Java, C and C++ `cursor`; the next
query ends one left open); L3 the columnar result
format ✅ (`ncp` version 7: batches `B` of a buffer per column — fixed
widths back to back, variable widths as Arrow's offsets and bytes, a
validity bitmap, every buffer 8-byte aligned; the Rust driver's
`Client::batches`); L4 columnar results in the drivers ✅ (Python numpy —
fixed widths without a copy —, Arrow — in place where its layout is the
batch's —, pandas, polars; JavaScript typed arrays; Java buffers; C and
C++ pointers; each checked cell by cell against rows); L5 bulk load ✅
(`COPY` text/CSV and `ncp` batches — `COPY t FROM STDIN`, each row an
INSERT's checks, atomic; `ncp`'s `I` / `d` / `B` / `e` / `f`; the Rust
driver's `copy_in`, `copy_rows`; pg-compat's sub-protocol; `ncsql
\copy`; `copy` in every driver, and `load` of a DataFrame, an Arrow
table, columns or rows in one call — Python, JavaScript, Java); L5b large
transactions ✅ (past `txn_memory` a transaction's writes spill into the
store's side area, checked as they spill; the commit is still one atomic
batch — a status record —, after which the side area moves into
versions while readers merge it; crash-tested at every I/O point; a
single-node form of §M21's write intents.  Left: a transaction's read
set — validated foreign key reads — is still in memory); L6 spilling
past `work_mem` to scratch space (the KV contract's `scratch()`): L6a ✅
(external merge sorts; materialised CTEs, recursion and the rows writes
collect; streaming `generate_series`; ANALYZE sampling 30 000 rows), L6b
✅ (hash aggregation, `DISTINCT`, `UNION`, `INTERSECT` / `EXCEPT`: memory
up to the limit, then hash partitions taken in turn; a recursive
`UNION`'s rows seen are still held), L6c joins ✅ (grace hash joins with
NULL keys padded through, block nested loops, a full join keeping only
a matched bitmap); L7 performance,
measured before adding parallelism ✅ (a 2-million-row load took 197 s:
the B+tree decoded every page on a lookup's path and applied a batch key
by key — pages are now read in place, batches applied in one sorted pass,
checksums slicing-by-8, the cache larger, scans decoding only the columns
used; 11 s).  Moved on, where they belong: parallel scans to §M14.20's
morsel-driven parallelism (the executor's per-row work, not the lack of
cores, is what the measurements show), page compression to §M14.20's
columnar segments, and converting a very large column in the background
in chunks to §M14.18 (`ALTER COLUMN … TYPE` stashes its rows and spills
meanwhile, so it is bounded in memory); L8 the benchmark
suite against PostgreSQL, in CI ✅ (`bench/run.py`: one workload — load,
scans, a sort, a join, reads by key, updates, deletes — through each
system's own client and server; CI against `postgres:18` with the table
in the job summary; results, targets and how to reproduce in
`docs/benchmarks.md`; TPC-H and ClickBench since, run locally
(`bench/tpch.py`, `bench/clickbench.py`, §M14.20); a pgbench-like mix and
ANN recall (§M14.12) to follow).

### §M14.20 — Analytics: vectorized execution and columnar storage ✅

Runs after §M14.11 (its number only reflects when it was planned).  Why
nucleus can compete with PostgreSQL on large data: PostgreSQL stores rows
in a heap and executes one row at a time, which suits transactions and
limits analytics (hence its columnar extensions).  An engine that works
on column batches with compiled, SIMD kernels and reads only the columns
and segments a query needs is commonly 10–100× faster on scans and
aggregates — an architectural gain, not a tuning one.  For transactions
the aim is parity with PostgreSQL; for analytics, to beat it clearly; and
to do both in one system, with vectors and streams beside them
(principle 11), where PostgreSQL needs extensions.

- **Vectorized executor.**  Operators pass batches (about a thousand
  rows) of typed column buffers with selection vectors, instead of one
  `Value` row at a time; expressions evaluate a batch per call and are
  fused where they can be (`a * b + c` in one pass, no temporary column).
  Types provide batch kernels as a capability (§M14.19); one without them
  runs value by value, so nothing is refused, only slower.
- **SIMD kernels** for the built-in types — integers, floats, `numeric`,
  date / time, booleans, strings (comparison, hashing, `LIKE`), `vector`
  distances — chosen at run time for the CPU (AVX2, AVX-512, NEON), with
  scalar versions as the reference the law kit compares them against.
- **Parallel execution**: a query's scans, joins and aggregates split
  across cores (morsel-driven), the KV contract giving split points of a
  key range and snapshots other threads can read (moved here from
  §M14.11, whose measurements put the executor's per-row work first:
  `docs/benchmarks.md`).
- **Page compression** for row storage too (moved from §M14.11), beside
  the columnar segments' encodings.
- **Columnar storage.**  Compressed column segments (dictionary,
  run-length, delta, bit-packing — the types' columnar encodings), min /
  max per segment to skip what a predicate rules out, only the needed
  columns read.  Writes go to a row-format delta store and are merged into
  segments in the background; deletes and updates mark rows in a
  per-segment visibility map, so MVCC visibility is the same as for rows.
  The segments are KV values keyed by (table, partition, column,
  segment), so columnar storage sits *on* the KV contract, not beside it:
  transactions, crash safety and, later, distribution (§F3) come with it.
- **Table storage is a module** (principle 12): a registry of storage
  implementations behind one trait — `row` and `column` built in, others
  (an append-only time-series store, say) added the same way.
- **The storage mode is chosen per table, per column and per partition**
  — all three are required in this milestone, none deferred:

  | Mode | Suits | Costs |
  |------|-------|-------|
  | `row` (the default) | point reads, small writes, updates | scans read every column |
  | `column` | events, logs, time series, analytics — many rows, few columns per query | point updates are dearer (delta store, then merge) |
  | `row, column` (HTAP) | transactions and analytics on the same table | space and write cost for both copies |

  ```sql
  CREATE TABLE events (...) WITH (storage = column);
  ALTER TABLE orders SET STORAGE row, column;                    -- per table
  ALTER TABLE orders SET STORAGE column (amount, status, created_at);  -- per column
  CREATE TABLE metrics (...) PARTITION BY RANGE (at);
  CREATE TABLE metrics_2026_10 PARTITION OF metrics
    FOR VALUES FROM ('2026-10-01') TO ('2026-11-01') WITH (storage = row);
  ALTER TABLE metrics SET STORAGE POLICY (column AFTER interval '30 days');
  ```

  - *Per table*: `row`, `column` or both, both copies maintained in the
    same transaction.
  - *Per column*: in a `row, column` table only the named columns get a
    columnar copy; the optimizer uses it when a query's columns are
    covered, and otherwise joins the copies by row id or uses the rows.
  - *Per partition*: declarative partitioning — `PARTITION BY RANGE`,
    `LIST` and `HASH`, with partition pruning at plan and run time, and
    `ATTACH` / `DETACH` / `DROP PARTITION` online.  Each partition has its
    own storage mode, so the hot, recent partition can be `row` and the
    old ones `column`; a storage policy moves partitions between modes by
    age.  (These are SQL partitions on one node; §M18 later places them
    on KV ranges.)
  - Every change of mode is online — built in the background, the table
    readable and writable throughout, switched over atomically — like
    `ALTER COLUMN … TYPE`.
  - The optimizer chooses rows or columns per query and per partition
    (point lookups from rows, aggregates from columns); `nc_catalog`
    shows each table's, partition's and column's storage, and the
    analyzer (§M14.9) suggests changes from the workload it observes.
- Results stay columnar end to end: a batch from a columnar scan goes out
  as an `ncp` columnar result (§M14.11) and into numpy without being
  turned into rows.

**Steps.**  V1 batch reads through the stack ✅ (a profile of a
million-row `sum` put half its time in the allocator: the B+tree, the
engine and the executor each copied every entry into fresh vectors — the
KV contract's `scan_chunks` packs entries into one reused buffer, and the
B+tree and the engine fill it through reused buffers; `count(*)` 360 →
200 ms, `sum` 430 → 265 ms; what is left is the executor's value per
cell);
V2 the vectorized executor ✅ (pipelines of a scan and the filters and
projections above it a batch at a time — typed vectors with a selection;
kernels for integer, double and boolean arithmetic, comparisons and logic,
the rest value by value or row by row; vectorized aggregates through the
`Aggregate::grouped` capability; a failing batch run again row by row, so
results and errors are the row executor's; `batch_size`, the test kit's
scripts run with it at 0, 3 and 1024; `sum` 265 → 90 ms.  Left for later
steps: joins and sorts in batches); V3 SIMD kernels chosen at run time,
each against its scalar reference ✅ (`nucleus-kernels`: dense loops
compiled per level — AVX2, AVX-512 detected on x86-64, NEON on AArch64 —
for integer and double comparisons and arithmetic, sums, minima and
maxima, selection, string equality and `LIKE`, hashing; text and date /
time vectors; every kernel at every level the CPU has checked against
its scalar reference, the references against SQL's definitions; the
crate is the workspace's one exception to forbidden unsafe code, for the
call into a detected level.  Numerics stay value by value; `vector`
distances come with the type, §M14.12); V4 parallel execution ✅ (the
KV contract's `split_points`, `shared` reads and `parallelism`; morsels
of a pipeline's scan on worker threads, aggregates combining partial
states — a capability — and filtering pipelines giving their rows by
rounds, both in key order, so results, their order and errors are a
single pass's; exact sums of doubles; `parallel_workers`; scans and
aggregates of a million rows 1.5–1.8× PostgreSQL's ten workers, from
5–6×.  Left: joins and sorts in parallel); V5 page compression ✅ (leaves store
their keys' shared prefix once — a million-row table's file 93 → 61 MB —
and values of 1 KiB or more (64 bytes at first; rows that short cost a
decompression per read) are LZ-compressed when that saves an
eighth, by a codec of the storage layer's own; format version 2, version
1 still read.  Compressing whole pages across rows is left to the
columnar segments, V7, where it pays); V6 table
storage as a module ✅ (`TableStorage`: writes, reads and ranges by row
key, batches, the integrity check's part; a process-wide registry modules
add to, `row` behind it; `CREATE TABLE … WITH (storage = …)`,
`nc_catalog.tables.storage`; a module's storage tested through every
path); V7 columnar storage
✅ (`column`: segments of up to 32 768 rows with disjoint key ranges, a KV
value per column — bit packing, runs, dictionaries —, content-addressed so
decoded columns cache; a delta store in the row format; deletion markers
by row (since V8); a statement's inserts and updates
gathered and written to segments when many; scans of only the wanted
columns, segments skipped by least / greatest; `nc_catalog.segments`; the
same queries on `row` and `column` tables compared, a delete and a merge of
one segment shown both to hold; a million-row `sum` 22 → 2 ms.  Left: a
merge in the background, and segments compressed by LZ too); V8 storage per
table and per column, converted online ✅ (`row, column` keeps both copies,
the column copy of every column or some, scans of its columns reading it;
`ALTER TABLE … SET STORAGE` converts in parts while others read and write
— small switches, every write reading the table's record validated, a
delta moved over after its switch, markers kept by row so merges and
deletes do not conflict; converted through every pair with writes
between and during, and crashed at I/O points, against a row table;
choosing rows or columns per query is by the columns a scan needs.
Left: the analyzer suggesting storage changes, §M14.9); V9 partitioning (range,
list, hash ✅; pruning when the plan is made and when it runs ✅;
inserts routed and updates moving rows ✅; storage per partition ✅;
attach / detach ✅; the age policy ✅); V10 results columnar end to end ✅ (a vectorized query's batches go
into the columnar result as columns, in parallel too — a million numeric
rows into numpy 262 → 92 ms from a row table, 241 → 51 ms from a column
table); TPC-H against PostgreSQL measured (`bench/tpch.py`, SF 1: every
result agrees; first 6.5–7.9× PostgreSQL's time, then (2026-11-01) 0.47× on column
tables and 1.11× on row tables in the geometric mean — 0.25× and 0.77×
when §M14.20 closed —, 18 of the 22
queries faster on column tables; `EXISTS` / `IN` unnested into semi and
anti joins on the way) ✅; ClickBench (`bench/clickbench.py`, the first
million rows: every result agrees, but for ties at a `LIMIT`; 0.24× of
PostgreSQL's time on a column table, 2.13× on a row table; 0.11× and
0.89× when §M14.20 closed) ✅; the
targets recorded ✅ (`docs/benchmarks.md`, Targets: column tables at most
0.8× PostgreSQL's time on TPC-H and 0.5× on ClickBench in the geometric
mean, no query above 3×; row tables within 2×; loads, updates and
deletes within 2×, scans, sorts and joins within 3×, reads by key at
least even — met so far: TPC-H and ClickBench on column tables, TPC-H
on row tables, and every `bench/run.py` target: loads, updates and
deletes, scans, sorts and joins, reads by key; left: TPC-H Q19,
ClickBench Q18 and Q32 above 3×, ClickBench on a row table, the TPC-H
and ClickBench loads); V11 write intents among the versions ✅ — a large
transaction's spilled writes go straight into the version keyspace as
*intents* (a version stamped with the transaction's id, not a commit
timestamp: readers resolve it by the transaction's status — its own,
committed at a timestamp, aborted —, conflicts see it as the write it
is), instead of a side area moved into versions after the commit: every
write of a load, an index build or a large `UPDATE` / `DELETE` written
once; committed intents rewritten as versions lazily (by vacuum, or the
next writer of the key), aborted ones dropped the same way; recovery
reads the statuses, an unfinished transaction's intents aborted.  The
guarantees stay: snapshot isolation and serializability, first committer
wins, atomic commit (one status record), bounded memory; tested as the
side area is — every I/O point crashed, concurrent writers, the
simulation — and against it (the same histories, the same results).  The
single-node form of §M21's write intents, which this makes the
mechanism itself; V12 statistics across columns ✅ — distinct counts of
column groups and functional dependencies (as PostgreSQL's `CREATE
STATISTICS`, and gathered on their own for the columns of every key and
index), so filters, joins and groupings on several dependent columns are
estimated from them rather than multiplied as independent (equalities by
the chain rule over the groups counted, joins by the combinations of the
side with more, `GROUP BY` by its columns' combinations); V13 what other
engines do, taken over (2026-11-01: DuckDB, ClickHouse, HyPer / Umbra,
Velox, the numeric libraries) — key filters from hash joins ✅ (a
join's integer keys — their range, a list, a Bloom filter — narrow the
scans below its probe side and skip column segments; DuckDB's
join-filter pushdown, made in every executor alike); eager aggregation ✅
(a grouping by a join's key below the join: Yan & Larson; TPC-H Q13 —
the groupjoin, fused with the join, Moerkotte & Neumann, later if it
measures); the aggregation table — merged by hash part on the workers
past many groups, groups in key order finished on the workers, compact
partial states, a hash array with a salt beside the groups' keys (the
keys hashed and compared from the batch's vectors), direct indexing for
a narrow integer range ✅ (ClickHouse's two-level tables, DuckDB's salt
and perfect hash; TPC-H Q18, Q1); predicates on a dictionary's
values ✅ (a text column's `=`, `IN` and `LIKE` once per distinct value,
then its codes: 1.3–3.6× on such filters, the segment cache holding
codes, not text); prefetching in hash probes ✅ (Chen & Ailamaki: a
batch's lookups before its output, so their misses overlap — safe code,
no prefetch instruction; 2–8 % on the joins — and, found on the way,
join tables on two integer keys and build sides that are no pipeline
gathered in columns, not spilled as rows: TPC-H Q9); division by
constants without dividing (libdivide) — measured, not taken: 0.6 →
0.37 ns a row on AArch64, and no query of the suites divides a row by a
run-time constant (the date functions' constants the compiler's
already); FSST string compression with `LIKE` on the compressed strings
— deferred: the suites' `LIKE` filters already run 7–14× faster than
PostgreSQL's on dictionary codes and the substring kernel, so it waits
for a storage-size goal; and expressions fused into one loop (the numeric libraries' expression
templates) — for decimals, typed vectors ✅ (one scale's mantissas, `+ −
*` and comparisons on them, sums straight from them, no value a row:
TPC-H Q1 2.5×, Q6 1.7×).

**V14 — the whole engine reviewed (2026-11-02).**  Before the remaining
targets, the TPC-H load was profiled phase by phase (SF 1, row tables,
against PostgreSQL 18 in the same session: `COPY` 27.6 s against 18.3,
`CREATE INDEX` 40.3 s against 5.5, `ANALYZE` 11.5 s against 5.3; a
column table's `lineitem` `COPY` 36.5 s against 13.4) and then every
layer read for speed — storage, transactions and the KV contract, the
SQL write paths, the executor, columnar storage and the planner, the
server, protocol and drivers — asking also what is built twice and what
existing machinery a fix should reuse.  The rule taken from it (with
`CLAUDE.md`: prior art is looked at — numeric libraries, algorithms and
optimization models, other engines — then thought through, measured and
bettered, never copied): **one mechanism per job** — where the review
found the same work done in several places, they are merged first and
the speed-up is made once, in the merged one.  The work, in this order:

- **W0 — concurrent writes of a column table** ✅ (a correctness bug the
  review found: two transactions writing overlapping keys to a column
  table both committed, the second's rows lost behind an overlapping
  segment — the directory is keyed by a segment's last key, so the two
  segments shared no key to conflict on).  A merge now reads the
  directory validated from its first incoming key through the first
  entry past its last, and the delta store's keys in that range; a
  statement storing rows in the delta store reads the directory the same
  way.  The later of two overlapping writers fails with `40001`, as in a
  row table; small writes to different keys never conflict; two writers
  appending past the last segment at once may (the claim runs to the
  directory's end; W3 left it so — an exact claim needs the batch's last
  key, W3b's).  Tested for every
  pair of large and small writes, overlapping or not, in either order.
  A second one, found while building W2: a column-table statement that
  failed part way (a unique violation on a secondary index during an
  `UPDATE`) left its gathered rows in the thread's statement buffer — the
  table was registered for the statement's clean-up only after a
  successful write —, and the next statement on that thread, even in
  another database, stored them past the unique checks.  A table is now
  registered when its first row is gathered; tested.
- **W1 — quick, safe gains** ✅.  `ANALYZE`: Li's Algorithm L from
  SplitMix64 seeded once, instead of an OS entropy call per row (55 % of
  its time) — `lineitem`'s 11.5 → 5.5 s, PostgreSQL's 5.3; a chi-squared
  test holds the sample uniform and the draws near k·ln(N/k).  Row
  decoding stops after the last wanted column.  The write set's `fresh`
  test before its lookup.  The checkpoint after 1 GiB of WAL, not 64 MiB
  (PostgreSQL's `max_wal_size`; 64 MiB on 32-bit targets).  D17's sync
  policy: `ordered` by default (`F_BARRIERFSYNC` on macOS), `full` by
  `--sync full`.  64 KiB read and write buffers on both front ends and the
  Rust client; the JS driver joins a frame's chunks once, not at every
  chunk (quadratic in a large frame).  Measured: the TPC-H SF 1 load on
  row tables 79.4 → 69.8 s (`ANALYZE` the most, `COPY` 27.6 → 26.2 s —
  the syncs inside it 16 % → 6.5 % of its profile, the rest being real
  writes, W4's).  Moved: decoding only the sampled rows needs a sampling
  method of the table storage (W3, then W3b); requesting the next result batch
  ahead helps only a stream with an explicit fetch size — a plain query
  and a columnar fetch never pause —, so it goes with the drivers' work
  (W8).
- **W2 — one sorted bulk path** ✅.  `CREATE INDEX`, `ALTER … TYPE` and
  key rewrites build their indexes through one builder,
  `dml::IndexBuild`: `CREATE INDEX` reads only the index's columns, in up
  to 1024 parts scanned a round at a time on the statement's workers, and
  computes only that index's keys; the entries are sorted, a unique
  index's duplicates found as neighbours, and written in key order into
  the `fresh` range, which the write set takes as appends and
  `apply_sorted` packs to 7/8 (PostgreSQL's `nbtsort`); internal nodes
  are now packed as leaves are (they were split in halves and stayed half
  full).  The sort, step by step, each measured on `lineitem`'s index on
  `l_suppkey`: the sorted build alone took the load 69.8 → 60.9 s; the
  parallel scan and parallel in-memory sort → 51.3 s (47.1 with the
  scratch disk cleaned); the external merge had ~90 runs (one per sort
  thread per spill), entries through `Value` encoding and a key copy per
  heap step — now one run of byte records per spill
  (`spill::Run::push_record`, the existing scratch runs) and a heap of run
  positions, the merge 1.7 → 0.5 s, the index 7.85 → 6.1 s; entries in one
  buffer with abbreviated keys (PostgreSQL's tuplesort, DuckDB's
  normalized-key sort: the key's first eight bytes as a number, most
  comparisons ending there; no allocation per entry) → 5.4 s; an index
  key in one allocation, not three.  The TPC-H SF 1 row load 69.8 →
  45.6 s, its indexes 15.8 s (PostgreSQL's 5.5).  What is left of the
  index on `l_suppkey` (5.4 s): ~2.9 s scanning, most of it page reads
  past the buffer pool (W5's read-ahead), and ~2.3 s writing key by key
  through the write set and the WAL (W4, D16).  `exec::index_entry`
  (unused) removed.  Moved: the per-row path of `INSERT` / `COPY` (a row,
  its index entries, the unique checks) is still its own — it becomes one
  with W3's row builder.
- **W3 — one row builder** ✅.  `COPY` and `INSERT` build rows through
  one bound builder, `dml::RowBuilder` (the target columns, defaults,
  checks and policies bound once; it was two paths).  Text fields parse
  straight to their column's type (`Value::from_text`: numbers, decimals,
  dates, timestamps without a string per field; anything else as the cast
  from text it was), a record's fields in one buffer, its UTF-8 checked
  once, and a record in column order built as it is read.  Parsing in
  parallel (DuckDB's parallel CSV reader, Instant Loading): `COPY` takes
  4 MiB of records, parses and builds them in parts of 2048 on the
  statement's workers when its defaults, checks and policies are safe off
  its thread, a row table's rows encoded there too
  (`TableStorage::encode_new` / `insert_encoded`); they are stored in
  order on the statement's thread, so errors name the line a serial load
  would.  One statement context (`storage::TableWrites`) instead of the
  column store's two thread-locals.  A column table's existence checks:
  the plan's "statement view" was first built in SQL (a key past which
  the statement knew nothing was stored: `lineitem` 22.4 → 15.3 s, 12.9
  with the columns encoded in parallel), then
  replaced, as planned, by the KV's own: `Transaction::first` remembers
  up to 8 ranges it found the store empty in (none of its writes there)
  until its intents next enter the store, and answers looks inside them
  from its write set — one mechanism, which every `first` caller gets,
  correct by construction (a random model test against a scan, with
  spills and others' commits, and a test that fails if a spill does not
  forget).  It cost 12.9 → 15 s at first — per row two lookups in the
  statement's context, a search of the gathered rows, seven allocations —
  and was made cheap: the keys built in one buffer (`Table::index_end`
  for an index's end), one look at the context per write, the gathered
  rows' last key compared before a search → 12.7 s.  A segment's columns
  encoded on threads of their own.  Measured, TPC-H SF 1: `lineitem`'s
  `COPY` into a row table 17.5 → 10.4 s, a column table 36.5 → 12.8 s
  (PostgreSQL 13.4 s); the whole row load 45.6 → 37.9 s, its `COPY`s 23.0
  → 14.5 s (PostgreSQL 18.3 s); the column load 36.4 s.  What is left of
  the column `COPY` (profiled): storing segments on the statement's
  thread while the parse waits (~3.7 s, ~1 s of it LZ and the write set),
  the parse waiting while rows are stored (~1.4 s), a row cloned into the
  gathered rows (~0.5 s).  Moved, as **W3b** after W4 (their write path
  is D16's): `TableStorage::insert_batch` — rows handed over owned, a
  column table's segments built from typed vectors with the kernels'
  minima and maxima (three row-to-column builders, four min / max
  computations made one), row ids reserved per batch, foreign keys
  deduplicated per chunk, partitions routed by their bounds without
  cloning each per row —, `ncp`'s binary batches decoded straight into
  vectors, parsing overlapped with storing (the parse reads through the
  writer's shared transaction today, so they take turns), and the
  storage's sampling method for `ANALYZE` (from W1).
- **W4 — durability with one owner** ✅.  One checkpoint trigger (today in
  spill and in commit, both under the commit lock), run in the
  background; WAL segments removed with one directory sync; no data sync
  at a segment's rollover; ordering-only syncs where ordering is all that
  is needed; then a checkpoint off the writer lock (copy-on-write epochs).
  Then D16: bulk loads into fresh ranges without the WAL, written as final
  versions, not intents a vacuum rewrites.
  *Done — the checkpoint* (profiled first: in `lineitem`'s row `COPY`,
  10.7 s, the statement's thread spent 0.41 s in checkpoints under the
  commit lock and 0.22 s in segment rollovers' syncs; in `CREATE INDEX` no
  checkpoint fell due).  A checkpoint in two halves: the pager's commit
  split into `freeze` — the state as it is taken as the next to commit,
  its pages no longer writable (writers copy them as they copy durable
  ones; LMDB's and the copy-on-write B-trees' rule, applied per epoch)
  but still written back, its free list's pages taken out of the free
  set — and `commit_frozen`, which writes free list and header without
  the lock; the store's `checkpoint_begin` freezes under the writer lock
  and returns, `finish` syncs the WAL, writes back the frozen pages and
  commits while writes go on.  One trigger, after every write to the
  store, waking a checkpointer thread (inline in the simulation, or when
  the WAL passes twice the limit).  WAL segments before a checkpoint
  deleted with one directory sync, `open` deleting again any a crash left
  (a log left with no segment starts at the checkpoint's LSN — a case the
  new crash test found: a segment created after the freeze at exactly the
  frozen LSN lost its entry).  Ordering-only syncs (`File::sync_order`, a
  barrier on macOS) for a segment's rollover and for a commit's pages
  before its header — they matter under `--sync full`; the default was a
  barrier already.  The rollover sync itself stays: it lets `open` tell a
  torn tail from corruption (PostgreSQL syncs a finished segment too).
  Tested: a crash at every I/O point of batches written between a
  checkpoint's halves (both crash modes, 8 seeds), writers and a
  checkpointer thread on real files; mutations caught — no write-back of
  a frozen page freed, no flush of frozen pages, frozen pages left
  writable, free-list pages handed to writers, no deletion of left-over
  segments, an empty log started at 0.  Measured: the TPC-H SF 1 row load
  37.9 → 35.3 s (`lineitem`'s `COPY` 10.4 → 9.6 s).
  *Done — D16* (PostgreSQL's `wal_level = minimal`, where a table or
  index made in the transaction is synced at its commit instead of
  logged, and `COPY FREEZE`, rows written frozen; CockroachDB's
  `AddSSTable`, ingested files instead of logged writes).  Taken: no WAL
  for a range nobody else sees, made durable by a sync before the commit
  record; the rows final at once.  Made nucleus's: the "sync" is the
  checkpoint (copy-on-write: what is not in the WAL is in the database
  file once a checkpoint after it is, and nothing else need be flushed
  by hand); the range is marked in a logged record, so recovery knows
  what a crash left and deletes it — PostgreSQL's equivalent drops the
  relation file, which nucleus does not have.  A value written into a
  range declared fresh (`Transaction::fresh`), above the highest key the
  transaction wrote there, goes into the store as a version at the
  transaction's snapshot, in 1 MiB batches (`Store::write_unlogged`); the
  first write registers the range under the commit lock — declined when
  the store holds any entry in it (a commit after the snapshot, an
  intent) or another transaction writes an overlapping one so — and logs
  its mark (`00 04 ‖ start → end`); others' commits and spills into it
  conflict; the transaction's reads write the batch first; the commit
  checkpoints, then logs its record, which deletes the marks; a rollback,
  or recovery for a mark left, deletes the range.  `EngineOptions` /
  `Options::wal_minimal`, the server's `--wal minimal|full`.  Tested
  (`tests/unlogged.rs`): a crash at every I/O point of loads past the
  cache, both modes; rollback, drop and crash before the commit; the
  stale snapshot and the overlapping range declined; a model test of the
  transaction's own reads and writes; 14 mutations caught.  Measured,
  TPC-H SF 1, the same binary: the indexes 17.0 s with `--wal full`, 13.5
  and 13.9 s minimal (`l_suppkey`'s 6.8 → 5.2 s), the whole row load 38.2
  → 34.0 and 35.6 s — the `COPY`s and `ANALYZE` are untouched and vary
  by a second between runs.  Not covered: a `COPY` into an empty table
  made in an earlier transaction — not fresh (others see the table);
  W3b's batches go through the same path where a range is.
- **W3b — batches into storage** ✅ (moved from W3, above, onto D16's write
  path): `insert_batch`, segments from typed vectors, `ncp` batches into
  vectors, parse and store overlapped, the sampling method.
  *Done.*  One batch path, `dml::insert_batch`, for `INSERT`, `COPY` and
  an `UPDATE`'s partition moves (it was a row at a time through
  `insert_row` / `insert_routed`): rows handed over owned, partitions
  routed by their bounds (`partition::route`, no table cloned per row),
  row ids reserved per batch (`catalog::next_rowids`), index entries and
  foreign keys computed before the rows are handed on, a foreign key's
  keys deduplicated by their encoding and checked once each; the first
  error in row order wins, and a statement failing on its data skips the
  foreign-key checks (PostgreSQL's order: the bad record is reported, not
  a missing parent).  `TableStorage::insert_batch` (default: row by row);
  a column table gathers an ascending batch at once (`BTreeMap::append`)
  when nothing stored lies between its first and last key — the gathered
  rows, the directory's segment there, the delta store —, else row by row.
  Segments from typed vectors (Arrow's layout; DuckDB's and ClickHouse's
  column writers): rows moved, not cloned, into columns transposed in
  parts on threads; `Vector::typed` the one row-to-column builder;
  integer minima and maxima from the kernels; dictionary or plain text
  decided by a distinct count that stops past n/4 — byte for byte the old
  encoder's output (kept in the tests as a reference, 400 random columns
  compared).  Parse and store overlapped (DuckDB's parallel CSV reader,
  Instant Loading's pipelining): a parser thread parses on the workers
  over the writer's transaction behind a lock (`nucleus_kv::Locked`, a
  `KvRead`), while the statement's thread stores what was parsed — the
  chunk queue bounded, the parsed queue drained before each read so the
  two never wait on each other.  The sampling method
  (`TableStorage::sample`, from W1): PostgreSQL's `ANALYZE` reads every
  block but forms only the sampled tuples; here Li's Algorithm L tells
  which rows the sample takes, and a row table decodes only those
  (`stats::Sample::offer` takes a closure), a column table decodes a
  segment's columns only when the sample takes one of its rows, and builds
  only the rows taken.  `ncp`'s binary batches: decoding them straight
  into vectors not taken — `insert_batch` takes rows (checks, policies,
  index entries and foreign keys are per row), and a vector path beside
  it would be a second insert path; they are built on the parser thread,
  so they are overlapped with storing as text is.  Found by the new tests:
  enum values read from a column segment lost their labels and type (the
  segment cache keeps stored values; the reading transaction's catalog now
  labels them, `Labels::fill`).  Tested: workers 1 and 4 load the same
  rows and fail at the same line into row, column, descending and
  partitioned tables, an enum made in the same transaction; a batch's
  first bad row; a bad record before a missing parent; batches meeting
  stored rows (gathered, delta, segments, a key twice in a batch); both
  samples against the rows a scan gives; mutations caught (the dictionary
  boundary, the point read's labels, the ascending test, the
  foreign-key skip, a segment row's index).  Measured, TPC-H SF 1,
  alternating with the previous binary: the row load 36.9 / 31.9 →
  30.4 / 29.0 s (`ANALYZE` 6.7 / 6.4 → 3.5 / 3.9 s); the column load
  33.8 / 34.3 → 26.2 / 25.7 s (`lineitem`'s `COPY` 12.2 → 7.8–8.1 s,
  `ANALYZE` 5.7 → 2.9 s) — PostgreSQL's 29.1 s.  What is left of the
  column `COPY`: LZ compression and the puts on the statement's thread;
  of the row `COPY`: the write set's spills and page evictions (W5).
- **W5 — reads, and reads after a load.**  Each spilled transaction's key
  span kept, so reads outside it skip the intent path (today every read
  takes it until a vacuum); read-ahead and coalesced reads; the pager's
  checks without its mutex; a smaller `Value`; decimals cached as 64-bit
  mantissas, a scan-resistant segment cache; one copy of a written byte
  where there are four or five; CRC-32C interleaved.
  *Done — page reads* (profiled first, TPC-H SF 1 on row tables: in Q1
  13.8 % of the busy time waited on the pager's mutex, held only to check
  a page id, and 17.3 % was `pread`, a page at a time from the OS cache).
  The checks against the page count and the poisoned flag mirrored in
  atomics (the count only grows); then `pread` 22.7 %.  Coalesced reads
  (PostgreSQL 17's read stream, `io_combine_limit` = 128 kB): a cursor
  moving to a leaf not cached reads the parent's next leaves with it, up
  to 16 pages in one read into frames the clock takes first if unused —
  at first a fixed 16, which sped the scans (Q1 −20 %, Q6 −30 %) but
  slowed index lookups whose entries crossed a leaf's end (Q17 2×, Q21
  +20 %), so the window ramps up with the leaves moved through since the
  last seek, 1, 2, 4 … 16 (Linux's read-ahead): `pread` 9.8 %, no query
  slower.  CRC-32C with three interleaved streams joined by table shifts
  (Mark Adler's `crc32c.c`; Intel, "Fast CRC Computation for iSCSI
  Polynomial Using CRC32 Instruction"): an 8 KiB page 934 → 353 ns.
  Tested: a run read once, a page dirty in the pool kept, nothing read for
  ids not consecutive or past the end, a pool smaller than the run; short
  ranges on a cold pool read no leaves ahead, a full scan's leaves arrive
  read ahead; CRC lengths around the blocks; mutations caught.  Measured,
  the 22 queries on row tables, alternating with W3b's binary: 10.0 →
  8.9 s (column tables unchanged, 4.6 s: their segments come from the
  segment cache).  Not taken: `preadv` into the frames (a second `unsafe`
  call in the platform crate, for a copy of 8 KiB per page); an
  asynchronous read-ahead (the scan's workers read in parallel already).
  *Done — reads past intents* (measured first: `lineitem` and `orders`
  each loaded by a spilled `COPY` into a table made before it, then read
  before and after a vacuum: Q6 382 → 336 ms, Q12 432 → 383 ms; and every
  read in the database merged intents while any were unresolved — a query
  of `orders`, loaded and vacuumed before `lineitem`, 70 → 67 ms the same
  way).  A spilled transaction's intents' keys kept as ranges, running and
  committed until vacuum, and only the reads they reach take the merging
  path — scans by their range, a key's read and a commit's conflict check
  by the key.  One range from the least key to the greatest (as the
  serializable check keeps) covered every table between a load's catalog
  key and its rows, so `orders` gained nothing; up to eight disjoint
  ranges, the neighbours whose facing keys share the longest prefix joined
  past eight (a greedy agglomerative merge by the keys' own similarity:
  the engine knows nothing of tables), and `orders` read 51 ms beside 50
  vacuumed.  The merging pass decoded each version's key into a buffer of
  its own and copied key and value twice; now a key is decoded into the
  batch once and its value replaced in place by a better ranked version's
  (the plain path's way): Q6 over the intents 385 → 330 ms (301 ms
  vacuumed).  The ranges are not in the status record (a format change for
  a window that lasts until the next idle vacuum): after a restart every
  read counts the intents, as before.  Tested: scans from and to each
  range's edges, keys at the edges, an older snapshot, a writer inside a
  running range conflicting at its write and at its commit and one outside
  committing, the same after a restart; the ranges kept apart and within
  eight; mutations caught (each edge's comparison, the commit's look, the
  merging pass's buffers).
  *Done — a smaller `Value`*: 64 → 32 bytes.  It was 64 because two
  variants held 128-bit integers (a decimal's mantissa, an address), whose
  16-byte alignment rounded everything up, and an enum's value took 48;
  now the two are `repr(C, packed(8))`, the 128-bit field first (24
  bytes; a field is read by copy, never by reference) and an enum's value sits
  behind an `Arc` (`Labels::fill` changes it with `Arc::make_mut`).  A
  compile-time assertion keeps it at 32.  Measured, the 22 queries,
  alternating with the step before: row tables 8.90 → 8.37 s (Q9 1243 →
  1082 ms, Q20 526 → 448), column tables 4.58 → 4.31 s.  Not taken: a
  `Box` for decimals (an allocation per value in the aggregates TPC-H
  lives in).
  *Done — decimals as 64-bit mantissas*: a decimal vector's mantissas
  (`vector::Mantissas`) are 64-bit while each fits — as a segment stores
  them, so a cached column takes half the memory (`lineitem`'s four
  decimal columns 384 → 192 MB beside the 256 MB cache) — else 128-bit (a
  product's, a sum's); the kernels read either through `DecRows`, a
  narrow column (and an integer column, which was copied widened) without
  a copy, and raising by 1 no longer copies the column.  Measured, the 22
  queries, alternating: column tables 4.33 → 4.25 s (Q6 77 → 67 ms, Q18
  303 → 277 ms); row tables within the noise (8.63 → 8.50 s).
  *Not taken — a scan-resistant segment cache.*  The cache evicts the
  columns used longest ago, an eighth of its bound at a time; a scan of a
  table larger than it keeps nothing from one run to the next.  Tried:
  TinyLFU's admission (Einziger, Friedman and Manes 2017; Caffeine) — a
  count-min sketch of how often each column is asked for, and a full
  cache letting a column in only over a less asked-for victim.  In a
  model it did what it should (a 150-column scan through 100 slots, ten
  times: 0 hits → 900+; twenty hot columns between a scan's: 0 → 500+),
  but on TPC-H SF 1's column tables it lost: 4.20 → 6.72 s, the hot
  `lineitem` columns' counts saturating and keeping a small table's
  columns (Q11, Q16) out; aging every 10 000 counts (Caffeine's ten times
  the entries) still 4.76 s.  The 22 queries' columns nearly fit the 256
  MB, so least-recently-used is what they want.  Left as it is; the
  question comes back with a working set past the cache (SF 10, or the
  §M14.20 closing runs) — then W-TinyLFU's recency window (1 % in plain
  LRU before the admission test) or S3-FIFO (Yang et al., SOSP 2023: a
  small FIFO, a main one, a ghost list), measured the same way.
  *Not taken — one copy of a written byte.*  Profiled first (`lineitem`'s
  `COPY` into a row table, 9.1 s): `memmove` is 4.7 % of the busy time,
  and of it the write path's own copies — into the write set, the
  version's encoding, the store's batch, the WAL record, the leaf — about
  1.7 %; the rest is parsing and encoding the rows, which must be done.
  Not worth reshaping the write set's representation for; the WAL record
  and the tree's application sharing one buffer stays W7's.
  *W5 done* (2026-11-03): page reads, reads past intents, a 32-byte
  `Value`, 64-bit decimal mantissas; the 22 TPC-H SF 1 queries on row
  tables 10.0 → 8.5 s, on column tables 4.6 → 4.25 s.  **W6 is next**;
  each step measured with `bench/phases.py` against the commit before it
  (`docs/benchmarks.md`, "How these steps are measured").
- **W6 — the queries above 3×.**  `count(DISTINCT)` planned as a
  grouping (the vectorized, parallel one); top-N; typed aggregate states
  and one key table for groupings, `DISTINCT`, set operations and join
  lookups (four or five maps today; groupings and joins done), one hasher, hashes a column at a
  time, dictionary codes as group keys; sorts on normalized keys with one
  comparator (done); Q19's per-row directory lookups (the segment found kept for the next
  key: W3's remembered ranges serve only keys not stored);
  expressions materializing only their columns, `CASE` vectorized (done);
  joins emitting batches (left from V2: done, joins inside pipelines).
  *Done — groupings past `work_mem`, on the workers* (profiled first:
  ClickBench Q32, `GROUP BY WatchID, ClientIP` — a million groups — on a
  column table, 2.5 s against PostgreSQL's 0.93 s; with `work_mem` at 2
  GB 0.52 s, Q18 0.98 → 0.21 s, Q39 1.15 → 0.24 s.  The parallel
  aggregate ran its morsels and merge parts to the end, found the merged
  groups past `work_mem` and threw it all away; one thread then aggregated
  again, its new groups' rows into partitions, each aggregated a row at a
  time — and the morsels, each with a `work_mem` of its own, had held
  more than `work_mem` together all along).  Now one bound for the whole
  aggregate (DuckDB's radix-partitioned aggregation, Kuiper et al.,
  "Robust External Hash Aggregation in the Solid State Age", ICDE 2024;
  ClickHouse's two-level tables gave the 64 parts): past `work_mem`, a
  running morsel holding at least its fair share writes its groups'
  partial states to scratch space, by merge part (one run a morsel, each
  part's byte range kept: `Run::at`, `Sealed::range`; runs readable from
  any thread, `Arc`), and goes on with an empty table; a morsel done keeps
  its groups in memory while those kept fit in half of `work_mem`; each
  merge part reads a morsel's spilled groups back before those it kept,
  so places and errors are a merge in memory's; merged rows past half of
  `work_mem` come a part at a time (each part's in the order first seen,
  so the same whatever the morsels), those that did not fit from scratch.
  Not taken: PostgreSQL's way, the input rows partitioned (it is what the
  single pass does, and it stays for aggregates without partial states
  and for one thread); a fixed share per running morsel (`work_mem` /
  2·workers: Q36's long URLs spilled though the whole fitted, 86 → 116
  ms).  Tested: many groups past `work_mem` give a single pass's rows and
  error and go to scratch on the workers; two and four workers give the
  same order; few groups met by every morsel, spilled, come in a single
  pass's order; the 43 ClickBench queries' results the previous build's
  at 64 MB and 4 MB but for ties at a `LIMIT`; mutations caught (spilled
  groups lost, no flush).  Not caught by a test: a morsel's spilled
  groups read after those it kept (in the simulation's tables no morsel
  both spills while running and keeps the rest).  Measured, alternating
  with W5's binary, two rounds: ClickBench on a column table, the sum of
  the 43 queries 9.06 / 9.03 → 5.13 / 5.54 s (geometric mean 38.2 → 34
  ms): Q32 2,545 → 623–695 ms, Q18 948 → 240, Q39 1,095 → 330, Q33 429 →
  194–383, Q34 514 → 216–371; Q36 86 → 101–103 ms.  TPC-H unchanged in
  sum (row tables 8.63 → 8.60 / 8.73 s, column 4.19 → 4.15 / 4.22 s; Q20
  on rows 533 → 462–492 ms) but Q13, 147 → 179 ms on rows, 87 → 108 on
  columns.  Q13 and Q36 are the bound's price: their groups recur in
  every morsel (`c_custkey` over `orders`, a URL), so the morsels'
  tables together pass `work_mem` though the merged groups fit — with
  `work_mem` at 2 GB both are as before.  Next for them: one table a
  worker across its morsels (DuckDB's thread-local tables), which needs
  the merge to take a group's least rank rather than its first.
  *Done — top-N*: a sort right below a `LIMIT` keeps only the rows the
  limit takes and skips (`spill::Top`, PostgreSQL's bounded sort,
  `tuplesort_set_bound`): the pairs gathered up to twice that many — fewer
  are sorted as they are —, then the least become a max-heap by (keys,
  the order given), a row past its greatest turned away by one
  comparison; so the rows are those of the full sort, ties in the input's
  order.  A parallel sort's morsels keep a heap each and build a row only
  when its keys enter it (Q24's `SELECT SearchPhrase … ORDER BY
  EventTime LIMIT 10` built every row of the scan).  Past `work_mem` the
  heap gives way to the full external sort, with the rows it kept in the
  order given.  The bound reaches the sort through the context (`Ctx::
  bounds`, by the sort's address), so the sort node is built and
  measured as before.  Not taken: a separate top-N operator (DuckDB's
  `PhysicalTopN`) — one sort, bounded or not; a heap from the first row
  (Q41, `LIMIT 10 OFFSET 1000` over about as many groups, 8.3 → 9.5 ms,
  hence PostgreSQL's 2k); the heap's greatest pushed into the scan as a
  filter on the column segments' least and greatest values (DuckDB's
  top-N dynamic filters, 2024) — left for the scans' own step.  Tested:
  `ORDER BY … LIMIT n OFFSET m` gives rows m.. of the full sort and its
  error, on row and column tables, a row or a batch at a time, on one
  thread and on workers, past `work_mem` (the fall-back); mutations caught
  (a tie admitted, the heap's tie order, the fall-back's rows dropped,
  the gathered pairs' cut, the bound without the offset).  Measured,
  alternating with the step before, two rounds: ClickBench on a column
  table, the 43 queries 5.21 / 5.10 → 4.71 / 4.73 s (geometric mean 33.3
  → 30.1 ms): Q32 610 → 525 ms, Q18 241 → 169, Q39 325 → 261, Q33 192 →
  140, Q34 201 → 155, Q24 11.2 → 5.4, Q25 12.8 → 5.9, Q26 13.3 → 6.5.
  TPC-H unchanged (row tables 8.8 → 8.6 s, column 4.2 s; Q20 on rows,
  best of five 434–457 → 481–527 ms, is the same in the best of forty,
  445 → 443 ms, and its profile is the same: reads of pages).
  *Done — a decimal division in one step*: a quotient whose dividend,
  scaled up by the digits asked for and one more, fits 128 bits (an
  average's sum and count do) is one hardware division, rounded on that
  last digit — the digits the long division made one at a time, a 128-bit
  division each (Q32's million averages: 5 % of its profile).  The long
  division stays for the rest (PostgreSQL's `div_var` is one too, in
  base 10,000; DuckDB and Arrow's 128-bit decimals scale and divide once,
  and so does this, but a dividend that does not fit goes to the long
  division instead of an error).  On the way: the long division failed a
  quotient that fits when the divisor, scaled up, did not (a tiny
  dividend over a huge divisor: 0.000, not an overflow).  Tested: 200,000
  random quotients — mantissas of every length, scales either way, both
  signs — are the long division's; mutations caught (no rounding digit,
  rounding at 4, the sign).  Measured, alternating with top-N, two
  rounds: ClickBench on a column table 4.85 / 4.87 → 4.75 / 4.70 s
  (geometric mean 30.7 → 29.8 ms; Q32 555 / 524 → 513 / 503 ms, Q33 159
  / 167 → 146 / 144); TPC-H unchanged (row tables 8.78 → 8.75 / 8.60 s,
  column 4.18–4.30 → 4.22–4.34 s; Q1, Q9 and Q18 the same in the best of
  thirty).
  *Done — typed aggregate states*: a grouping's built-in states were an
  `Acc` a group and aggregate, 144 bytes whatever the aggregate (Q32's
  million groups: allocating and reading them, 4.9k of its 38k samples),
  counted against `work_mem` as a guessed 96.  Now each aggregate keeps a
  column of the state it needs (`builtin::States`): `min`, `max`,
  `bool_and`, `bool_or` a value (32 bytes; any type, one mechanism), a
  `sum` or `avg` of integers or of decimals at one scale an exact 128-bit
  sum and a row count (24), chosen by its first rows or partial state;
  anything else — doubles, intervals, `string_agg`, `array_agg` — and a
  sum that meets another scale or kind turns into `Acc`s, made from the
  typed state, so results are the same whichever.  An `avg`'s partial
  state is its rows and sum (it was the six-part `Acc` state); a sum's
  and a value's as before.  The accumulator says what a group takes
  (`GroupAccumulator::group_bytes`, ClickHouse's `sizeOfData`; by default
  96), and a grouping counts that against `work_mem` — so a `count` (8
  bytes) no longer spills as if it held 96.  Taken from DataFusion's
  `GroupsAccumulator` (2023: a vector per state across groups — the shape
  `GroupAccumulator` already had, now with its states typed) and
  ClickHouse's state sizes.  Not taken: DuckDB's states in the hash
  table's row beside the key (one allocation, the key's cache line) —
  that is the one key table's step; 64-bit sums with an overflow check
  (DuckDB sums into 128 bits too, and the exact sum is what makes a sum
  overflow only when its result does); integer least and greatest values
  as `i64`s beside a bit (a value serves every type; Q6's
  `min(EventDate)` already 14 → 6 ms, no row's arguments built as a
  vector any more).  Tested: sums and means of integers and decimals,
  least and greatest values of each kind, on row and column tables,
  grouped by few and many keys, a row at a time, by batches of 3 and of
  1,024, on workers and past `work_mem` — and one aggregate fed integers
  then decimals, decimals at two scales, decimals then values, integers
  then a batch a row at a time; mutations caught (the integer sum lost
  turning into `Acc`s, a mean's rows miscounted when combined or in its
  partial state, a bigint sum's overflow dropped, a sum of another scale
  taken as its state's, a least value's first row lost).  The few-groups
  spill test's `work_mem` 1 MB → 256 kB (its counts now 8 bytes).
  Measured, alternating with the step before, two rounds: ClickBench on a
  column table 4.79 / 4.79 → 4.64 / 4.76 s (geometric mean 30.3 → 29.5
  ms): Q32 522 / 540 → 438 / 451 ms, Q6 14.3 → 6.2, Q36 80 / 83 → 70 / 73,
  Q28 240 / 234 → 218 / 222, Q18 170 / 173 → 164 / 158 (Q33, Q34, Q39
  move by a fifth from run to run on either build); slower, Q27 36 → 39
  ms in every round — not its aggregate (its updates' samples 780 → 688)
  but `length(URL)`'s string a row, more of it in the allocator's tiny
  zone: left for expressions reading text in place.  TPC-H on column
  tables 4.42 / 4.25 → 4.27 / 4.13 s (Q18 287 / 274 → 253 / 243 ms, Q13
  112 → 103 / 101, Q1 166 → 158 / 148); on row tables unchanged (8.92 /
  9.05 → 9.16 / 8.86 s, the pread-bound queries' noise).
  *Done — `DISTINCT` inside aggregates as groupings*: a `DISTINCT` call
  kept a set of values a group, so its whole aggregate ran a row at a
  time on one thread (ClickBench Q4, `count(DISTINCT UserID)`, 558 ms:
  2.9× PostgreSQL's; Q5, Q8, Q9 each 0.55–0.73 s).  Now the optimizer
  (`distinct::two_level`) makes it two groupings — by the group and the
  `DISTINCT` arguments, the other aggregates giving their partial states
  (an aggregate `partial f` whose result is `f`'s `GroupAccumulator::
  partial`), then by the group, the `DISTINCT` call fed each value once
  and the others combining (`combined f`) — both the vectorized,
  parallel grouping within `work_mem`.  It holds when the `DISTINCT`
  calls share their arguments, no call has an `ORDER BY`, every
  aggregate gives the same result in any order of its rows (a new
  capability, `Aggregate::order_free`: not `array_agg`, `string_agg`)
  and has partial states, and the input is a pipeline.  Taken from
  Calcite's `AggregateExpandDistinctAggregatesRule` and Spark's
  `RewriteDistinctAggregates` (the two groupings), with the engine's own
  partial states in place of their SQL ones (a sum of sums, an average
  as a sum and a count: other types, and a partial sum that overflows
  where the whole does not).  Not taken: DuckDB's distinct hash tables
  inside the aggregate operator (one mechanism: the grouping as it is);
  PostgreSQL's sort of each `DISTINCT` call's rows; ClickHouse's
  `uniqExact` — a set a group as the state, what this was; Spark's
  `Expand` for calls of other arguments (each row once a set of them) —
  those stay a set a group; over a join's rows, still a row at a time,
  the two groupings only hash each row twice (TPC-H Q16 127 → 192 ms,
  so not there until joins emit batches).  Tested: the rows and errors of
  the same calls with an `ORDER BY` inside (a set a group, as before) —
  grouped and not, `NULL`s, equal numerics of other scales, no rows,
  `HAVING`, a correlated subquery, an error in the `DISTINCT` argument
  and in a sum beside it — a row or a batch at a time, on workers, past
  `work_mem`; calls of other arguments, `array_agg` beside, a join's
  rows: one grouping; the 43 ClickBench queries' results the previous
  build's at 64 MB and 4 MB but for ties at a `LIMIT`; mutations caught
  (the `DISTINCT` call's columns, the partial states' columns, the
  arguments' or the order's condition dropped, a partial finished as a
  result, the pipeline's condition dropped).  Measured, alternating with
  the step before, two rounds: ClickBench on a column table 5.01 / 4.82
  → 2.45 / 2.41 s (geometric mean 31.1 → 20.0 ms): Q4 547 → 14 ms, Q5
  549 → 12.5, Q8 607 → 24, Q9 728 → 74, Q10 22 → 5.5, Q11 23 → 6, Q13
  85 → 22, Q22 12.7 → 7.0 — Q4 now 0.07× PostgreSQL's, and no ClickBench
  query on a column table above 3×.  TPC-H unchanged (its one `DISTINCT`
  call, Q16's, is over a join: 128 ms on both).
  *Done — shared reads without a queue* (profiled first: TPC-H Q19 on
  column tables, 65 ms against PostgreSQL's 17.8, 3.1×; its 477 parts
  each look up some 30 `lineitem` rows by `lineitem_part_supp`, the
  lookups on the workers).  Not what the plan above guessed — the
  segments a part's rows lie in are scattered, so a segment kept for the
  next key would not serve —, but the workers queueing: each lookup is
  three reads (the delta store, the segment directory, the row's delete
  marker), and every read of a just loaded table took the engine's
  mutex over the committed transactions with intents (`unresolved`,
  16k of the profile's samples); the directory's `first` went through a
  whole scan (`KvRead` had none) with its read-ahead; and every read
  locked the transaction's pending unlogged writes.  Now the statuses
  and the running ranges are under a reader-writer lock; `KvRead::first`
  reads one entry (`Transaction::first_shared`: no read-ahead, and no
  empty range remembered — the workers would contend for that memory);
  a read flushes unlogged writes only in a transaction that has a fresh
  range (`flush_unlogged`).  Found on the way: `EXPLAIN ANALYZE` read
  every column (it never set the columns needed), so Q19 under it took
  18.6 s — every `lineitem` column of every row looked up, past the
  segment cache —; now 145 ms.  Not taken: a statement's view of a
  column table (directory, markers, delta) kept for its lookups — the
  workers' threads outlive a statement, and the reads are cheap enough
  once they no longer queue; lock-free statuses (an epoch-swapped
  snapshot) — the reader-writer lock left no wait in the profile.
  Tested: a Q19-shaped join — a row table split into morsels, each row
  looking up a column table by a secondary index, rows deleted from the
  segments and some in the delta store — gives one thread's rows on four
  workers (no test before ran a column table's lookups on workers: the
  lookups' test had too few outer rows to split); mutations caught (the
  shared `first` finding nothing, in the engine's reads and in
  `SharedTxn`; unlogged writes never flushed).  Not covered by a test:
  `EXPLAIN ANALYZE`'s columns (no observable but time).  Measured,
  alternating with the step before, two rounds: TPC-H on column tables
  4.50 / 4.82 → 4.39 / 4.41 s, Q19 65 / 66 → 46 / 45 ms (2.5× now: no
  query on column tables above 3×), Q21 511 / 575 → 454 / 471; on row
  tables, best of twenty, Q19 43 → 40 ms, Q21 1,040 → 910 (best of
  three moves by a tenth either way); ClickBench unchanged (2.62 / 2.59
  → 2.62 / 2.61 s).
  *Done — correlated subqueries on the workers* (profiled first: TPC-H
  Q20 on column tables, 557 ms against PostgreSQL's 261, 2.1×: its
  condition `ps_availqty > (SELECT 0.5 * sum(l_quantity) …)` stays above
  the semi join with `part` and runs its subquery — an index scan of
  `lineitem_part_supp` — 8,876 times on the statement's thread, 550 of
  the 571 ms; 87% of that the column table's point reads).  Not
  decorrelated (Neumann and Kemper, "Unnesting Arbitrary Queries", as
  DuckDB and HyPer): a join with the subquery's rows grouped by `l_partkey,
  l_suppkey` computes groups no outer row asks for and evaluates the
  subquery's expressions over other rows in another order — an error
  (an overflowing `sum`, a division in its filter) where the statement
  had none, or another first one —, and nucleus's rewrites keep errors
  where a single pass meets them (eager aggregation takes only infallible
  aggregates for the same reason).  Taken instead: the same per-row
  evaluation on the workers, as PostgreSQL's planner allows a correlated
  `SubPlan` in a parallel plan when the parameters it reads are of the
  level right outside and its own plan, recursively, is parallel safe
  (pgsql-hackers, "parallelize queries containing subplans", 2017 —
  Robert Haas's point that the check must recurse into the subplan).
  `expr::ready_subqueries` readies a correlated subquery for the workers
  when the statement is a query (nothing it runs writes), no rows it
  inserted are held unstored on its thread, and the subquery reads only
  the row it runs for and nothing that differs by thread or run (no
  volatile or table function, no CTE, work table or catalog view, its own
  subqueries included); pipelines and an index nested loop's lookups then
  take such conditions; and a filter or projection over rows that are not
  a pipeline's (Q20's, a semi join's) runs them on the workers the way the
  lookups do — the lookups' block iterator made one for both
  (`exec::OnWorkers`: the input in blocks of 256 doubling to 16,384, each
  cut into pieces for the workers, the rows in the input's order, the
  first error where a single pass stops, none from rows a `LIMIT` never
  reaches).  On the way: the buffer pool's frames and shards each on a
  cache line of their own (128 bytes; they shared lines, three frames to
  one) and its hits counted in their shard (one counter all threads
  wrote): Q20 on ten workers 199 / 202 → 174 / 175 ms, best of ten.  Not
  taken (yet): optimistic latches for the tree's upper pages (LeanStore,
  Leis et al. 2018; Umbra) — the root's pin and its shard's lock are
  still written by every lookup's descent (some 10% of the workers'
  samples); the point reads themselves (next).  Tested: a filter and a
  projection whose subqueries run for each row — over a semi join's rows,
  over a pipeline's, nested, reading a CTE (kept on one thread) — give one
  thread's rows (row and column tables, four workers, and the row path),
  the same errors (in the subquery's rows, after it, and of two the first
  in row order), and none where a `LIMIT` stops before an erroring row;
  mutations caught (the pieces' rows out of order; correlated subqueries
  kept off the workers).  Measured, alternating with the step before, two
  rounds: TPC-H on column tables 4.35 / 4.32 → 3.89 / 3.95 s — Q20 577 →
  173 ms (0.66× PostgreSQL's), Q2 49 → 25, Q17 88 → 31, Q21 458 → 432 —;
  on row tables 8.76 / 8.52 → 8.64 / 8.52 s — Q20 509 → 175, Q2 41 → 26,
  Q17 43 → 19 (Q6's 315 → 363 was noise: best of ten 338 / 356 against
  320 / 335) —; ClickBench unchanged (2.49 / 2.46 → 2.52 / 2.45 s).  One
  loss: Q18 on column tables 257 → 290 ms (three rounds, best of twenty,
  either order), from no code it runs — a build with the subquery change
  but not `exec.rs`'s is as fast as before, one with `exec.rs`'s but its
  new paths never taken as slow —: `Expr::eval`'s instructions are the
  same, 36 bytes further on, and its per-row filter over Q18's 1.5
  million groups costs half again.  The row-at-a-time `Expr::eval` keeps a
  1.7 kB frame for every node (`#1 > 300` is three), which its largest
  arms size; the next step gives it a small one.
  *Done — a small frame for `Expr::eval`*: its common nodes (columns,
  literals, parameters, the operators, `IS NULL`) stay in it, the others
  go to `eval_other` (not inlined), and the error a missing parameter
  makes is built out of line — the frame 1,680 → 480 bytes a node.  The
  compilers' rule of outlining the cold paths of a hot function (LLVM's
  hot/cold splitting pass); not taken: PostgreSQL's
  `ExecInterpExpr` — an expression compiled to a flat array of steps run
  by one loop, no recursion — which a row-at-a-time path not meant to
  stay hot does not repay, now that the vectorized path takes most rows.
  Tested by the whole suite (no behaviour changes).  Measured,
  alternating with the step before, two rounds: TPC-H on column tables
  3.92 / 3.92 → 3.74 / 3.75 s (Q18 305 / 313 → 267 / 262 ms: the layout
  loss above gone, and below the 257 before it); on row tables 8.56 /
  8.20 → 8.05 / 7.98 s; ClickBench 2.47 / 2.47 → 2.41 / 2.37 s (Q32 427
  / 442 → 408 / 412 ms).
  *Done — a column table's point reads* (profiled first: Q20 on the
  workers, its 66,000 `lineitem` lookups 87% of the time on one thread —
  per lookup the segment directory's tree (1.3 µs), its entry decoded,
  every column's least and greatest values among it (0.4 µs), the row
  keys' binary search (1.45 µs: fifteen steps over 32,768 keys, each a
  miss or two), the delete marker's tree (0.6 µs), the delta store's
  (0.65 µs)).  A segment's id is its contents' hash, so its directory
  entry is decoded once and cached by id with its columns; and its row
  keys keep every 32nd key's first 8 bytes as a big-endian number
  (`Keys::marks`, 8 kB a segment): a search finds the run between two
  marks in that array — a mark below the probe's prefix is a key below
  it, one above a key above it, equal prefixes widen the run — then
  searches those keys only.  ClickHouse's sparse primary index (a mark
  every 8,192 rows over granules it reads whole) taken at a point read's
  scale; a B+-tree's inner level; not taken: an Eytzinger or B-tree
  layout of the keys themselves (Khuong and Morin, "Array Layouts for
  Comparison-Based Searching", 2017) — variable-length keys, and the
  marks leave a search two or three misses.  Not taken either, after
  measuring: the three trees each lookup still descends (directory,
  marker, delta: 1.3 + 0.6 + 0.65 µs of a lookup's 4.6) — a statement's
  snapshot of a column table's directory and of its markers' and delta's
  emptiness would spare them, but the storage interface gives a lookup a
  transaction, not the statement, and the trees' own reads are what the
  next W (W7, extents) changes.  Tested: the marks' search against a
  plain one — keys sharing their first 8 bytes in runs across marks,
  keys shorter than 8 bytes, probes between, before and after every key
  — and by the suite's lookups; mutations caught (a run's first key
  skipped, equal prefixes not widening).  Measured, alternating with the
  step before, two rounds: TPC-H on column tables 3.77 / 3.73 → 3.77 /
  3.74 s — Q19 40.4 / 39.7 → 37.7 / 36.7 ms, Q20 170 / 167 → 162 / 156,
  Q21 420 / 425 → 440 / 426 (within its rounds' spread: 426 against
  425) —; row tables and
  ClickBench unchanged (row tables read no segments).
  *Done — one grouping table a worker*: the price of the step above it
  that bounds groupings by `work_mem` (Q13, Q36: groups recurring in
  every morsel, so the morsels' tables together passed `work_mem` though
  the merged groups fit).  A worker now keeps one table across the
  morsels it takes (DuckDB's thread-local hash tables, HyPer's morsel
  workers): a group's rank is its morsel and place in it (`(morsel <<
  32) | place`, a spilled group's too), a merge part takes a group's
  least rank rather than its first, and the merged groups are finished
  in rank order — the order a single pass first saw them, whichever
  worker took which morsel.  Only when every aggregate is `order_free`
  (`array_agg` combines, but its elements come in the order fed).
  Adaptive: on its second morsel a worker sees whether its groups recur
  (fewer than half new); when they do not (`GROUP BY WatchID, ClientIP`)
  it hands each morsel's table over as before, so places stay cheap and
  nothing waits for the worker's end — keeping them had cost Q32 413 →
  470 ms.  Not taken: a shared concurrent table (DuckDB's earlier one,
  Leis et al.'s for HyPer) — its atomic inserts cost every row what the
  per-worker merge costs a group.  Tested by the suite's groupings on
  two and four workers, spilled and kept (a single pass's order and
  error); a morsel that spilled on the way says nothing of recurrence
  (its table was emptied: a subtraction that overflowed, caught by the
  suite in a debug build, wrapped in the release one measured first);
  mutations caught (the first rank kept rather than the least, finished
  in part order, `array_agg` let through).  Measured, alternating with
  the step before, two rounds: TPC-H on column tables 3.82 / 3.78 →
  3.79 / 3.77 s — Q13 109 / 105 → 94 / 97 ms; on row tables (the build
  before the fix) 8.18 / 8.28 → 7.83 / 8.00 s, Q13 175 / 184 → 164 /
  167; ClickBench 2.43 / 2.41 → 2.46 / 2.43 s — Q32 423 / 415 → 430 /
  420 ms, Q36 71 / 73 → 75 / 79 (the rank sort at the end; Q36's groups
  fit `work_mem` since typed states, so the bound no longer cost it).
  *Done — joins inside pipelines* (profiled first: TPC-H Q9 on column
  tables, 878 ms on the workers against 1,430 on one — only the lowest
  join's probe ran on them; the three joins above it, their projections
  and the grouping ran a row at a time on one thread, as every join over
  a join did: Q5, Q7, Q10 alike).  An inner, left, semi or anti hash join
  whose left side is a pipeline is now a step of it (`Step::Probe`), so
  a chain of joins and the grouping, sort or projection above it run as
  one pipeline on the morsels of the lowest scan — HyPer's morsel-driven
  pipelines (Leis et al., "Morsel-Driven Parallelism", SIGMOD 2014; the
  probe as a pipeline operator, its hash tables built before the
  pipeline that probes them).  `vector::prepare` makes the tables before
  the pipeline runs, outermost join first as the row-at-a-time join runs
  its right sides, each with its key filter; the workers' contexts get
  them, and they drop once the pipelines are made (`Prepared`, a guard).
  A probe joins a batch into a batch: the left columns gathered by the
  matched rows, the table's from its columns (`Columns::gather`), a left
  join's unmatched rows with `NULL`s, a semi or anti join's by the
  selection alone; `on` beyond the keys over every candidate pair at
  once (an error there sends the batch a row at a time, which then
  fans out each row's matches through the steps above).  A right side
  past `work_mem` is stashed and the pipeline does not run as one: the
  row-at-a-time join takes the stashed rows and the tables made above
  them (`take_built`), so no side runs twice.  Not with an enclosing row
  (a correlated subquery's plan runs again per row: its tables would be
  stale); a recursive CTE's term makes them each round.  Not taken:
  DuckDB's and Velox's way, the probe emitting through a selection vector
  over the table's rows (late materialization of the build side) — the
  gather is a copy a matched row, and the columns read above a join are
  few after pruning; worth measuring once a join's output feeds only a
  grouping.  Tested: joins over joins under groupings, projections and
  sorts, fan-out, left joins with `on` beyond the keys, semi and anti
  joins in the chain, text and `NULL` keys, errors in `on` and above, a
  build side that is itself joins, a correlated subquery's and a
  recursive CTE's joins, a stashed side mid-chain, `EXPLAIN ANALYZE` —
  against the row-at-a-time executor, one thread, batches of 3 and
  `work_mem` 64 kB, on row and column tables; TPC-H's 22 results the
  previous build's on column and row tables, at 1 MB `work_mem`, one
  worker and `batch_size` 0.  Mutations caught: a left join's unmatched
  rows dropped (with and without `on`), semi and anti inverted, the
  tables never dropped (the recursive CTE's term probing a stale table
  never ends).  Not caught: a table made by an earlier caller not
  adopted (the row-at-a-time join then makes the rest: the same rows,
  slower); a join's rows before its error given after it (visible only
  under a `LIMIT`, see below).  Found on the way, not from this step: a
  `LIMIT` over a join whose `on` fails on a later candidate gives the
  rows or the error depending on the statements the session ran before
  — on the row-at-a-time executor too (`batch_size` 0); open.  Measured,
  alternating with the step before, two rounds: TPC-H on column tables
  4.00 / 3.78 → 2.30 / 2.27 s (geometric mean 107 → 77 ms): Q9 882 → 243
  ms, Q5 308 → 80, Q10 391 → 142, Q7 198 → 89, Q3 123 → 75, Q18 264 →
  171, Q14 92 → 59, Q13 99 → 60; on row tables 8.38 / 8.01 → 7.16 / 7.21
  s (Q9 1,136 → 530, Q18 552 → 444, Q13 170 → 128); ClickBench 2.50 /
  2.47 → 2.48 / 2.42 s (no joins).
  *Done — `CASE` by batches, rows reading their columns only*: a `CASE`
  went a row at a time, and every expression that does — `CASE`,
  `ILIKE`, a correlated subquery, … — built each row with all the
  batch's columns (a long `URL` copied for a test of two integers).  Now
  `CASE` runs a branch at a time: each `WHEN` over the rows no earlier
  one took, its `THEN` over those it takes, `ELSE` over the rest
  (`logic`'s way for `AND` / `OR`: a row evaluates what it would alone,
  so a branch no row takes raises nothing); what still goes a row at a
  time holds only the columns the expression reads (`Expr::reads`, a
  correlated subquery's included).  Both `#[inline(never)]`: inlined into
  `eval`, their frames slowed every expression on row tables (TPC-H Q6
  299 → 360 ms, Q14 288 → 351, as with `Expr::eval` before).  Not taken:
  typed branch results merged without `Value`s (the merge is a fraction
  of the branches' work).  Tested: searched and simple `CASE` (a `NULL`
  operand, a `WHEN NULL`), nested, in a filter, a grouping key, under
  sums, mixed types; errors in a branch taken, in a `WHEN` after one that
  took the row, in `ELSE`, and none from a branch no row takes — against
  the row-at-a-time executor and batches of 3.  Mutations caught: a row
  left for later `WHEN`s after one took it; the row fallback reading no
  columns.  Not caught (the batch's error sends it a row at a time, which
  decides): `THEN` or `ELSE` over more rows than they take — slower, the
  same rows.  Measured, alternating with the step before, two rounds:
  ClickBench 2.39 / 2.37 → 2.34 / 2.34 s — Q39 273 / 270 → 232 / 230 ms
  (`CASE WHEN … THEN Referer ELSE '' END`); TPC-H unchanged (column 2.29
  / 2.26 → 2.26 / 2.25 s, row 7.07 / 6.97 → 7.00 / 6.88).
  *Done — one key table for groupings and joins*: a join table found
  its keys in hash maps of its own (`Groups<K>` over `FastMap`; any
  other keys as a `Vec<Value>` built for every probe row), a grouping in
  `keytable`'s.  Now a join's single integer key goes into `IntKeys` (a
  direct index while the keys span at most 65,536 values — TPC-H's
  nation, supplier and the like —, hashed beyond) and any other key into
  `KeyTable`, a probe's key hashed and compared from the batch's
  vectors; a pair of integers stays a pair in `FastMap` (no such table
  for groupings yet).  `Runs` gives each key's rows.  Not taken yet:
  `DISTINCT` and set operations on the row-at-a-time path keep
  `HashSet<Row>` (the vectorized `DISTINCT` is a grouping already); one
  hasher for the rest (`fasthash`'s and `MixState`'s mix differ, no
  query measured needs it).  Tested by the suite's joins (text, `char`,
  mixed and `NULL` keys, on workers and past work_mem).  Not caught by a
  test, as expected: keys not compared beyond the salt (32 bits of the
  hash: a wrong match needs a collision), `NULL` probe keys not skipped
  (no table group holds one).  Measured, alternating with the step
  before, two rounds: unchanged — TPC-H on column tables 2.26 / 2.31 →
  2.30 / 2.30 s, row tables 6.96 / 6.95 → 7.08 / 7.03 (within the
  rounds' spread), ClickBench 2.36 / 2.33 → 2.37 / 2.33; one mechanism
  rather than two, not a speed-up.
  *Done — sorts on normalized keys* (profiled first: ClickBench's sorts,
  Q24–Q26, are top-N heaps of 6–7 ms — nothing to gain there; a whole
  sort of a million rows by `(EventTime, WatchID)` took 375 ms on the
  workers, its sort node 880 ms on one thread past the scan's 280, and by
  `(URL, EventTime)` 775 ms).  A morsel's sort, and a sort's buffer in
  memory, compare each row's keys as one byte string when every key
  column holds one kind whose index encoding orders as `compare` does
  (`spill::sort_keyed`: `encoding::encode_key_value` — one mechanism for
  index keys and sort keys — after a byte placing a `NULL` first or last,
  a descending key's bytes inverted; a stable sort of row numbers by
  `memcmp`, the rows then moved once).  DuckDB's normalized keys (Raasveldt,
  "Fastest table sort in the West", 2021) and the System R line's; not
  taken: DuckDB's fixed-width key prefix with a tie-breaking comparator
  and its radix sort (the gain measured is the comparisons' — the rest
  of a sort is building and merging rows, `Value`s), doubles and decimals
  (`NaN`, `-0`, a decimal's scale: they keep the comparator).  Tested:
  random keys of every kind it takes — texts that are prefixes of each
  other, with zero bytes, padded `char(n)`, `NULL`s — under every
  direction and `NULL` placement, and kinds it does not (an integer and a
  double mixed: falls back), against a stable sort by `compare`;
  mutations caught (a descending key not inverted, `NULL` places swapped,
  an unstable sort, a mixed column normalized).  Measured, alternating
  with the step before: the whole sorts 421 / 436 → 377 / 368 ms and 978
  / 793 → 893 / 730; the benchmarks unchanged (TPC-H on column tables
  2.32 / 2.34 → 2.30 / 2.33 s, row tables 7.11 / 7.04 → 7.19 / 7.12,
  ClickBench 2.37 / 2.33 → 2.33 / 2.37).
  *W6 closed* (2026-11-04).  Left, recorded rather than done: hashes a
  column at a time and dictionary codes as group keys (a grouping's
  time over ClickBench is now its aggregates' and its keys' text, no
  query measured above 3× waits on them); `DISTINCT` and set operations
  on the row-at-a-time path in maps of their own, one hasher for
  `fasthash` and `MixState`; the open `LIMIT`-over-a-failing-join
  finding above.
- **W7 — formats (D18).**  Column offsets in the row format (a row
  table's late decoding: ClickBench on a row table 2.13×); a column
  chunk stored as extents, not an overflow chain; one buffer for a batch's
  WAL record and its tree application.
  *Row format version 3* (2026-11-05).  Profiled first, not assumed:
  ClickBench on a row table reads the whole table at every query, a
  984 MB file past the 128 MiB cache, and of a scan's time `pread`, the
  pages' checks, zeroing and copies are about two thirds (`count(*)`,
  which decodes nothing, 140 ms; Q2, two columns, 120), decompressing the
  rows over 1 KiB another tenth, decoding the columns read at most an
  eighth.  So an offset for every column — 210 bytes on `hits`' rows of
  about 860, a quarter more to read — was *not taken*: PostgreSQL has none
  either (it caches the offsets of a row's fixed-width prefix,
  `attcacheoff`), nor does InnoDB; SQLite's record header has a type per
  column, from which offsets are summed.  Taken instead: an offset every
  16 columns (`u16`, `FFFF` past 64 KiB; 12 bytes on `hits`), so a read
  skips at most 15 values to a column — and the bytes a scan reads cut:
  text, `char(n)`, bytea and enums under 256 bytes with a one-byte length
  (tags 22–25, rows only; the wire keeps `encode_value`'s four bytes).
  One writer for every stored row (`encoding::RowWriter`, used by
  `large::encode` and `encode_inline` alike) and one seek for both
  decoders (`RowHeader::seek`); rows of version 2 and 1 still read, and a
  row is written in version 3 when next written.  `hits` 984 → 902 MB.
  Tested: rows of 1 to 200 columns, offsets past 64 KiB, every pick of
  columns, against the values and against the same rows in version 2; a
  table of version-2 and version-3 rows side by side, a row at a time and
  by batches (it found a version-2 row of more than 16 columns
  overrunning the offsets it does not have); mutations caught (an offset
  off by one, a mark's column, a short length skipped wrong) — not
  caught: giving up at an `FFFF` mark rather than taking the one before
  (only slower).  Measured, alternating with the step before, two
  rounds: ClickBench on a row table 7.30 / 7.16 → 6.25 / 6.25 s,
  geometric mean 157 / 153 → 130 / 130 ms (each of the 43 queries
  faster, Q40 165 → 108; `SELECT *`'s Q23 174 → 157 once a read in order
  skips the seek); TPC-H on row tables 6.64 / 6.60 → 6.45 / 6.43 s (Q12
  378 → 344), on column tables and ClickBench on a column table
  unchanged.  Results the same but for ties at a `LIMIT` (Q18, Q32,
  Q39, as before).
  *Page reads without zeroing* (2026-11-05), from the same profile: a
  read of the file allocated and zeroed its run of up to 16 pages, then
  allocated and zeroed each page and copied it in — `memset` and the
  kernel's `madvise` 8 % of `count(*)`'s time.  Now each thread keeps
  its read buffer and each page is copied out of it once, unzeroed
  (`pager::read_run`, for one page and for a run); and a batch scan
  reading no column does not parse its rows' headers.  Reading into the
  frames themselves (no copy at all) would need the pool to hand out its
  victims before the read — not taken until a profile asks again.
  Measured, alternating with the step before, two rounds: ClickBench on
  a row table 6.68 / 6.39 → 6.16 / 6.09 s, geometric mean 140 / 135 →
  128 / 126 ms, Q0 93 → 78; TPC-H on row tables 6.59 / 6.44 → 6.59 /
  6.55 s and on column tables 2.20 / 2.16 → 2.16 / 2.15 (the page
  reads alone, before the header skip), ClickBench on a column table
  unchanged.
  *The file grown ahead* (2026-11-05).  W7's other two formats were
  measured before being built, and neither is taken.  *A column chunk
  as an extent:* on ClickBench's `SELECT *` over a column table
  (`hits`, 407 MB) reading the overflow chains is 1–1.5 % of the time,
  decompressing the chunks' LZ blocks 29 %; the suites' first runs, cold,
  are within a tenth of their best — the chunks are in the cache, and an
  extent saves only the cold read's single-page reads (the chain's pages
  allocated last to first, so read-ahead never joins them).  Left for a
  working set past the cache.  *One buffer for a batch's WAL record and
  its tree:* the copies are about 1 % of a logged `COPY` (`memmove` from
  `Transaction::record`, `Wal::append`, `encode_leaf` together), as W4
  found.  What that profile showed instead: a logged `COPY` of
  `lineitem` (into a table made in an earlier transaction, outside D16)
  spent more than half its statement's time in `pwrite`, evicting a
  dirty page for each page it made — and a write that makes the file
  longer costs APFS far more than one into blocks the file has (8 KiB
  pages written at the end 47 µs each; into zeros written ahead in 1 MiB
  pieces 2 µs, 8 with the zeros).  Taken: the pager grows the file ahead
  of its pages, by zeros an eighth of its length at a time, 1 to 64 MiB
  (`pager::extend`, under the state lock every write past the known
  length already took) — as PostgreSQL extends a relation by many blocks
  at once (16) and SQLite by `SQLITE_FCNTL_CHUNK_SIZE`; `F_PREALLOCATE`
  measured the same as zeros and needs an `fcntl` the platform crate
  would have to allow; zeros past the header's page count are ignored on
  open and written over by later allocations.  Not taken: the same for
  the WAL's segments (a small commit with an ordered sync 206 → 174 µs
  into zeros written ahead, with a full sync no better — not enough to
  preallocate and recycle segments yet).  Tested: the file grown in
  chunks, reopened longer than its header says, its zeros written over;
  the pager's crash at every I/O point covers the zeros' writes.
  Measured, alternating with the step before: the load of TPC-H SF 1 on
  row tables 56.6 / 55.2 → 26.3 / 27.9 s (by phase, one run each: the
  `COPY`s 27.8 → 14.5 s, `lineitem`'s 23.5 → 10.0, the indexes 34.4 →
  12.9, `ANALYZE` 4.6 → 1.8; total 66.7 → 29.3), on column tables 34.4
  / 32.8 → 22.1 / 21.3 s; a logged `COPY` of `lineitem` 24.2 / 21.1 →
  9.6 / 8.8 s; ClickBench's load (into a new table, the file growing in
  one checkpoint's large writes) and every query unchanged.  The step
  before's row load, 44–57 s in today's runs, had been 29 s at W3b on
  the same machine: what grew since is the file system's cost of
  growing the file (the disk fuller), not bisected further — this step
  takes it away either way.
  *LZ blocks decoded by wild copies* (2026-11-05).  Decompressing values
  was the profiles' largest single item left: 29 % of ClickBench's
  `SELECT *` on a column table (its chunks' LZ blocks), a tenth of a row
  table's scan (rows over 1 KiB).  The decoder appended each sequence
  with `extend_from_slice` / `extend_from_within` — a capacity check and
  a call to `memmove` each, for literals and matches mostly under 16
  bytes.  Taken from LZ4's decoder (its "wild copy"): the output sized
  first with 32 bytes of room past its end, literals and matches copied
  in fixed 16-byte pieces that may overrun a sequence (what follows
  writes over the excess), a match overlapping itself a byte at a time
  for 16 bytes and then 8 at a time from a multiple of its period 8 to 16
  back — in safe Rust (fixed-length `copy_from_slice` / `copy_within`
  compile to plain loads and stores), every length and offset checked as
  before.  Not taken: LZ4's 4-byte period table for offsets under 8 (the
  byte loop it replaces runs 16 bytes once per such match).  Measured on
  `hits`' first 32 MB of text, decoded in blocks: 1 KiB 1.19 → 2.35
  GB/s, 8 KiB 1.72 → 3.76, 64 KiB 1.66 → 4.14.  Tested against the plain
  decoder (kept in the tests) on periodic runs of every period to 20,
  random data, and the blocks cut, with a bit flipped and made up — the
  same result, `None` alike, and a failure leaves the output as it was;
  mutations caught (the period's multiple, the bytes copied one at a
  time, the overlap bound, the literals' bound).  Measured, alternating
  with the step before, two rounds: ClickBench on a column table 2.24 /
  2.23 → 2.20 / 2.20 s, geometric mean 19.0 / 19.2 → 18.1 / 18.2 ms (Q23
  133 → 116); on a row table 6.34 / 5.75 → 5.86 / 5.92 s (the rounds
  apart by more than the change); TPC-H 6.55 / 6.40 → 6.34 / 6.44 s
  (row), 2.15 / 2.14 → 2.11 / 2.14 (column) — its Q9, slower in those
  rounds, the same or faster alone (262 / 262 → 257 / 227 ms, seven
  runs each).
  *W7 closed* (2026-11-05).  Of the three formats D18 named, the row
  format's offsets were taken one in 16 columns, with shorter lengths;
  extents and the shared WAL buffer were measured and not taken (1–1.5 %
  and 1 %).  What the profiles found instead was taken: page reads
  without zeroing, the file grown ahead, LZ by wild copies.  Since W6,
  alternating with each step before it: ClickBench on a row table 7.2 →
  5.9 s (geometric mean 155 → 122 ms, its file 984 → 902 MB), the TPC-H
  load on row tables 56 → 27 s and on column tables 34 → 22, a logged
  `COPY` 22 → 9 s; TPC-H's queries 6.6 → 6.4 s (row), 2.16 → 2.12
  (column); ClickBench on a column table 19.4 → 18.15 ms.  Left,
  recorded: extents for a working set past the cache, the WAL's segments
  preallocated and recycled, reading into the buffer pool's frames
  without a copy.
- **W8 — per-statement costs** ✅ (what AI agents' traffic pays most): a
  catalog epoch, and on it a session's catalog, settings and plan cache
  (pg-compat's prepared statements onto the same); one bind for describe
  and run; pipelining and array binds in the drivers; one `ncp-wire` crate
  for the server and the Rust client (four encoders and decoders today);
  the C and Python drivers' buffering; the next result batch requested
  while the client handles one (from W1).
  Measured first (`crates/client/examples/latency.rs`, one connection,
  20 000 statements each, median): `SELECT 1` 28 µs, a point read 42
  (literal) / 48 (parameter), an insert 184, an update by key 197.  A
  profile of the parameterized point read: the binder about 40 % of the
  session thread's busy time — the privilege checks (roles, ACLs and
  owners read and decoded each statement) most of it, the table's
  definition decoded —, database settings read 10 %, a parallel scan's
  morsels and split points set up for one key 5 %, parsing 8 %, the
  optimizer 6 %.
  *The catalog epoch and a session's catalog cache.*  The KV store
  watches key ranges: a commit writing into one raises one mark under the
  commit lock, before it is visible, and a transaction answers whether
  it reads the watched keys as a snapshot at a given timestamp did
  (`Kv::watch`, `KvTxn::watched_as_at`; DOCS §5.2).  A session keeps
  what its statements derived from the catalog — tables, database
  settings, ACLs, owners, effective roles — and uses it while that
  holds; a snapshot that reads the catalog otherwise (begun before
  another's DDL, `AS OF`, its own DDL, serializable and writing) reads
  the catalog itself (DOCS §8.2).  Taken: SQLite's *schema cookie* — one
  counter, bumped by every schema change and checked by each statement,
  the schema reloaded when it moved —, made snapshot-aware: SQLite has
  one writer and its readers see the newest schema, nucleus's snapshots
  each see their own.  Not taken: PostgreSQL's per-object invalidation
  messages (each backend's relcache and syscache entries invalidated by
  a shared queue read at command start) — finer, but a queue to keep,
  and catalog writes are rare; one mark is enough until a measurement
  says otherwise.  DuckDB versions its catalog entries per transaction;
  nucleus's catalog is already MVCC in the KV store, so the cache only
  has to know when it is current.  Tests: another session's DDL, `GRANT`
  / `REVOKE` and role membership, a database setting, an older snapshot
  beside a newer one, a transaction's own DDL rolled back, serializable;
  in the engine, spilled writes; mutations caught (the validity check,
  the mark not raised, a transaction's own writes, the spilled span).
  Measured, alternating with the step before, two rounds: a point read
  52.2 / 52.1 → 41.8 / 42.1 µs (parameter), 44.9 / 45.5 → 38.5 / 38.5
  (literal), `SELECT 1` 28.3 / 28.3 → 24.8 / 24.9; writes within the
  noise of their fsyncs; TPC-H on row tables 6.85 / 6.55 → 6.68 / 6.47 s
  (unchanged, as it should be).
  *A point read set up as one.*  Three costs the profile showed beside
  the catalog: a scan of one key of a unique index was offered to the
  parallel workers — its key range computed and the B+tree asked for
  split points, a row at most to share — and now is not; the B+tree's
  `split_keys` looked at every child of every node on its way down, and
  now finds the children in the range by binary search (what its point
  descents already did; a test compares it with the old walk over trees
  three and four levels deep, bounds between keys and on them); and the
  real platform asked the OS for its CPU count — a system call — each
  statement, now once.  Measured, alternating with the step before, two
  rounds: a point read 38.7 / 38.9 → 35.7 / 35.9 µs (literal), 42.5 /
  42.9 → 40.0 / 39.8 (parameter), `SELECT 1` 24.5 / 24.8 → 23.8 / 23.8;
  TPC-H on row tables 6.69 / 6.60 / 6.59 → 6.60 / 7.00 / 6.65 s (the
  second round slower on every query alike: the machine), on column
  tables 2.08 / 2.08 → 2.08 / 2.12.
  *A session's plans, by their text.*  On the same mark, a session keeps
  a query's plan — bound, its privileges checked, optimized, pruned —
  and a statement's description (its parameters' types, its columns) by
  the statement's text: the server describes a query with parameters
  and then runs it, and both bound it each time.  An entry holds its
  statement and serves the same one only (a text noted for another is
  read again, not trusted); a kept plan replays the tables its checks
  looked at, for the audit log's `audit_tables`; the cache empties when
  the session's identity changes (`SET ROLE`: a plan's privileges and
  row-level security were checked for who ran it).  Taken: PostgreSQL's
  plan cache for prepared statements (`plancache.c`: a plan kept and
  invalidated with the relations it depends on), here for every query by
  its text, as MySQL's and Oracle's shared cursors are found by theirs —
  without their sharing across sessions (a lock and memory to bound; one
  session's plans first).  TinyLFU's *doorkeeper*: a text is kept the
  second time it comes, so a query with its literals inline — never the
  same text twice — costs nothing to keep and pushes nothing out; at
  most 512 texts are kept, all dropped past it.  Not taken (yet): a
  plan per parameter value (PostgreSQL's custom plans) — the optimizer
  does not look at parameters' values today.  Tests: a kept plan under
  `SET ROLE` with row-level security and without the privilege, the
  audit log of a kept plan, a description by a text noted for another
  statement, the doorkeeper and the bound; mutations caught (the
  statement compared, the cache emptied for a new identity, the tables
  replayed).  A query not kept is planned where it runs, as before: a
  first version that planned every query into the kept form (boxed,
  shared) ran TPC-H's Q18 15 % slower even with nothing kept (430 / 393
  → 481 / 475 ms, alternating, twice; the profiles alike — not
  explained), and as it is now 401 / 400 → 406 / 390.  Measured,
  alternating with the step before, two rounds: a point read with a
  parameter 39.3 / 39.0 → 28.9 / 29.1 µs, `SELECT 1` 23.6 / 23.5 → 21.4
  / 20.8; with a literal (a new text each time, never kept) 35.5 / 35.5
  → 36.4 / 36.2 — the text hashed and remembered, what the doorkeeper
  costs; TPC-H on row tables 6.81 / 6.85 → 7.26 / 6.79 s, its Q9 the
  difference and alone the same or faster (base 525 / 547 → 495 / 510
  ms, nine runs each).
  *pg-compat's prepared statements on the same entries.*  `Parse` kept
  its description and `Execute` ran whatever the catalog then planned: a
  driver that had read two columns got three after another session's
  `ALTER TABLE`.  Now `Parse` describes by the statement's text, and
  `Bind` describes again — from the session's cache while the catalog
  holds — and refuses a statement whose result columns changed with
  `0A000` "cached plan must not change result type", as PostgreSQL does
  (`plancache.c`'s revalidation of a statement with a fixed result); its
  plan comes from the same entries, by the text `Execute` notes.
  `Session::describe` takes the text explicitly (no hidden noted text).
  Test: through the `postgres` driver, a statement prepared, another
  session's `ALTER TABLE`, the statement refused, another one whose
  result held still run, and the first prepared again; mutation caught
  (the check skipped).  Measured, alternating with the step before, two
  rounds (`modules/pg-compat/examples/pg_latency.rs`, new): a prepared
  point read 41.0 / 40.4 → 40.8 / 40.6 µs, an update by key 193 / 189 →
  190 / 192 — the check costs nothing seen.  Through this front end a
  point read costs 40 µs to the native protocol's 29: next to look at.
  `INSERT` / `UPDATE` / `DELETE` plans are not kept yet (their time is
  their commit's).
  *A session's texts, parsed and normalized once.*  What the profile of
  the parameterized point read showed next: the statement statistics
  tokenized each statement's text again to normalize it (8 % of the
  session thread's busy time), and the parser read it each time (7 %).  A session
  now keeps both by the text — its statements as parsed
  (`Session::parse`, which `execute`, the native server, `ncsql`'s local
  database and pg-compat's simple query and `Parse` use) and its key and shown form in the
  statistics — with the plans' admission and bound, the one type every
  entry by a text is kept in (`by_text::ByText`: the doorkeeper, 512
  entries, 4 096 texts remembered).  Neither reads the catalog, so
  neither empties with it.  Taken: Oracle's *soft parse* — the library
  cache finds a statement by its text's hash and skips the parse —,
  per session.  Not taken: pg_stat_statements' key from the parsed tree
  (a hash over the tree after analysis, no second pass over the text) —
  every statement's tree walked to spare a repeated text's lookup; for a
  text never seen again the second tokenization stays, and handing the
  parser's tokens to the statistics is what would spare it.  Tests: a
  text parsed again equal to a fresh parse, kept the third time, an
  error each time; runs counted under the normalized text kept or not.
  Measured, alternating with the step before, two rounds: a point read
  with a parameter 29.4 / 29.2 → 25.8 / 25.8 µs, `SELECT 1` 21.6 / 21.7
  → 20.8 / 20.4, with a literal 36.7 / 36.5 → 37.0 / 36.5 (unchanged: a
  new text each time); a prepared point read through pg-compat 41.0 /
  40.5 → 39.3 / 39.4 µs; TPC-H on row tables 7.06 / 6.83 → 6.79 / 7.19 s
  (the machine: the queries run what they ran).
  *Pipelining.*  A profile of the server and the client side by side
  (the parameterized point read, 25 µs): the server works 5.7 µs of it
  and spends 3.6 in `sendto`, the client 1.3 and 2.1; the rest is the
  loopback's wake-ups.  What is left of one statement's cost is the
  round trip, and an agent sends many statements at once.  The protocol
  always let a client send its next Query before reading the reply (the
  server answers in order), but the server flushed after each Ready.
  Now it flushes only when no next whole message waits in its read
  buffer, and the Rust driver's `Client::pipeline(&[(sql, params)])`
  sends statements and reads their replies, each as `execute`'s (a
  failure in its place, the rest run).  It sends at most 32 KiB before it
  reads (`PIPELINE_WINDOW`): with both sides only writing — the client
  its statements, the server replies it does not yet read — they would
  wait on each other once the sockets' buffers fill; a window that fits
  in them is always written whole (a test sends 2 MB of statements
  answered by 40 MB, and without the window hangs).  Taken: PostgreSQL's
  pipeline mode (libpq 14: queries sent before their results are read,
  the server flushing at `Sync`) and Redis's replies gathered and written
  when its input is drained; not taken: libpq's non-blocking socket,
  reading while it writes, to avoid the same deadlock — a window bounded
  in bytes needs no second mode of I/O.  Measured, alternating, two
  rounds, the new client on both: a point read 100 at a time 11.5 / 11.5
  → 5.8 / 5.9 µs a statement (one at a time 25.7); the other statements
  as before.
  *Every driver pipelines; a statement for many rows.*  `pipeline` in
  Python, JavaScript, Java, C and C++ as in Rust (the same window, each
  statement's result or error in its place), and `execute_many`
  (`executeMany`) — one statement for many rows of parameters — on it:
  outside a transaction block the rows run in one of their own (`BEGIN`,
  the rows, `COMMIT`, in the same pipeline), so they are all or none and
  cost one commit; inside one they run in it.  This is what array binds
  are for (JDBC's `executeBatch`, psycopg's `executemany` in pipeline
  mode); the protocol needs no message for them — a `Q` per row costs
  its parse and plan from the session's cache — and the transaction is
  PostgreSQL's implicit one up to `Sync`, made explicit.  Python's DB-API
  `executemany` is it now: before, it ran a statement at a time, each
  committed alone, and a failure left the rows before it.  The C
  driver's buffering (from W1): a frame went out in two `send`s, and was
  read in two `recv`s and more; now one `send`, and replies read through
  a 64 KiB buffer (the others already buffered: Python reads 64 KiB at a
  time, Java through a `BufferedInputStream`, JavaScript by chunks).
  Tests in each driver: order kept over many windows, a failure in its
  place, a text without statements, a failed block's statements after
  it, and a statement for many rows all or none; mutation caught (the
  rows not in a transaction of their own).  Measured: inserts 100 rows a
  call 11.3 / 12.4 µs a row, against 185 / 184 one at a time; the
  parameterized point read through the C driver 28 / 27 → 25 / 25 µs
  (macOS's clock in whole µs), its 99th percentile 78 / 63 → 40 / 46.
  *One wire format crate.*  The protocol was written down four times in
  Rust: the server's values (through the storage's row encoding, whose
  tags 0–10 the protocol once shared — and whose tags past them mean
  other things: a parameter tagged 17 was read as a `time`), its
  parameters, its columnar batches both ways, and the Rust driver's own
  encoder and decoder.  `nucleus-wire` (`crates/wire`, named like the
  other crates) holds them once: the versions, message kinds and value
  tags by name; frames (`frame`, `begin_frame` / `end_frame`,
  `read_frame`, `message_waiting`); a body's fields (`Reader`); a
  value's encoding (`put::…`) and decoding (`read_value` into a
  `WireValue`, which each end maps onto its own values); the columnar
  layout's widths and padding.  The server encodes the protocol's values
  itself now, so the storage's row encoding can change without the
  wire.  Not taken: one value type for both ends — the server's rows
  would be converted value by value on the hottest path; the protocol's
  vocabulary and codecs shared, each end's values its own, is the
  boundary.  Tests: the crate's round trips and cut bodies (malformed,
  never a panic); a script comparing the old and new servers' replies
  byte for byte (every type as rows and as columnar batches, parameters,
  an error: identical); every driver's suite.  Measured, alternating, two
  rounds: per statement the same (a parameterized point read 25.5 / 25.5
  → 25.6 / 25.7 µs); a million rows of four columns read as rows 486 /
  479 → 460 / 446 ms, as columnar batches 147 / 149 → 147 / 146
  (`crates/client/examples/bigread.rs`, new).
  *The next batch made while the client reads one* (from W1).  With a
  fetch size the server sent a batch and `S`, then sat waiting for `F`
  while the client worked through the batch, and the client waited for
  the next one to be made after asking.  Now the rows after an `S` are
  made and held — up to a batch of the size last asked for — and go out
  when `F` comes: as many as it asks for, another `S` after them if
  some are left (a columnar batch read ahead and larger than asked for is
  cut, its rows decoded and rebuilt — a size change no driver makes), or
  dropped on `c`, Done counting the rows sent.  The protocol is unchanged:
  the client cannot send `F` ahead itself, since one arriving after the
  result's end would be read as the next message.  An error in a row
  read ahead ends the statement even when `c` follows (the session has
  failed it; a sort's input would have failed the same way before any
  row went out).  Taken: PostgreSQL's portals run only on `Execute`, and
  libpq's `PQsetSingleRowMode` / chunked mode only spare memory — neither
  reads ahead; this is the double buffering of a cursor's fetch that
  Oracle's prefetch (`OCI_ATTR_PREFETCH_ROWS`) and JDBC's fetch size do on
  the client side, moved to the server, where the rows are made.  Not
  taken: a thread making rows while another sends them — the executor
  already hands rows to the sink in order, and the sink holding a batch
  is all the overlap a fetch needs.  Tests: the rows after a pause made
  before the client asks for them (by `clock_timestamp()`), batches of
  every size asked for by hand, row by row and columnar, smaller and
  larger than read ahead, stopped, exactly a batch more; mutations
  caught (one row read ahead, the count of rows sent, the columnar cut).
  Measured, alternating, two rounds, the new client on both: a million
  rows through a cursor, 1000 a batch, as rows 600 / 615 → 461 / 466 ms,
  as columnar batches 178 / 180 → 165 / 166; a Python stream of a
  million rows, 1000 a batch, summing them 2787 / 2810 → 2429 / 2370 ms;
  whole results (no fetch size) the same, 453 / 454 → 466 / 459 and 147 /
  150 → 146 / 148.
  *W8 closed* (2026-11-05).  Since W7, alternating with each step before
  it: `SELECT 1` 28.3 → 20.4 µs, a point read with a parameter 52.1 →
  25.8 and with a literal 45.5 → 36.5; 100 point reads pipelined 5.8 µs
  each, inserts 100 a call 11.5 µs a row against 185 one at a time; a
  cursor of a million rows 600 → 461 ms.  One wire format crate for the
  server and the Rust driver, one by-text cache for plans, parsed
  statements and the statistics' keys.  Left, recorded: pg-compat's
  prepared point read (39.4 µs against the native 25.8); `INSERT` /
  `UPDATE` / `DELETE` plans kept; a plan per parameter value; the
  statistics' second tokenization of a text never repeated.

Each step records what it took from where, and what it measured, here
and in `docs/benchmarks.md`.

**Done when:** every storage mode works per table, per column and per
partition, with online conversion between them under crash and
concurrency tests, the same query giving the same result in each mode;
partition pruning and the age policy work; and on the §M14.11 suite,
analytical queries (TPC-H,
ClickBench) are faster than PostgreSQL on the same machine by the
recorded targets, transactional workloads stay within the recorded
margin of it, every SIMD kernel matches its scalar reference under the
law kit, and a table's columnar copy stays consistent with its rows under
crash and concurrency tests.
✅ (2026-11-06; agreed: "M14.20 is").  The full runs against PostgreSQL
18.6 on one machine (`bench/tpch.py`, `bench/clickbench.py`,
`bench/run.py`, nucleus on `a2e7f6f`; `docs/benchmarks.md`): TPC-H SF 1
0.25× PostgreSQL's time on column tables and 0.77× on row tables in the
geometric mean (it was 0.47× and 1.11×), no query above 1.93× (Q19);
ClickBench's first million rows 0.11× and 0.89× (0.24× and 2.13×), no
column query above 1.36×; the loads 0.55–0.92× (3.1–3.8× and 2.4–2.7×
before V14); `bench/run.py` within every margin (load 1.1×, update
0.9×, delete 1.5×, scans, sorts and joins 0.6–1.8×, reads by key 0.4×).
Every result the same, but for ties at a `LIMIT`.  Left as findings,
not targets: the `LIMIT`-over-a-failing-join finding (W6), TPC-H Q19 on
column tables (an `OR` of conjunctions across a join, 1.93×).

### §M14.27 — Statistics and numerical analysis on large data ✅ (study written; S1–S8 ✅)

**First a study, then the plan.**  Can nucleus give mathematical and
statistical functions over large data — fast, exact where exactness is
promised, in bounded memory — that make it useful to AI models, to the
developers building AI applications and to those who build and train AI
itself?  Nothing of the kind is in the engine yet (no `stddev`, no
`variance`); the analytics engine of §M14.20 (vectorized, parallel
aggregates, column storage, SIMD kernels) is what it would run on.

**The study** (2026-11-05; what it asked is kept in its answers).

*Who needs what, and how they get it today.*

- **An AI agent analysing data for a user** asks first "what is in this
  table" — per column the count, the `NULL`s, the distinct values, the
  range, the mean and spread, the quartiles (pandas' `describe()`,
  DuckDB's `SUMMARIZE`) — then "what is unusual" (values far from the
  mean or outside the quartiles' fences, groups whose rate moved), then
  "how does A relate to B" (correlation, a fitted line, a share by group).
  Today it exports rows into pandas, or writes `sum(x * x)` by hand —
  the formula that cancels (measured below).  It needs the functions to
  exist under the names it was trained on (PostgreSQL's and DuckDB's),
  one statement each, and to find them through the MCP server.
- **An AI application's developer** needs the same over logged prompts,
  outputs, scores and latencies: data quality (null and distinct rates,
  ranges, a column's type drifting), **drift** between two periods or two
  model versions (a statistic per feature and a test of whether two
  distributions differ: Kolmogorov–Smirnov, the population stability
  index, Jensen–Shannon divergence, χ² for categories — TensorFlow Data
  Validation's and Amazon Deequ's checks), evaluation metrics over logged
  outputs (precision and recall are counts with filters; ROC AUC is a
  rank sum, the Mann–Whitney U; calibration is a binned mean), latency
  percentiles (p50, p95, p99 — exact or within a stated error), and
  moving averages over time.  Today: a warehouse job or a notebook after
  an export; Deequ runs them as Spark aggregations (with HyperLogLog and
  KLL sketches).
- **Those who train models** need dataset statistics at scale (per
  feature moments and quantiles to normalize with — computed once, the
  same on every rerun), deduplication (exact by hash; near by MinHash),
  **reproducible** sampling and train / test splits (the same rows each
  run, whatever the thread count: a split by a stable hash of the key,
  not by `random()`), stratified samples, and statistics of embeddings
  (centroids, norms, a covariance) — with §M14.12.  Today: Spark,
  numpy over Parquet, DuckDB.

*Measured: what the usual methods give* (a standalone program, 10⁷
doubles, Apple M-series, one core; best of five).  The sample variance
of `1e9 + U(0,1)` (values on a large offset, as timestamps and ids are)
and of `N(0,1)`, against the exact value (power sums kept exactly and
rounded once; a two-pass exact computation agrees bit for bit;
`bench/variance/`):

| method | relative error, offset data | relative error, N(0,1) | distinct results, 20 runs of 8 workers | ns a value |
|---|---:|---:|---:|---:|
| sums of x and x² in doubles (ClickHouse's `varSamp`) | 1.1 × 10⁹ (meaningless) | 3 × 10⁻¹⁴ | — | 0.86 |
| Youngs–Cramer (PostgreSQL's `float8_accum` since 12) | 1.0 × 10⁻³ | 1.2 × 10⁻¹³ | — | 1.15 |
| Welford, merged by Chan et al. (DuckDB's `stddev`) | 9.1 × 10⁻¹⁰ | 1.2 × 10⁻¹³ | 20 and 19 | 4.7 |
| exact power sums, Shewchuk expansions (`fsum.rs`) | 0 | 0 | 1 | 17 and 60 |
| exact power sums, a superaccumulator (Neal 2015, scalar) | 0 | 0 | 1 | 8.9 |
| numpy `var(ddof=1)` (two passes, in memory) | — | — | — | 0.73 |

So: the textbook formula is unusable on real data; PostgreSQL's method
loses three digits on it; Welford is accurate but gives a different last
digit for every split of the rows among workers — 20 runs, 20 answers —
which breaks reproducibility (principle: results reproducible across
threads and runs); exact sums cost more, and how much depends on the
accumulator: Shewchuk's expansions grow with the spread of magnitudes
(x² as two doubles, 60 ns), a fixed superaccumulator does not (67
chunks of 32 bits in 64-bit integers, an addition three integer adds;
8.9 ns for Σx and Σx² together, 4.0 ns for one sum, scalar and
unvectorized).

*The function families, chosen, with their methods and bounds.*

1. **One exact accumulator for every floating-point aggregate.**  `sum`
   and `avg` of doubles are exact already (§M14.20 V4, Shewchuk); moments
   need the same of x², x³, x⁴ and xy, which a product's exact parts
   (`fma`: x·y = hi + lo exactly) feed.  Taken: Neal's superaccumulator
   ("Fast exact summation using small and large superaccumulators",
   2015) as the one exact accumulator (one mechanism per job: it replaces
   `ExactSum` in `sum` / `avg` once measured faster), its additions
   vectorized as a kernel (the chunks' index computed per lane, the
   integer adds scattered; ReproBLAS's binned sums, Demmel & Nguyen, are
   the fallback idea if the scatter does not vectorize — reproducible,
   not exact, and cheaper).  A result is computed from the exact sums and
   rounded once: the variance as Σ(x − m)² = Σx² − 2mΣx + n·m², exactly,
   for m the rounded mean, then divided — the same bits from any split,
   any order, row or column storage.  Integers and `numeric` keep exact
   integer sums (`i128`, then `numeric`), as PostgreSQL does.  The
   accumulator subtracts exactly too, so a sliding window frame removes
   the row leaving it without drift.
2. **Moments**: `var_samp` / `variance`, `var_pop`, `stddev_samp` /
   `stddev`, `stddev_pop` (PostgreSQL's names), `skewness`, `kurtosis`
   (DuckDB's; sample-corrected, as its and Excel's are), z-scores and
   outliers through window functions (item 5).  Bound: the exact value
   rounded once, then the division and square root rounded (≤ 1.5 ulp).
3. **Two variables**: `corr`, `covar_pop`, `covar_samp`, `regr_slope`,
   `regr_intercept`, `regr_r2`, `regr_count`, `regr_avgx`, `regr_avgy`,
   `regr_sxx`, `regr_syy`, `regr_sxy` (PostgreSQL's, the SQL standard's),
   from exact Σx, Σy, Σx², Σy², Σxy.
4. **Quantiles, exact by default**: `percentile_cont(f) WITHIN GROUP
   (ORDER BY x)` and `percentile_disc` (one fraction or an array of
   them), `median(x)`, `mode() WITHIN GROUP …` — ordered-set aggregates,
   which nucleus's grammar lacks today.  Not by sorting everything: by
   selection — Floyd–Rivest in memory, and past a memory bound a radix
   selection over the values' order-preserving keys (the key encoding
   every type already has): one pass counts the top 16 bits' buckets, the
   next keeps only the bucket holding the wanted rank, so an exact
   median of 10⁹ rows needs two or three passes over the column and a
   bucket's rows of memory.  `histogram` (equi-width, or by
   `width_bucket`, PostgreSQL's) and equi-depth by quantiles.
5. **Window functions**, a prerequisite nucleus does not have: `OVER
   (PARTITION BY … ORDER BY … frame)`, every aggregate as a window
   aggregate, `row_number`, `rank`, `dense_rank`, `percent_rank`,
   `cume_dist`, `ntile`, `lag`, `lead`, `first_value`, `last_value`,
   `nth_value`.  Taken: Leis et al., "Efficient Processing of Window
   Functions in Analytical SQL Queries" (VLDB 2015, HyPer; DuckDB's
   segment trees) for frames over any aggregate, and the exact
   accumulator's subtraction for sliding sums and moments in O(1) a row.
6. **Approximate, only by name and only where exactness costs memory**,
   each sketch chosen for a merge that does not depend on order, so the
   approximation is the same in every run:
   - `approx_count_distinct(x)`: HyperLogLog++ (Heule, Nunkesser, Hall
     2013: the sparse form for small counts, bias correction), precision
     14 — 16 KiB, a standard error of 0.81 %; registers merged by
     maximum.  The hash is a stable one with a fixed seed, versioned as
     a format (`fasthash` is seeded per process, so it cannot be).
   - `approx_percentile(x, f)`: DDSketch (Masson, Rim, Lee, VLDB 2019):
     logarithmic buckets, every quantile within a relative error α of
     the true value (1 % by default), merged by adding counts.  Not
     taken as the default: t-digest (Dunning) and KLL (Karnin, Lang,
     Liberty 2016; Apache DataSketches, Deequ) — their compactions depend
     on the order of merges, so the answer moves with the thread count;
     ClickHouse's `quantile`, a reservoir of 8192, is not even the same
     between two runs.  DDSketch's error is relative to the value, so on
     data far from zero (timestamps) the exact quantile of item 4 is the
     one to use; the documentation says so.
   - `approx_top_k(x, k)`: Space-Saving (Metwally, Agrawal, El Abbadi
     2005), mergeable (Agarwal et al., "Mergeable Summaries", 2012), ties
     broken by value so the merge is deterministic; each count with its
     error bound.
   - Sketches as values: `hll` and `ddsketch` types (registered, as
     principle 12 wants) with `…_agg`, `…_merge` and their readers, so a
     rollup table can keep a day's sketch and a month be merged from
     them.
7. **Sampling and splits**: `TABLESAMPLE BERNOULLI (p) | SYSTEM (p)
   [REPEATABLE (seed)]` (PostgreSQL's; `SYSTEM` by morsels of the
   storage); `reservoir_sample(x, n)` (Li's Algorithm L, which `ANALYZE`
   already uses); `hash_unit(key [, seed])`, a stable hash of a value
   into [0, 1), so `WHERE hash_unit(id) < 0.8` is the same training set
   on every run, version and machine.  Near-duplicates (MinHash) wait for
   §M14.21's analyzers.
8. **Drift and tests**: `ks_test(x, side)` — the two-sample
   Kolmogorov–Smirnov statistic and its p-value, `side` telling the two
   samples apart (exact through item 4's sort, or over two DDSketches) —,
   `psi(x, side, bins)`, `js_divergence`, `chisq_test(category, side)`;
   ROC AUC as `auc(score, label)` (the rank sum).  Each named as scipy
   names its counterpart where one exists, so a model reaches for the
   right one.
9. **`SUMMARIZE t`** (DuckDB's statement) and an MCP tool of the same:
   one scan of every column — count, `NULL`s, approximate distinct,
   minimum, maximum, mean, standard deviation, the quartiles — the first
   thing an agent asks of a table it has not seen; `docs/llm.md` lists
   the families with an example each.
10. **Vectors** (§M14.12): `avg` and `sum` of vectors (centroids,
    exactly, the accumulator per component), norms' statistics, and a
    covariance of embeddings — a Σ xxᵀ, a matrix product (BLAS's
    `syrk`), which is the one place a BLAS-style blocked kernel is worth
    writing.  Principal components and clustering stay out of the engine
    (export through columnar batches, measured at 147 ms a million rows).

*The fit with the principles.*  Exact by default (items 1–5); approximate
only under `approx_…` names, each with a stated bound and a merge that
is the same in any order (item 6); reproducible bit for bit across
threads, morsels, row and column storage — tested as a law (an
aggregate over any split of its rows equals it over one); memory bounded
(selection with spilling, fixed-size sketches); every function a
registered aggregate or function, the sketches registered types, the
kernels type capabilities (principle 12); names from PostgreSQL first,
DuckDB second (principle 11's "familiar names, our own design").

*Benchmarks to run* (each step against the one before, and against
PostgreSQL 18 and numpy on the same data): accuracy on adversarial data
(offsets, wide dynamic ranges, ill-conditioned regression) against
Python's `fractions`; the split law at 1, 2, 8 and 32 workers; TPC-H's
`lineitem` (`stddev`, `corr`, `percentile_cont` by group); ClickBench's
`hits` (`approx_count_distinct(UserID)`, latency-like percentiles); the
time a value of each accumulator; memory of the exact median of 10⁸
rows under a 64 MiB bound.

*Steps*, each its own commit, measured, in this order — **to be agreed
before they are built**:

- **S1 — the exact accumulator** ✅ (2026-11-06; the steps agreed: "legyen
  meg az S, L és V lépések"): the superaccumulator as a vectorized
  kernel, `sum` / `avg` of doubles on it (replacing `ExactSum` where
  faster), the split law in the test kit.
  *Built:* `fsum::ExactSum` is Neal's small superaccumulator — one
  exact accumulator, Shewchuk's partials gone (their routine is kept only
  as the tests' oracle).  Two departures from Neal's, each measured or
  forced: (1) only the chunks a sum has touched are kept, a window grown
  as values come — Neal's 67 chunks are 536 bytes, a state a group, and
  a grouping of a million groups would carry half a gigabyte; values of
  like magnitude touch three or four; (2) the result is rounded from the
  integer itself — the chunks made non-negative digits, the top 53 bits,
  the next bit and a sticky bit, the exponent field read off the bit
  position — not by summing the chunks as doubles, which overflow at
  the top of the range where the sum does not (`[MAX, MAX, −MAX]`).
  Subtraction (a window frame's leaving row, S4) and `add_product` (x·y
  as its rounded product and the product's error from a fused
  multiply-add, for S2's Σx², Σxy) are the same three integer additions.
  A worker's state passes as bytes (the window, its place), not as
  doubles.  Tests: Shewchuk's summation as the oracle over 2000 random
  sets of any finite doubles (subnormals, the largest, cancellations,
  ties), an integer reference, splits and merges both ways, and — in
  `crates/sql/tests/vector.rs` — the split law: `sum` and `avg` of 50,000
  doubles spread over 2⁴⁰ of magnitude, ungrouped and by group, the same
  bits as an integer reference rounded once at 1, 2, 8 and 32 workers
  and batch sizes 0, 3 and 1024; three mutations of the rounding, each
  caught (one only after a test with its sticky bit in the lowest chunk
  was added).
  *Measured* (`bench/stats/aggs.py`, 10M rows, alternating builds, best
  of five): `sum` of doubles 217 / 183 → 171 / 189 ms, `avg` of values
  spread over 30 decades 219 / 207 → 171 / 185, a sum by 1000 groups 197 /
  209 → 197 / 197, an `avg` by a million groups 2278 / 2486 → 2369 / 2486
  — the same answers, the same time: the scan, not the accumulator, is
  the cost.  *Not taken:* a vectorized kernel for the additions (the
  scatter of three adds a lane) — nothing to gain while the accumulator
  is not what a query waits on; to be measured again under S2's moments,
  which add four sums a value.  *Correction (S2):* the million groups
  were slower — measured again against S1's own build and Shewchuk's,
  2368 / 2678 → 3199 ms; two rounds' noise had hidden it.  S2 fixed it.
- **S2 — moments** ✅ (2026-11-06): item 2 over doubles, integers and
  `numeric`.
  *Built:* `moments` — `var_samp` / `variance`, `var_pop`, `stddev_samp`
  / `stddev`, `stddev_pop`, `skewness` / `skew`, `kurtosis` — one
  vectorized, combinable `GroupAccumulator` whose state is the row count
  and the power sums, so any split gives the same bits.  Over doubles
  Σx … Σx⁴ are `ExactSum`s, each power added exactly as its rounded value
  and its error by fused multiply-adds; the central sums C₂ = nΣx² −
  (Σx)², C₃, C₄ are then exact integers (`bigint::BigInt`, built only at
  the end) and the result is rounded once from them: the variance from
  the integer quotient and its remainder, the standard deviation its
  root (≤ 1 ulp more), skewness and kurtosis from exact numerators.  Over
  integers and decimals the variance and deviation are PostgreSQL's
  digits: Σx, Σx² exact in `bigint::Wide<8>` (no allocation a row) and
  PostgreSQL's formula and scale.  `ExactSum`'s state passed between
  workers shrank and is merged without copies: chunks carried (only
  when the top needs a chunk above it), each but the top in four bytes,
  the zeros at either end left out, added straight from the bytes.
  Tests: `46_moments.slt` (expectations from PostgreSQL 18 and DuckDB,
  in every mode), the split law for every moment at 1–32 workers and
  three batch sizes against a `BigInt` reference, the state through its
  bytes (negative, cancelled, at the range's top, `NaN`), and the
  quotient's rounding mutated (caught only after a test whose remainder
  decides it was added).
  *Prior art:* PostgreSQL's `numeric_stddev_internal` and
  `select_div_scale` — taken for integers and decimals, so the digits
  are PostgreSQL's (nucleus's `numeric` stops at 38 digits: there 22003).
  PostgreSQL's doubles (Youngs–Cramer since 12) and Welford's update —
  not taken: a division a row, rounded a row, the answer depending on
  the order; PostgreSQL's variance of the offset column differs between
  runs (0.0833333256511713, 0.0833333257012356) and from the exact
  0.08333332757266079 at the ninth digit.  DuckDB's skewness and
  kurtosis — definitions and NULL rules taken; its computation from
  double power sums not (it loses digits on offset data).  numpy's
  two-pass variance (mean, then pairwise-summed squares: 0.08333332757266525,
  8 ms on one core over a contiguous array) — the speed of an array in
  memory, not of a table scan; the target for a column table's kernels
  later, not this step.
  *Measured* (`bench/stats/aggs.py`, 10M rows, builds alternating with
  S1's, best of five, two rounds; PostgreSQL 18 with its parallel
  workers): `variance` 189 / 194 ms (PostgreSQL 163), `stddev` by 1000
  groups 225 / 225 (350), `variance` of integers 174 / 162 (243),
  `skewness` and `kurtosis` 307 / 314; `avg` by a million groups 3068 /
  3166 → 2089 / 1985 (PostgreSQL 2351), `sum` 207 / 237 → 197 / 197.
  `bench/phases.py` against S1's build, two rounds: ClickBench (column)
  geometric mean 19.8 / 19.8 → 19.3 / 18.9 ms, TPC-H (row) 240.5 / 232.2
  → 225.5 / 231.9 ms; Q27's 42 → 47 ms, run alone three times
  alternating, was noise (47 / 43 / 43 → 42 / 39 / 39).
- **S3 — two variables** ✅ (2026-11-06): item 3.
  *Built:* `regression` — `corr`, `covar_pop`, `covar_samp`,
  `regr_slope`, `regr_intercept`, `regr_r2`, `regr_count`, `regr_avgx`,
  `regr_avgy`, `regr_sxx`, `regr_syy`, `regr_sxy`, PostgreSQL's
  (arguments `(y, x)`, doubles, its `NULL` / `NaN` / horizontal- and
  vertical-line rules, checked against PostgreSQL 18 case by case).  A
  group keeps n and only the sums its result needs of Σx, Σy, Σx², Σy²,
  Σxy; Cxx, Cyy, Cxy are exact integers and each result one exact ratio
  rounded once, `corr` the exact root of Cxy²/(CxxCyy) rounded once.  On
  the way: `ExactSum::add_product` adds the significands' 106-bit
  product in one step (five chunks) instead of a fused multiply-add's
  two terms — exact down to 2⁻⁹⁶⁹ rather than 2⁻¹⁰²¹, and one addition a
  product instead of two (the moments' x² and x⁴ too); `BigInt::div_big`
  is Knuth's long division instead of a bit at a time (a ratio of two
  300-bit integers a group, a million groups); the integers of a batch
  are read from their vector, not converted a `Value` at a time.
  Tests: `47_regression.slt` in every mode; the split law for all twelve
  at 1–32 workers and three batch sizes, the slope against a `BigInt`
  reference; the long division against its identity over 3000 random
  operands (the add-back step mutated: caught); ratios and roots at a
  tie and just above it (the root's sticky bit mutated: caught); random
  products against fused multiply-adds (the product's top chunk
  mutated: caught).
  *Prior art:* PostgreSQL's `float8_regr_accum` and its finals — the
  semantics taken, the computation not: its Youngs–Cramer update rounds
  a row at a time, and on the benchmark's `corr(x, id)` (x near 10⁹,
  spread 1) it answered −1.667e−6 and −3.056e−6 in two runs (its
  workers' order) where the exact value is −3.459e−6 (numpy's two-pass
  `corrcoef` −3.4589588653e−6, 11 digits); on four
  rows near 10⁹ its slope is wrong from the seventh digit, and its
  correlations of small data are an ulp off.  Kulisch's long accumulator
  (an exact dot product: significands multiplied in integers, added to a
  fixed-point register) — taken for `add_product`, in Neal's windowed
  chunks.  Knuth's Algorithm D (TAOCP 4.3.1, as Hacker's Delight's
  `divmnu`) — taken.
  *Measured* (`bench/stats/aggs.py`, 10M rows, builds alternating with
  S2's, best of five, two rounds; PostgreSQL 18 with its parallel
  workers; numpy one core): `corr(x, id)` 217 / 220 ms (PostgreSQL 173,
  numpy 37), slope and intercept by 1000 groups 277 / 281 (367); before
  the exact product and the integers read from their vector, 258 /
  345 (`ExactSum::put` was the top frame).  The moments with the new
  product: `variance` 207 / 210 → 214 / 214, `skewness` and `kurtosis`
  294 / 294 → 307 / 296 — the same within the noise (the scan is their
  cost: `sum` alone 164 / 160).  `bench/phases.py` against S2's build,
  two rounds: ClickBench (column) geometric mean 18.9 / 19.4 → 19.1 /
  18.8 ms, TPC-H (row) 234.0 / 226.1 → 228.8 / 227.4 ms; TPC-H Q15's
  307 → 344 ms, run alone three times alternating, was noise (best 288
  → 284).
- **S4 — window functions** ✅ (2026-11-06): item 5.
  *Built:* `OVER (PARTITION BY … ORDER BY … frame)` and `WINDOW`
  clauses (named windows refining one another, PostgreSQL's rules and
  errors), every aggregate as a window aggregate, `row_number`, `rank`,
  `dense_rank`, `percent_rank`, `cume_dist`, `ntile`, `lag`, `lead`,
  `first_value`, `last_value`, `nth_value`; frames `ROWS`, `RANGE` (with
  offsets over numbers, dates and times) and `GROUPS`, `EXCLUDE`.  The
  binder puts a `Sort` and a `Window` node per window (the calls sharing
  a partition and order), so the sort is the one `ORDER BY` uses
  (parallel over a pipeline, spilling past `work_mem`); an `ORDER BY`
  the last window's sort already gives is left out.  The `Window` node
  takes a partition at a time and evaluates each aggregate's frames by
  the cheapest way that fits: one state for the whole partition; one
  state grown row by row for frames from the partition's start, finished
  in place where the aggregate allows it (`finish_keeps_state`, new,
  true for every built-in but `string_agg` and `array_agg`), else a copy
  of it; up to 64 rows added straight to a fresh group; a segment tree
  of partial states for larger frames (`partial` / `combine`, so exact
  sums stay exact); a fresh state per distinct frame for aggregates
  whose states do not combine.  A partition is held in memory (spilling
  one is left for later: no workload here has asked for it).
  Tests: `48_window.slt` (every function, frame kind, exclusion, named
  windows, windows over groups, the errors) with PostgreSQL 18's
  expectations generated from it; every frame kind × eight bound pairs ×
  four exclusions against a brute-force definition, with rows, batches
  of 3, 8 workers and 64 kB of `work_mem` (the tree's query, the running
  state, an `EXCLUDE TIES` cut and a `ROWS` bound mutated: each caught);
  5000-row partitions past the scratch groups' renewal; the running
  state against the cut-frame paths for nineteen aggregates (`min`'s
  finish made destructive again: caught).
  *Prior art:* Leis, Kundhikanti, Kemper, Neumann, "Efficient Processing
  of Window Functions in Analytical SQL Queries" (VLDB 2015, HyPer) —
  taken: the segment tree of partial aggregates for arbitrary frames,
  the per-partition evaluation after one sort a window, and sharing that
  sort between calls of one window; DuckDB's `WindowSegmentTree` (the
  same paper) the same way, with its constant-aggregate and running
  shortcuts.  Not taken: HyPer's partition-parallel sort (nucleus's
  `Sort` already runs in parallel over a pipeline, and a partition's
  evaluation stays on one thread for now).  PostgreSQL's
  `nodeWindowAgg.c` — taken: its semantics, errors and default frames,
  and its read-only final functions (`finalfunc_modify = READ_ONLY`) as
  `finish_keeps_state`; not taken: its inverse transition functions
  (moving-aggregate `msfunc`/`minvfunc`), which for floats accumulate
  rounding — of 200,000 ten-row moving averages of the benchmark's `x`
  it rounds 98,995 to another double than the exact average's (up to 2
  ulps), and its running sum over 10M rows ends at 1.00000000045036e16
  where the exact sum is 1.0000000004999998e16.  The exact accumulator's
  subtraction would give an O(1) sliding sum without that error, and is
  the step to take if a moving frame's cost asks for it; the segment
  tree and direct feeding of small frames cover it now.  Found on the way: nucleus's own `avg` of doubles divided the
  correctly rounded sum — two roundings, 79,983 of those 200,000
  averages off by an ulp —, now `ExactSum::mean`: the exact sum divided
  by the count in integers and rounded once (none off; `avg(x)` of
  `[1e16, 1, 8]` is 3333333333333336.5, PostgreSQL's
  3333333333333336).
  *Measured* (`bench/stats/aggs.py`, its five window queries, best of
  five): at 1M rows, the first version — its own row sort inside the
  node, keys copied into every row, a fresh group per frame — took
  row_number by groups 901 ms, top 3 884, running sum 1005, moving
  average 1716, lag 695 (PostgreSQL 398, 312, 233, 875, 219).  The
  `Sort` node below instead: 628 / 615 / 882 / 1417 / 590; keys read in
  place, frames as fixed ranges, `RANGE` bounds by binary search (the
  first version built an index vector per row: O(n²)): 531 / 484 / 754
  / 1333 / 497; small frames fed directly and the running state
  finished in place: 577 / 514 / 632 / 809 / 518 (the first two the same
  within the noise).  At 10M rows, one round: 5662 / 5234 / 8484 / 8102 /
  6901 ms, PostgreSQL 4537 / 3883 / 2585 / 7936 / 2411.  Where the time
  goes: the sort — 10M rows by `id` alone 3.8 s, by `(g, x)` 5.1 s,
  PostgreSQL's 1.6 and 1.9 s with 4 MB of `work_mem` — and PostgreSQL
  reads `ORDER BY id` from the primary key's index (its plan has no
  sort); then a partition of 10M rows held as rows (about 2 GB, walked
  several times: the window's own cost per row grows from about 130 ns
  at 1M rows to about 390 at 10M).  Against S3's build (`bench/stats/aggs.py`, 10M rows, two rounds
  alternating): the ten aggregates the same within the noise (`avg` by
  a million groups, now rounded once, 2130 / 2032 → 2113 / 2018 ms);
  `bench/phases.py` two rounds: ClickBench (column) geometric mean
  19.1 / 19.0 → 19.4 / 19.5 ms — Q32 437 → 458, run alone three times
  alternating: best 422 / 424 / 437 → 416 / 418 / 424, noise —, TPC-H
  (row) 227.3 / 228.0 → 224.6 / 218.9 ms.
  *Next, recorded here:* (1) the sort itself, 2.3–2.7× PostgreSQL's on
  the same rows (every `ORDER BY` gains); (2) orders the input already
  has — a scan in its key's order, an index — so a `Sort` is left out
  (the plan has no notion of a provided order yet; the final `ORDER BY`
  matching the last window is the one case handled); (3) a partition
  streamed rather than held, for the functions and frames that look a
  bounded way ahead (`row_number`, ranks, `lag` / `lead`, running and
  `ROWS` frames), as PostgreSQL's tuplestore does.
- **S4·sort — the sort, and orders a plan already has** ✅ (2026-11-06):
  S4's next steps (1) and (2).
  *Built:* one sorter (`sorter.rs`) for every full sort — `ORDER BY`,
  a window's sort, the spilled sorts of `DISTINCT` and set operations —
  in place of the keyed-row sort and its heap merge.  A row becomes one
  record: a normalized key (per key a null byte placed by `NULLS
  FIRST / LAST`, then the value's order-preserving encoding, the bytes
  inverted for `DESC`), the key values, the row.  Keys of one type
  whose encoding orders as the type does (booleans, integers, floats,
  numerics, text, `char`, dates, times, timestamps, `bytea`) compare as
  bytes, an 8-byte abbreviated prefix first; other or mixed keys
  compare as values.  Each worker sorts chunks of `work_mem` / 2 a
  worker stably; chunks stay in memory while the statement's budget
  holds them and go to a scratch run past it, with an index entry every
  128 records.  The merge splits the key space into ranges by
  splitters sampled from the parts, finds each range's start in every
  part by binary search (in memory, or through a run's index), and
  merges the ranges on the workers into ordered slots read in turn,
  a bounded number ahead; small inputs merge on one thread, and more
  than 512 parts are merged a group at a time first.  Ties go to the
  earlier part, so the sort is stable.
  Orders a plan has: a `Sort` whose keys are an ascending prefix of the
  primary key, over a scan of an unpartitioned table in key order
  (through filters, projections, row security), is left out — a
  table's scan returns rows in primary-key order in row and column
  storage alike, within a transaction too.  An identity projection
  left behind is dropped.  Two things that made a sort's absence
  visible: an unfiltered chain ran on one thread (it now runs on the
  workers unless a `LIMIT` of at most 65,536 rows stops it early), and
  a `LIMIT`'s bound reached only its input (it now reaches the scan
  through projections and filters, so `ORDER BY id LIMIT 3` reads
  three rows).
  Tests: the sorter against a stable sort by `spill::compare` — 36
  rounds × four direction / null orders × five settings (memory only,
  spilling, one to eight workers, one to many parts), keys of every
  normalizable kind with NaN, ±0 and infinities, numerics, intervals,
  mixed integers and floats (a flipped tie and a run's range start
  mutated: both caught); 812 parts merged a group at a time; enum keys
  and arrays through the scratch runs.  `sort.rs`: 30,000 rows in five
  settings, stable against a reference; the `Sort` count of twelve
  plans and their order in row and column storage, inside a
  transaction with inserts, updates and deletes.
  *Prior art:* normalized keys compared as bytes — System R's and
  DuckDB's (`RowLayout`, its sort keys): taken, with values kept for
  the kinds whose encoding does not order; PostgreSQL's abbreviated
  keys: taken as the 8-byte prefix; PostgreSQL's external merge and
  Gather Merge: their spill-past-`work_mem` and their single merge
  thread not taken — the merge is split by key ranges as in Merge Path
  (Odeh, Green, Mwassi, Shmueli, Birk, IPDPS 2012) and DuckDB's
  parallel merge sort, here by sampled splitters rather than exact
  co-ranks, which a scratch run's sparse index makes cheap.  Orders a
  plan has: PostgreSQL's pathkeys and DuckDB's / Umbra's order
  properties — taken only the case that pays now (a primary key's
  scan); indexes, merge joins and group orders are left for when a
  workload asks.
  *Measured* (10M rows of `bench/stats/aggs.py`'s table, best of
  three, against S4's build; PostgreSQL 18 with 4 MB `work_mem`):
  `ORDER BY id` read whole 3830 → 867 ms (the sorter alone 1567;
  PostgreSQL 1650, from its index); `ORDER BY g, x` 5140 → 1800
  (PostgreSQL 1890); `ORDER BY x DESC` 1530; `ORDER BY id LIMIT 3` 195
  → 0.1 ms, `… OFFSET 5000000` 212; an unfiltered scan read whole 2429
  → 840.  S4's window queries, 10M rows, best of two rounds: 5662 /
  5234 / 8484 / 8102 / 6901 → 2534 / 2207 / 3447 / 5242 / 2578 ms
  (PostgreSQL 4507 / 3880 / 2570 / 7924 / 2407); the running sum and
  `lag` are left behind PostgreSQL's by the 10M-row partition held in
  memory (next).  `bench/run.py`, 1M rows: the sort 279 → 82 ms
  (PostgreSQL 155).  `bench/phases.py` two rounds alternating:
  ClickBench (column) geometric mean 19.3 / 18.9 → 17.4 / 17.7 ms (Q32
  413 → 306, Q18 166 → 129, Q39 228 → 185; all three group by many
  keys, the cause not isolated), TPC-H (row) 228.6 / 230.1 → 230.5 / 231.2, the same
  within the noise.
  *Next, recorded here:* S4's (3), a partition streamed rather than
  held; a sort's records narrower than the row (only the columns read
  above it); a sort's scratch runs shared by the workers rather than
  one file a part.
- **S5 — ordered sets** ✅ (2026-11-06): `WITHIN GROUP`, item 4's
  exact quantiles with spilling selection, `histogram`, `width_bucket`.
  *Built:* `percentile_cont`, `percentile_disc` (a fraction or an array
  of them) and `mode()` `WITHIN GROUP (ORDER BY x [DESC])`, and
  `median(x)`; PostgreSQL's semantics, results and errors (42809, 0A000
  for `OVER`, 22003 for a fraction outside `[0, 1]` — a constant one
  checked when bound, so even over no rows, as PostgreSQL's final
  function does), its interpolation with fused multiply-adds (its arm64
  build's bits).  A group's values are kept as order-preserving 8-byte
  keys (integers, booleans, dates, times, doubles) or as values (any
  other ordered type); its result is found by selection at the ranks
  wanted, each above the last (`select_nth_unstable`), `mode` by a sort
  and runs.  States combine — a partial state is the fraction and the
  values, keys as bytes — so values are collected on every worker.  Past
  `work_mem` keys go to a scratch run of (group, key) records, read
  back radix-partitioned: groups packed whole into partitions of half
  of `work_mem`, a larger group split into key ranges by a count over
  2¹⁶ slices of its range (a slice of one key kept as a count), so each
  rank is selected in the one partition holding it — every key written
  twice and read three times, whatever the size; other types go to the
  sorter.  `width_bucket` — PostgreSQL's over doubles step for step,
  exact over decimals (wide integers), and by an array of lower bounds
  of any ordered type — and `histogram(x, low, high, count)` /
  `histogram(x, bounds)`, its counts as a `bigint[]` (combinable).
  On the way, in the vectorized executor: an expression of constants
  (an operator, cast or immutable call of constant operands, an
  `ARRAY[…]` of literals) is evaluated once per batch, not per row —
  `histogram(x, 0.0, 1.0, 10)`'s coerced bounds and `ARRAY[0.5, 0.95,
  0.99]` had cost as much as the aggregate itself.
  Tests: `49_ordered_sets.slt` (expectations from PostgreSQL 18, in all
  four executors), `ordered_sets.rs` — 30,000 rows, 23 groups, against
  the definitions (sorted values, ranks read off) in five settings,
  `work_mem` 64 kB among them, NaN, infinities and NULLs; selection at
  every rank, key order as SQL's, the radix partitions against a sort
  (two mutations of the partitioning — a single-key slice's key and the
  shift's condition — caught, as were a spilled rank offset and an
  in-memory selection skip).
  *Prior art:* PostgreSQL's `orderedsetaggs.c` — taken: the semantics,
  the errors, the fraction checked first; not taken: a tuplesort per
  group (a full sort for a few ranks, one thread).  DuckDB's quantile
  aggregates — taken: `median` and its typing, selection (`nth_element`)
  rather than a sort in memory.  Selection: Rust's introselect
  (quickselect with median-of-medians fallback, O(n) worst case) chosen
  over Floyd–Rivest, whose sampling wins on large in-memory arrays by
  a constant only, and over a heap.  Radix selection over
  order-preserving keys (as GPU and external-memory quantile selection
  do, e.g. Alabi et al., "Fast k-selection algorithms for GPUs", 2012):
  taken for the spill, its two-pass histogram bounding the I/O where
  an external merge sort's passes grow with the input.  Approximate
  quantiles (t-digest, KLL) are S6's.
  *Measured* (10M rows of `bench/stats/aggs.py`'s table, best of two,
  `work_mem` 64 MB; PostgreSQL 18 with 4 MB): `median` 613 ms
  (PostgreSQL 2249, numpy one core 26), `p50, p95, p99` 668 (2240),
  `percentile_disc` 618 (2374; numpy 44), `median` of 1000 groups 615
  (550: PostgreSQL's 4 MB is per group, nucleus's `work_mem` per
  accumulator; 499 with 1 GB), `mode` 736 (1333), `histogram` 167 (`sum`
  185).  The first, non-combinable design (one thread, through the
  sorter): 5388 / 8444 / 5264 / 4268 / 3360 / 416.  10⁸ values of
  `generate_series` to one exact median: 23.0 s and 216 MB peak against
  `sum`'s 18.0 s and 146 MB (`work_mem` 16 MB: 19.5 s, 168 MB) — memory
  bounded by `work_mem`.  `bench/phases.py` two rounds alternating
  against S4·sort's build: ClickBench (column) geometric mean 17.9 /
  18.5 → 19.4 / 18.7 ms, its two outliers (Q22 7.4 → 10.9, Q10 5.2 →
  6.5) measured again seven runs × three rounds alternating — Q22 6.4–6.9
  → 6.5–6.8, Q10 5.4–5.5 → 5.1–5.2, Q23 and Q34 the same: noise; TPC-H
  (row) 237.8 / 232.7 → 227.7 / 230.5, the same within the noise.
  *Next, recorded here:* a median over a sliding window frame keeps
  segment-tree value bags (an order-statistic tree is the step); a
  single-key-range slice larger than a partition is loaded whole; an
  out-of-order read of a spilled sort restarts it; a spilled
  accumulator's partial state is built in memory (workers' runs could
  be handed over instead); `generate_series` runs on one thread at
  ~180 ns a row.
- **S6 — sketches** ✅ (2026-11-06): item 6, the sketch types included.
  *Built:* `stablehash` — xxHash64, seed 0, of a value's key encoding
  (equal values hash alike in any form; integers and doubles from their
  encoding built on the stack), a format the sketches store.
  `approx_count_distinct(x)` — HyperLogLog++: sparse, the sorted set of
  25-bit prefixes with their ranks (buffered, merged in batches), up to
  4096 of them, then 2¹⁴ one-byte registers; read by Ertl's improved
  estimator.  `approx_percentile(x, f)` / `approx_quantile` — DDSketch,
  α = 1 %, its cubically interpolated mapping, 2048 buckets a sign
  collapsing the lowest, zero, ±∞ and NaN counted apart.
  `approx_top_k(x, k)`, `approx_top_k_counts(x, k)` — Misra–Gries in
  the batched, mergeable form.  The types `hll` and `ddsketch`
  (registered; equality and hashing on canonical bytes, text form hex,
  casts with `bytea`, versioned formats), `hll_agg` / `hll_merge` /
  `hll_count` (`hll_cardinality`), `ddsketch_agg(x [, α])` /
  `ddsketch_merge` / `ddsketch_quantile` / `ddsketch_count`.  Every
  sketch combines, so it aggregates on the workers.
  Tests: xxHash64 against the reference's vectors, the quick forms
  against the key encoding; HLL within its error from 0 to 10⁶
  distinct, the same bytes from any split and merge order (a sparse
  merge turning dense part way — a bug the first SQL run showed, its
  test catching the old code); DDSketch within α at α = 0.1 %, 1 %, 5 %
  over its reach, its mapping monotone with every bucket at most `γ`
  wide, the same bytes from any order with collapsing, SQL's order of
  the edges (two mutations — the negative buckets' order, the fold —
  caught); top-k within its bound on a skewed stream, exact in any
  order with few values.  `sketches.rs`: the same bytes in five
  settings (a row at a time, batches, four workers, `work_mem` 64 kB,
  one thread), merged sketches equal to the whole table's, bounds
  against the exact answers; `50_sketches.slt` (the formats' text form
  among its expectations).
  *Prior art:* HyperLogLog++ (Heule, Nunkesser, Hall, EDBT 2013) —
  taken: precision 14, the sparse form at precision 25 and its ranks;
  not taken: its empirical bias tables and linear counting, replaced by
  Ertl's estimator (2017; Redis uses it), which needs neither and
  computes with `+ * / √` only — the same bits on every machine; the
  sparse form's varint compression (not yet: 4 bytes an entry).
  DDSketch (Masson, Rim, Lee, VLDB 2019; DataDog's `sketches-java`) —
  taken: the relative-error buckets, the cubically interpolated mapping
  (no `ln`, whose last bit is the platform's), the collapsing-lowest
  store of 2048 buckets; `ln γ` by its series and the inverse by
  Newton's method in fixed steps, to keep every library function out.
  Not taken: t-digest and KLL (their compactions depend on merge order),
  ClickHouse's sampled `quantile` (differs between runs).  Frequent
  items: Misra–Gries (1982) / Space-Saving (Metwally et al. 2005) in
  Agarwal et al.'s mergeable form ("Mergeable Summaries", PODS 2012) —
  taken, batched (reduce at `2C`); recorded where it falls short of the
  study's line: no summary that returns the values themselves merges
  the same in any order, so past `2C` distinct values its counts depend
  on where reductions fell, and it is not `order_free` (its morsels
  merge in their order: the same every run of a plan, not across worker
  counts).  The hash: xxHash64 (Collet) for its specification and test
  vectors in every language, over MurmurHash3 (DataSketches') and
  wyhash; hashing the key encoding rather than a value's own bytes
  follows the principle that one encoding defines equality.
  *Measured* (10M rows of `bench/stats/aggs.py`'s table, best of five,
  the better of two rounds): `approx_count_distinct(id)` 172 ms against
  `count(DISTINCT id)` 941 (PostgreSQL 1126), +0.30 %; by 1000 groups
  352 against 1658 (PostgreSQL 878), 9937–9973 for 10,000;
  `approx_percentile(y, [0.5, 0.95, 0.99])` 207 against
  `percentile_cont`'s 747 (PostgreSQL 2240), −0.04 % / +0.83 %;
  `approx_top_k(id % 1000, 3)` 248 against `mode`'s 757.  On `x`
  (1e9 + a fraction) every value falls in one bucket — 1 % of 1e9 —
  as the study warned: the documentation sends such data to
  `percentile_cont`.  `bench/phases.py` two rounds alternating
  against S5's build: ClickBench (column) geometric mean 17.7 / 17.9 →
  17.6 / 18.5 ms, TPC-H (row) 230.5 / 233.5 → 230.5 / 229.3 — the same
  within the noise (the step only registers functions and types).
  *Next, recorded here:* `count(DISTINCT)` by many groups is twice
  PostgreSQL's time (its own step: a distinct aggregate's per-group
  sets); a sketch's `group_bytes` is a guess (256), where a dense HLL
  takes 16 KiB — many groups' dense sketches are not yet counted against
  `work_mem` as they are; the sparse form's entries as varint deltas;
  precision other than 14 (the format has its byte); the counts' error
  bounds as values (a record type).
- **S7 — sampling and splits** ✅ (2026-11-06): item 7.
  *Built:* `TABLESAMPLE BERNOULLI (p) | SYSTEM (p) [REPEATABLE (seed)]`
  after a table in `FROM` — the binder puts a filter over the scan, so a
  sample keeps everything a scan does (columns, predicates pushed down,
  partitions, workers).  With `REPEATABLE`, `__tablesample_repeatable`:
  a row is in when `stablehash` of its primary key's key encoding (all
  its columns without a key), seeded by the seed's bits, falls below
  `p / 100` of the hash space — the same rows in every executor, worker
  count, run, version and machine, in row and column storage alike, and
  a smaller `p`'s sample within a larger one's.  Without it,
  `__tablesample`: a draw from a per-thread SplitMix64 seeded from the
  database's random source.  `hash_unit(key [, seed])`, that hash as a
  double in `[0, 1)` — a stable train/test split.  `random()`
  (PostgreSQL's), from the same generator.  `reservoir_sample(x, n [,
  seed])` — Algorithm L, the reservoir `ANALYZE` already drew with,
  moved to `sample::Reservoir` and shared (one mechanism); per-worker
  reservoirs merge by drawing, without replacement and by the rows each
  saw, how many values each gives (Lemire's unbiased bounded draw), out
  of their shuffled samples.  `ScalarFunction::parallel_safe` (default:
  unless volatile): the two draws are volatile but their calls
  independent, so a sampled scan runs on every CPU — it ran on one
  before (1024 ms, now 196).  PostgreSQL's error codes (`2202H`,
  `2202G`, `42704`, `42809`).
  Tests: `sample.rs` — the same `REPEATABLE` rows in five settings and
  both storages, `SYSTEM` = `BERNOULLI`, 1 % within 10 %, a keyless
  table's duplicate rows in or out together, another seed another
  sample; `hash_unit` splits alike everywhere and its halves add up;
  `reservoir_sample` the same each run of a plan, NULLs left out, all
  values when fewer than `n`.  Unit tests: every value equally likely
  in a merged reservoir as in one fed whole (three splits, 20,000
  seeds), Algorithm L uniform (moved with it), Lemire's draw unbiased,
  the hash pinned.  `51_sampling.slt`.
  *Prior art:* PostgreSQL's `TABLESAMPLE` (SQL:2003) — taken: the
  grammar, `REPEATABLE`, the error codes; not taken: `SYSTEM` by pages
  and both methods' per-block generator, whose sample changes with the
  storage layout, `VACUUM` and a dump and restore — a primary-key hash
  makes the sample a function of the rows, the property ML splits need
  (the "Repeatable Splitting" pattern of Lakshmanan, Robinson and Munn's
  *Machine Learning Design Patterns*, BigQuery's `FARM_FINGERPRINT(id)`
  modulo 10, and Géron's split by `crc32(id)` — here built in as
  `hash_unit`, over the key encoding so `1.0` and `1.00` split alike).
  DuckDB's `USING SAMPLE` — a reservoir for a count of rows, `SYSTEM` by
  vectors — not yet (below).  Reservoirs: Vitter's Algorithm R (1985, one draw a
  row) against Li's Algorithm L (1994, `n · (1 + ln(N / n))` draws) —
  L kept, as the study planned and `ANALYZE` used; a first version with
  R, written for integer-only draws the same on every machine, was
  replaced (two reservoirs for one job): L's gaps take `ln` and `exp`,
  so a seed's sample is the same on one platform and almost always on
  another, recorded.  Merging reservoirs by a hypergeometric draw:
  the distributed-sampling literature (e.g. Tirthapura and Woodruff,
  "Optimal random sampling from distributed streams revisited", DISC
  2011) — taken in its simplest exact form.  `PARALLEL SAFE` as its own
  property beside volatility: PostgreSQL's (where `random()` is
  `PARALLEL RESTRICTED` because of `setseed`, which nucleus does not
  have).
  *Measured* (10M rows of `bench/stats/aggs.py`'s table, best of five,
  the better of two rounds; `count(*), sum(x)` of the sample):
  `TABLESAMPLE BERNOULLI (10)` 196 ms (PostgreSQL 318), with
  `REPEATABLE (42)` 248 (263); `WHERE hash_unit(id) < 0.8` 399 (8M rows
  summed); `reservoir_sample(x, 1000)` 179; `ORDER BY random() LIMIT
  1000` 250 (636).  `bench/phases.py` two rounds alternating against
  S6's build: ClickBench (column) geometric mean 17.9 / 17.5 → 18.0 /
  17.7 ms, TPC-H (row) 235.7 / 230.4 → 237.3 / 230.3 — the same within
  the noise.
  *Next, recorded here:* `SYSTEM` that skips storage — whole column
  segments or morsels by their hash, which needs a block identity that
  survives compaction to stay repeatable; DuckDB's `USING SAMPLE n ROWS`
  (a fixed count, by reservoir in the plan); aggregate `FILTER (WHERE
  …)` is not parsed yet (the tests sum `CASE` instead) — a small
  grammar step; `reservoir_sample` order-free (by the values' hashes,
  a bottom-k sample, at the cost of a sample that depends on the values
  rather than the draws); `setseed`; `ln` and `exp` of our own (as
  DDSketch avoids them) would make Algorithm L's samples the same on
  every machine.
- **S8a — drift and tests** ✅ (2026-11-06): item 8 (S8 split in two:
  the tests, then `SUMMARIZE`).
  *Built:* `ks_test(x, side)`, `auc(score, label)`, `psi(x, side [,
  bins])` over the values in order; `js_divergence(x, side)`,
  `chisq_test(x, side)` over each value's counts; `side` a boolean,
  false the reference.  The first three keep each side as the ordered
  sets keep their values — `quantile::Doubles`, a thin face on S5's
  groups (8-byte keys, `work_mem`, the radix-partitioned spill, partial
  states from the workers): one store, not a second — and walk both
  sides once in ascending order, merged, the compared side's distinct
  values in memory up to half of `work_mem`, then in a scratch run.
  The KS statistic is found in integers (`max |a·n − b·m|`), its p-value
  exact up to 10,000 values a side by the lattice-path recurrence with
  the outside counted directly, Stephens' asymptotic form past that;
  the AUC is twice the Mann–Whitney U in integers, divided once; PSI's
  bins are the reference's equal-frequency ranks, found during the walk.
  The counted two sum in their values' encoding order (order-free), a
  χ² p-value by the regularized incomplete gamma function (series /
  Lentz, `ln Γ` by Lanczos), Yates' correction at one degree of freedom
  as scipy applies it.  While measuring: `mode` and the walks sorted
  10M keys with the standard library's sort — `quantile::sort_keys`, an
  LSD radix sort skipping the bytes every key shares (found by one
  OR of XORs), now sorts them.
  Tests: `drift.rs` — scipy's and scikit-learn's answers on 30,000- and
  9,000-row tables (exact and asymptotic KS, AUC, PSI by its
  definition, χ² with 4 degrees of freedom, JS), the same bits in five
  settings (four workers; `work_mem` 64 kB spilling both sides and the
  walk's run); per group as the group alone; `NULL`s and empty sides;
  types refused.  Unit tests: the exact p-value by hand, χ²'s 5 % points
  at 1 and 10 degrees of freedom, the Kolmogorov tail continuous where
  its two series meet, the radix sort against the standard sort.
  `52_drift.slt` (scipy's numbers to 14 digits; the KS statistic the
  correctly rounded quotient, scipy's one unit in the last place above).
  *Prior art:* scipy's `ks_2samp` — taken: its default (exact up to
  10,000 a side), its statistic over ties, the p-value it computes (the
  probability of leaving the band, Hodges 1957); not taken: its
  asymptotic `kstwo` (Simard and L'Ecuyer's exact one-sample
  distribution at `round(m·n/(m+n))`: a larger piece of numerical code,
  recorded) — Stephens' (1970) corrected Kolmogorov limit instead, as
  Numerical Recipes' `probks`.  scikit-learn's `roc_auc_score` — its
  value, by the rank sum instead of the curve's trapezoids (equal with
  ties counted half).  PSI has no reference implementation: the
  industry's form (Siddiqi, *Credit Risk Scorecards*, 2006) with deciles
  of the reference and the common 10⁻⁴ floor for empty bins, both
  stated.  scipy's `chi2_contingency` and `jensenshannon` (base 2,
  squared, so a divergence, not its distance).  The study's other route
  — KS over two DDSketches — not taken: exact is cheap enough (below);
  it stays the way for sketches already kept (with S6's types).  Radix
  sort: LSD over bytes, as ClickHouse's `RadixSort.h` sorts numbers and
  DuckDB sorts its normalized keys; measured against pdqsort on 10M
  keys: faster with up to six differing bytes, slower with eight (so
  those keep pdqsort).
  *Measured* (10M rows of `bench/stats/aggs.py`'s table, halves by
  `id % 2`, best of five, the better of two rounds): `ks_test(x, …)`
  854 ms — 373 with `work_mem` 1 GB, where nothing spills; scipy's
  `ks_2samp` on the two halves already in numpy arrays 407 —; `auc(y,
  …)` 899 (scikit-learn 1,327); `psi(x, …)` 888; `chisq_test(id
  % 100, g < 500)` 282; `js_divergence` 275.  `mode` of 10M
  doubles in memory 390 → 316 ms, of `id % 1000` 317 → 298, past
  `work_mem` 887 → 828 (the radix sort).  `bench/phases.py` against
  S7's build, S8a alone: ClickBench (column) 17.6 / 17.8 → 18.3 / 18.2
  ms, TPC-H (row) 236.9 / 227.0 → 232.6 / 229.0 (measured again with
  S8b, below).
  *Next, recorded here:* the asymptotic KS p-value by `kstwo`'s exact
  distribution (scipy's digits); KS over two DDSketches (a bound, for
  kept sketches); the spill of 10M keys costs more than the test (373
  ms in memory, 2.3× that at the default `work_mem`) — a spill that
  writes each key once; `psi` with the bins given as edges; a record
  type for `{statistic, p-value}` (now arrays).
- **S8b — `SUMMARIZE`** ✅ (2026-11-06): item 9, the MCP tool,
  `docs/llm.md`.
  *Built:* `SUMMARIZE t` and `SUMMARIZE <query>` — one aggregate query
  over the source (`count`, `approx_count_distinct`, `min` / `max` as
  text for ordered types, `avg`, `stddev` and `approx_percentile` of the
  quartiles for numbers), so one parallel scan of every column, its one
  row turned into a row a column; read-only and described (prepared
  statements know its columns).  The MCP tool `summarize_table`, named
  in the server's instructions as the first look at a table.
  `docs/llm.md` lists every family with an example (S1–S8).
  Tests: `summarize.rs` — the figures are the aggregates' own, the same
  in five settings and in row and column storage; `53_summarize.slt`
  (a table, a query, no rows, an unknown table); the CLI's MCP test
  calls the tool.
  *Prior art:* DuckDB's `SUMMARIZE` — taken: the statement, the columns
  and their order; changed: its approximate quartiles `q25`, `q50`,
  `q75` are `approx_q25` … here (the study's line: approximate only
  under an `approx_` name; DuckDB names `approx_unique` so but not
  those), the mean and deviation as doubles (DuckDB gives text), the
  `NULL` share as a double.  pandas' `describe()` and Polars'
  `describe()` give the same figures a column (count, mean, std,
  min, quartiles, max): a model meets one of these shapes.
  *Measured:* `SUMMARIZE t` of `bench/stats/aggs.py`'s 10M rows, four
  columns: 624 ms (`sum(x)` 183, `approx_percentile` of one column 195).
  `bench/phases.py` two rounds alternating against S7's build (S8a and
  S8b together): ClickBench (column) geometric mean 17.7 / 17.7 → 17.8 /
  17.8 ms, TPC-H (row) 242.1 / 232.5 → 241.9 / 230.8 — the same within
  the noise (S8a's 18.3 earlier was noise).
  *Next, recorded here:* `SUMMARIZE` as a `FROM` item (DuckDB allows
  `FROM (SUMMARIZE t)`); its figures for vectors (§M14.12) and arrays
  (lengths); exact quartiles on request.
- Vectors' statistics with §M14.12.

**Done when:** the study is written here — the needs, the families
chosen and their order, the methods and their error bounds, the
benchmarks — and the steps it leads to are planned as this milestone's
own, agreed before they are built.

### §M14.28 — B+tree or LSM tree, chosen per table ◐ (study written; L0 ✅)

**First a study, then the plan.**  Today every table lives in the one
copy-on-write B+tree (D2, D10); an LSM tree was left for later, "as a
second engine behind the KV contract".  This milestone makes it a choice
per table, as the row / column layout is (§M14.20): `CREATE TABLE …
WITH (engine = btree | lsm)`, `ALTER TABLE … SET (engine = …)` converting
online, and the engine orthogonal to the layout (a row or a column table
on either; a partition its own, so recent data can sit in an LSM tree
and older in the B+tree).  Indexes follow their table unless given their
own.

The study answers, and records here:

- **Who needs it.**  Write-heavy tables: event and log ingestion, the
  streams of §M16.5, time series, an AI application's logged prompts and
  outputs, embeddings written in bulk (§M14.22) — where a B+tree's
  random page writes and copy-on-write path cost most; and what each
  engine's read, write and space amplification is on them, measured.
- **The shape behind the KV contract** (principle 1): the KV routes a
  table's key range to its engine; one transaction, MVCC and WAL across
  both (a commit touching tables on both engines stays atomic); the
  checkpoint and recovery of D10 for memtables and flushed runs;
  compaction scheduled against foreground work (§M14.20 W1's workers),
  its I/O bounded; tombstones and range deletes (`DROP`, `TRUNCATE`, the
  vacuum's versions); scans merging runs, with prefix bloom filters for
  lookups; the fresh-range bulk loads of D16 as ingested sorted runs;
  branches and time travel (§M14.13, §M14.14) on immutable runs; the
  embedded editions' footprint (§M14.23); the deterministic simulation
  (principle 4) covering compaction and crashes in it.
- **Prior art**, read for ideas, not copied (`CLAUDE.md`): O'Neil et al.,
  "The Log-Structured Merge-Tree" (1996); LevelDB and RocksDB (leveled and
  universal compaction, column families — a per-table engine in all but
  name), Bigtable, Cassandra and ScyllaDB; WiredTiger, which offered a B
  tree or an LSM tree per table (`type=lsm`) and later deprecated its LSM
  tree — why is part of the study; MyRocks (an LSM engine under a B-tree
  database, chosen per table); WiscKey (keys and values apart); Monkey
  and Dostoevsky (bloom filters and merge policy tuned as an optimization
  problem); bLSM and PebblesDB; CockroachDB's move to Pebble.
- **The fit with nucleus's principles**: one KV contract, so SQL,
  documents and streams need not know which engine holds a table; one
  file and WAL layout (D6) or a reason recorded to leave it; the format
  versioned and older ones read (D15, D18); measured against the B+tree on
  the §M14.11 suite and on write-heavy workloads before it is
  recommended for any.

**The study** (2026-11-06; what it asked is kept in its answers).

*Who needs it.*  Tables written far more than they are read per key:
event and log ingestion, §M16.5's streams, time series, an AI
application's logged prompts, outputs and scores, embeddings written in
bulk (§M14.22), counters and sessions updated in place.  Their keys are
either in order (a time, `uuidv7()`) or random to the table's clustered
order (`uuid` v4, a hash, `(user, time)` across many users).  The second
kind is where a B+tree pays: each row lands on a different leaf, and a
leaf is written whole.

*Measured: write amplification today* (`bench/wamp/wamp.py`; rows of 108
bytes — a `bigint` key and 100 bytes of text; the bytes the server wrote
to disk, all of a phase's pages durable before it counts as done:
nucleus killed after the phase and restarted, its recovery replaying the
WAL and checkpointing; PostgreSQL 18 in Docker with its defaults, a
`CHECKPOINT`, the bytes its cgroup wrote.  APFS on an Apple M-series
SSD; nucleus with its default 128 MiB page cache and a checkpoint every
GiB of WAL; two runs each where they differ):

| phase | nucleus | PostgreSQL 18 |
|---|---:|---:|
| bulk: 10M rows, 1M a transaction, keys in order | 4.1 | 5.2 – 7.8 |
| bulk: 10M rows, 1M a transaction, random keys | 4.7 – 5.7 | 7.8 – 11.3 |
| 100k rows, one a transaction, keys in order | 152 | 80 |
| 100k rows, one a transaction, random keys among the 10M | 204 | 83 |
| 100k rows, 100 a transaction, random keys | 27 | 6.6 |
| 5M rows, 100 a transaction, random keys (steady state) | 29.4 | 29.6 |

Where the bytes go:

- **Bulk** loads are already cheap in nucleus's B+tree — cheaper than
  PostgreSQL's heap and index, which writes each row to the WAL, the
  heap and the index, and full pages after each checkpoint.  An LSM
  tree would not beat 4–6 here (WAL, flush, then compactions); a sorted
  bulk load written straight as a run (D16's fresh ranges) can reach ~1
  on either engine.
- **A row a transaction** costs nucleus ~16 KiB a commit (1541 MiB for
  100k), PostgreSQL ~8 KiB (its WAL page).  It is the commit, not the
  tree: the same for keys in order, and only 31 MiB was left for the
  checkpoint.  nucleus appends to a WAL segment that grows, so each
  `F_FULLFSYNC` writes the data block and the file's metadata;
  PostgreSQL writes into preallocated, recycled segments.  An LSM tree
  has the same WAL, so this is fixed beside it (step L0), not by it.
- **Random keys into a large table** are the tree.  nucleus stores rows
  in the leaves of the table's B+tree (a clustered table): 100k random
  rows dirty tens of thousands of 8 KiB leaves, each written whole at
  the checkpoint (539 MiB after the one-row phase, 248 MiB after the
  100-row one).  PostgreSQL appends rows to its heap and only its narrow
  index leaves are random.  What a leaf costs per row depends on how many
  rows it collects between checkpoints — the steady-state row is the one
  that decides: over 5M rows both engines write about 29 bytes per byte
  of rows (nucleus 12.9 GiB during the phase — its checkpoints every GiB
  of WAL and the evictions of a 128 MiB cache under a 3 GiB table — and
  1.9 GiB at the end; PostgreSQL its index leaves and the full-page
  images after each checkpoint).  The short phase's 27 against 6.6 was
  the forced checkpoint, not the engines.  With the table twenty times
  the cache, a leaf collects few rows between its writes in either
  engine; this is the workload an LSM tree is for.  (nucleus took 857 s
  with the test suite running beside it, PostgreSQL 269 s: times not
  comparable in this run.)

*The shape behind the KV contract* (principle 1).  An engine serves a
key range — a table, an index, a partition — and the KV routes each
range to its engine; SQL, documents and streams see one ordered,
versioned map as now.

- **One WAL.**  A commit's batch is one WAL record whatever engines it
  touches, so a commit across both stays atomic without two-phase commit
  (MySQL's cross-engine transactions need the binlog's 2PC — the cost
  avoided).  The LSM's memtable is filled from the batch as the B+tree's
  pages are; the WAL is cut at the oldest of the B+tree's checkpoint and
  the oldest unflushed memtable; recovery replays into both (D10).
- **MVCC unchanged**: versions are keys to either engine.  The vacuum's
  deletions are tombstones, dropped by compaction at the last level;
  `DROP` and `TRUNCATE` delete the runs wholly inside the range and leave
  a range tombstone for the rest (RocksDB's `DeleteFilesInRange` and
  range deletions).
- **Files**: runs are immutable files of D6's checksummed blocks with a
  block index and a prefix bloom filter, beside the B+tree's file; a
  manifest names the live runs and is switched like the meta page at a
  checkpoint; formats versioned and old ones read (D15, D18).
- **Bulk loads** (D16's fresh ranges) write a sorted run straight into
  the last level, no WAL (RocksDB's and Pebble's ingestion).
- **Branches and time travel** (§M14.13, §M14.14): immutable runs are
  shared between branches by reference, as the B+tree's copy-on-write
  pages are.
- **Compaction** runs on §M14.20 W1's workers with an I/O budget, so
  foreground writes see bounded stalls (bLSM's "spring and gear"
  scheduler; RocksDB's rate limiter); its memory is its own budget
  beside the page cache (WiredTiger's lesson below).  The embedded
  editions (§M14.23) default to a small memtable and one compaction
  thread.
- **The simulation** (principle 4) drives flushes and compactions
  through the platform layer, with crashes inside them; every engine runs
  the same KV and SQL test suites.

*The compaction policy chosen, and why.*  One mechanism with parameters,
not a menu of strategies ("one mechanism per job"): tiered and leveled
compaction are the two ends of one scale — a fan-in `T`, and per level
whether it holds several runs or one (Dostoevsky, Dayan and Idreos 2018;
Cassandra 5's Unified Compaction Strategy is the same idea in
production).  The default is **lazy leveling**: tiered upper levels
(each byte rewritten about once per level) and one leveled last level
(space amplification at most 1 + 1/`T`; a point lookup one run there and
a bloom filter per run above), bloom filter bits spread over levels by
Monkey's allocation (Dayan, Athanassoulis and Idreos 2017: more bits on
the small levels).  For the random-key workload above that is about
1 (WAL) + 1 (flush) + 1 per upper level + `T`/2 at the last: ~6–7 at
`T` = 4 and four levels, against the 29 both engines measured — about
four times less.  Time
series need no policy of their own: a partition by time (§M14.20) is
dropped or detached with its runs — what Cassandra's time-window and
RocksDB's FIFO compaction give.

*What stays with the B+tree*: read-mostly tables, tables that fit the
cache, point lookups and range scans that must not merge runs, small
tables, the catalog; and, until measured otherwise, everything by
default.

*Prior art, read for ideas.*

- **O'Neil, Cheng, Gawlick and O'Neil, "The Log-Structured Merge-Tree"
  (1996)**: the model; the rolling merge between components is what
  leveled compaction became.
- **LevelDB → RocksDB**: leveled compaction (each level `T` = 10 times
  the last; up to about `T` rewrites of a byte per level),
  universal (tiered: less writing, up to twice the space), FIFO;
  **column families** — separate LSM trees sharing one WAL, atomic
  across families — are an engine per table in all but name, and the
  model for one WAL here.  Taken: the shared WAL, ingestion, range
  deletions, the rate limiter.  Not taken: the number of strategies and
  options (hundreds), which one parameterized policy replaces.
- **WiredTiger** offered a B-tree or an LSM tree per table (`type=lsm`);
  MongoDB, its main user, kept to the B-tree; the LSM tree's tests were
  disabled (WT-8867) and it was deprecated.  No rationale was published
  that we found; the tickets show LSM chunks and B-tree pages
  competing for one cache and eviction (WT-2940).  Taken: the LSM's
  memory is its own budget, and it runs every test the B+tree runs — a
  second engine that is second-class in testing decays.
- **MyRocks** (Matsunobu, Dong and Lee, VLDB 2020): RocksDB under MySQL,
  chosen per table beside InnoDB; Facebook's user database moved to it
  for about half the space or less, and less writing (compression of immutable
  runs; no half-empty pages).  Taken: per-table choice; space
  amplification as a measured goal, not only writes.
- **CockroachDB's move to Pebble** (2020): an engine they control,
  ingestion of runs for bulk loads and rebalancing, L0 sublevels against
  write stalls.  Taken: ingestion, and owning the engine rather than
  embedding one (principle 2).
- **WiscKey** (Lu et al., FAST 2016; RocksDB's BlobDB): keys in the tree,
  large values in a log.  Not now — 100-byte rows gain nothing — but an
  embedding of 768 floats is 3 KiB: to measure with §M14.12's tables.
- **bLSM** (Sears and Ramakrishnan, SIGMOD 2012): bounded write latency
  by scheduling merges; bloom filters on every run.  Taken: the
  scheduling idea.
- **PebblesDB** (Raju et al., SOSP 2017): fragmented runs between
  guards, less rewriting, more read cost.  Not taken: lazy leveling gets
  most of the saving with simpler invariants.
- **Cassandra** (size-tiered, leveled, time-window, then the Unified
  Compaction Strategy) and **ScyllaDB** (incremental compaction, against
  size-tiered's temporary doubling of space): the history that ends in
  one parameterized policy.
- **Buffered B-trees instead of a second engine**: Bε-trees (TokuDB's
  fractal trees: buffers in the inner nodes) and InnoDB's change buffer
  (secondary index changes held until their page is read; MySQL 8.4
  turned it off by default).  They keep one structure but make every
  page more complex and every read check the buffers; immutable runs are
  simpler to verify, compress and share between branches.  Not taken,
  recorded as the alternative if the engine boundary (L1) proves costly.
- **The RUM conjecture** (Athanassoulis et al., EDBT 2016): read, update
  and memory overheads cannot all be minimal — the reason the choice is
  per table and not global.

*Benchmarks to run* (each step against the one before, on macOS and on
Linux ext4): `bench/wamp/wamp.py` at steady state, with updates and
deletes added; space amplification after each; point-lookup and
range-scan latency (p50, p99) while compaction runs; YCSB's A (50 %
updates), B (5 %) and E (scans); the §M14.11 suite on an LSM table;
recovery time after a crash mid-compaction.

*Steps*, each its own commit, measured, in this order (agreed
2026-11-06: "legyen meg az S, L és V lépések"):

- **L0 — the commit's bytes** ✅: WAL segments preallocated and recycled
  (left in W7), so a commit writes its block and no metadata; the bytes a
  commit costs measured on APFS and ext4.  Both engines gain.
  *Measured first* (a probe: 3000 appends of 220 bytes, each synced;
  bytes the device wrote): on **ext4** (Docker's Linux VM) into a file
  that grows 16.7 KiB and ~400 µs a commit, by `fsync` or `fdatasync`;
  into zeros written ahead, or a recycled file, 5.7 KiB by `fsync` and
  **4.2 KiB, ~80 µs by `fdatasync`**.  On **APFS** the reverse: a file
  that grows 10–12 KiB a commit, zeros written ahead or a recycled file
  16.2 KiB — APFS writes a changed block elsewhere (copy-on-write), so an
  overwrite allocates as an append does and costs more metadata; the
  time is the same (~180 µs ordered).  So the 16 KiB a commit measured
  above on APFS is the file system's, and PostgreSQL's 8 KiB was
  measured on ext4 in a cgroup that leaves out the journal: not the same
  quantity.  *Taken*: the platform's sync is `fdatasync` on Linux
  (PostgreSQL's `wal_sync_method` default there, and SQLite's); a
  file system says whether it writes in place
  (`FileSystem::writes_in_place`: not on Apple's systems); where it does,
  the WAL grows a segment ahead of its records by zeros a mebibyte at a
  time and keeps the segments a checkpoint drops as spares, renamed into
  the next ones, as many as that checkpoint dropped (PostgreSQL's
  recycled segments, `wal_recycle`; its zero-filled new segments,
  `wal_init_zero`) — APFS keeps segments that grow.  A spare's old
  records are not cleared: each record's checksum folds in its LSN
  (D10), so none passes at its new place (PostgreSQL relies on its
  pages' addresses the same way); a torn record of the log's own at the
  end is written over by zeros on open.  The simulation answers
  `writes_in_place` by its seed, so every seeded crash test runs both
  ways; it found, with recycling, that a checkpoint could drop old
  segments before the directory entry of the segment holding it was
  durable — a crash keeping an older segment but losing the newer left
  a log ending before the checkpoint.  The deletions before had the same
  window; fixed for both by a directory sync first.  *Measured*, the
  server on ext4 (Docker's Linux VM, `bench/wamp/wamp.py
  nucleus-docker`), against the step before: 100k one-row commits
  41.8 / 58.3 / 58.3 → 15.2 / 14.6 / 13.6 s, the
  disk's bytes a commit 16.6 / 17.0 → 4.4 KiB (write amplification 157 →
  41); 100 rows a transaction 4.2 / 4.2 / 4.4 → 3.3 / 4.0 / 3.8 s (its bytes 39 → 35
  MiB, the new segment's zeros included).  macOS unchanged by
  construction (segments grow there as before), confirmed by
  `bench/phases.py`: the TPC-H SF 1 load on row tables 32.5 / 26.2 → 26.9 / 26.4 s (the first
  run cold).
- **L1 — the engine boundary**: the KV routes a key range to an engine
  behind the store's interface (get, scan, apply a batch, checkpoint);
  one WAL; a second, trivial engine (a sorted map flushed whole at a
  checkpoint) in the tests proves routing, atomic commits across engines
  and recovery.
- **L2 — the LSM engine**: memtable, flushes to runs, the manifest,
  merging reads, bloom filters, lazy-leveling compaction, range
  tombstones; the simulation with crashes in flushes and compactions.
- **L3 — in SQL**: `CREATE TABLE … WITH (engine = lsm)`, per partition,
  indexes following their table; `ALTER TABLE … SET (engine = …)`
  converting online (a copy into the new engine, switched at a commit).
- **L4 — scheduling and ingestion**: compaction against foreground work
  with an I/O budget; bulk loads as ingested runs.
- **L5 — the verdict**: the benchmarks above; where the LSM is
  recommended, recorded here with the numbers.

**Done when:** the study is written here — the workloads and their
measured amplifications, the design behind the KV contract, the
compaction policy chosen and why, what is left to the B+tree — and the
steps it leads to are planned as this milestone's own, agreed before
they are built.

### §M14.12 — Vectors ✅ (V1: the type, exact search, every driver; V2: bound by bandwidth; V2b: positions, numpy / Arrow, pgvector's clients; V3: estimates on a kept sample; V4: the HNSW index's storage, measured; V4b: the index in the engine; V5: filtered search; V6: quantized graphs, sparse vectors; V7: query vectors from rows, `LATERAL`, vectors read with their row, `sparsevec` in the drivers)

Embeddings beside the data they describe, in the same transactions.
`vector` is a **built-in type with every capability** — it matters most
for AI work, so nothing about it is optional.  It is built as a module
through §M14.19's type registry like every other built-in, and it is the
reference that the capability model is proven on:

- `VECTOR(n)`, float32 components, the dimension as a modifier checked on
  every write; text form `'[1, 2.5, -3]'`; casts to and from `real[]`
  and `text`; an `ncp` tag and a native mapping in every driver (`Vec<f32>`,
  `float*`, `std::vector<float>`, a list or `array('f')` in Python,
  `Float32Array`, `float[]`).
- Equality and hashing (`DISTINCT`, `GROUP BY`, `UNIQUE`, hash joins) and
  a default total order, element by element, with an order-preserving key
  encoding — so a vector can be sorted, be a key and be B-tree-indexed.
- Distances as distance capabilities — L2 `<->`, cosine `<=>`, inner
  product `<#>`, L1 `<+>` — and exact nearest-neighbour search with
  `ORDER BY v <-> :q LIMIT k`; each distance a SIMD batch kernel (AVX2,
  AVX-512, NEON, chosen at run time), so an exact search over a column is
  bound by memory bandwidth.
- Operators and functions: element-wise `+`, `-`, `*`, `||`
  (concatenation), `v[i]` and slices, `dims`, `norm`, `normalize`,
  `subvector`; `UPDATE … SET v[i] = …`.
- Aggregates: `avg` and `sum` (centroids).
- Statistics for the optimizer: dimension, norms, and distance selectivity
  sampled from the data.
- An approximate nearest-neighbour index (HNSW) the optimizer uses for
  `ORDER BY … LIMIT k`, kept transactionally consistent with the table,
  one per distance.
- **Filtered vector search** — "the ten nearest, among this user's
  documents" — returning `k` rows at the index's recall whatever the
  filter, where an ANN index followed by a `WHERE` returns too few or
  wrong rows (the common failure of vector extensions):
  - the filter is applied *during* the graph search, not after it, so
    the search keeps walking until it has `k` matching rows;
  - the optimizer estimates the filter's selectivity and, when few rows
    pass, searches them exactly instead (or through a B-tree index on the
    filter columns first);
  - an index can be split by a column (`CREATE INDEX … USING hnsw (v)
    PARTITION BY (tenant_id)`), one graph per value, for multi-tenant
    applications; table partitions (§M14.20) each have their own graph;
  - row-level security (§M14.25) is a filter like any other, so a search
    never returns, or is shaped by, rows the session may not see.
- Later, as further encodings or types on the same model: half precision,
  int8 and binary quantization, sparse vectors.

**V1 ✅ (2026-11-06) — the type, exact search, every driver.**

- `vector` / `vector(n)` (1–16000 components, pgvector's limit) as a
  registered type in `crates/sql/src/vectors.rs`, on the capability
  model: canonical bytes are the components as little-endian `f32`s
  (`-0` stored as `0`, so equal vectors are equal bytes); equality and
  hashing on them; the total order component by component, then the
  shorter first (pgvector's, which is PostgreSQL's arrays'); an
  order-preserving key encoding (`01`, a sortable `u32` per component,
  `00`), so a vector is a key, sorted, `UNIQUE` and B-tree indexed.  The
  law kit has a `vector` generator (`-0`, extremes, shared prefixes,
  lengths 1–4) and passes on it.
- The capability model grew what `vector` needed, each a `Type` method
  any module's type can use (§8.12 in DOCS): `arithmetic()` (operators
  the type defines: `+ - * ||`), a modifier (`modifier(n)` checked where
  `type(n)` is written, `fit(v, n)` on every write and cast), casts
  (`cast_from`, `cast_to`: `real[]` and any numeric array both ways;
  `text` as for every type), and `aggregate(name)` (a type's own `sum`
  and `avg`).
- Distances `<->` (the type's `Distance`), `<=>`, `<#>`, `<+>` and their
  functions; `ORDER BY v <-> $1 LIMIT k` is the exact search.  Each
  distance is a `nucleus-kernels` kernel over the stored bytes: sixteen
  running `f32` sums, component `i` into sum `i mod 16`, added in a fixed
  order — so a distance is the same bits on NEON, AVX2 and AVX-512 and in
  the reference (a test checks it at every level for lengths 0–1536).
  FAISS and pgvector leave the order to the compiler and fuse multiplies
  into adds where the CPU can (results that differ by machine); a fixed
  order keeps principle 4's determinism (its cost against a fused,
  unordered kernel is to be measured with the benchmark below).
- Functions `vector_dims` / `dims`, `vector_norm` / `l2_norm` / `norm`,
  `l2_normalize` / `normalize`, `subvector`, `inner_product`; `avg` and
  `sum` exact per component (`fsum::ExactSum`, §M14.27 item 10's
  centroids; pgvector sums in `float8`, order-dependent).
- `ncp`: tag 11 named `vector`, its bytes as stored (little-endian, so a
  driver reads a column in place: numpy, `Float32Array`, C `float*`).
  Drivers: Rust `Value::Vector(Vec<f32>)`, C `NC_VECTOR` / `nc_vector`,
  C++ `std::vector<float>`, Python `array('f')` (float32 numpy arrays
  sent too), JavaScript `Float32Array`, Java `float[]` — each tested
  both ways, in rows, batches and bulk loads where the driver has them.
- `crates/testkit/slt/45_vector.slt` (pgvector's definitions; nucleus's
  SQLSTATEs) in every slt mode; `docs/llm.md` names the type and the
  operators.

*Measured* (`bench/vectors/knn.py`, Apple M-series, release build):
100,000 random vectors of 768 dimensions (293 MiB), the ten nearest
of 20 queries, median; pgvector (image `pgvector/pgvector:pg18`), no index:

| | ms | GiB/s read | answers differing from numpy's |
|---|---:|---:|---:|
| numpy brute force (one core) | 57 | 5.1 | — |
| nucleus `ORDER BY v <-> $1 LIMIT 10` | 206 | 1.4 | 0 of 20 |
| pgvector, exact | 155 | 1.8 | 0 of 20 |

Exact, and the same answers as the reference — but not yet bound by
memory bandwidth as specified: about 2 µs a row, where the kernel needs
a tenth of that.  Where the rest goes (the scan making a value of each
3 KiB vector, the top-k a row at a time, parallelism) is V2's first
question.

**V2 — the exact search bound by bandwidth** ✅ (2026-11-06; the steps
agreed: "legyen meg az S, L és V lépések").  The question above, answered
by profiles (`sample`, the query repeated):

1. *The page cache.*  The server's default cache is 128 MiB; the table,
   293 MiB of vectors, did not fit, so every query read it again through
   `pread` and checked each page's checksum — while pgvector ran with
   `shared_buffers = 1GB`.  Taken: the server's `--cache <size>`
   (PostgreSQL's `shared_buffers`; the KV's `Options::cache_bytes`), and
   `knn.py` gives nucleus the same 1 GB.
2. *A seek a vector.*  A 3 KiB vector is longer than a row keeps
   (§M14.26: 2000 bytes) and lives out of line, in chunks keyed by its
   row; a scan read each by a scan of its own (a B+tree descent, the
   versions resolved).  Taken: `large::Chunks`, one scan of the chunks
   from the first row holding one to the scan's end, walked beside the
   rows' — the chunks are keyed by the row's key, so they come in the
   rows' order — reading a value's chunks until they hold its length (no
   read-ahead) and decoding a one-chunk value from the chunk as read: a
   merge of two ordered ranges where there were point lookups, for
   every type stored out of line (`bytea`, long text).  The row-at-a-time
   executor keeps its lookups (a row at a time is its contract).
3. *A lock a value.*  Decoding a registered type's value looks its type
   up by id in the process's interning table, under a mutex — taken by
   every worker of a parallel scan for every row, where the profile
   waited most.  Taken: each thread keeps the type it last looked up
   (by id and by name; a real type only, a stand-in for a missing one
   may give way); `vectors::vector_type` was a lookup by name a call.

Measured (`bench/vectors/knn.py`, 100k × 768, the median of 10
queries; one run each, alternating where two builds):

| | ms | GiB/s |
|---|---:|---:|
| before (128 MiB cache) | 233 – 243 | 1.2 |
| chunks read in row order (128 MiB) | 178 – 180 | 1.6 |
| … and a 1 GB cache | 128 | 2.2 |
| … and the type looked up once a thread | 53 | 5.4 |
| … and no read-ahead, one copy less | **48** | **6.0** |
| numpy brute force (one core) | 55 – 58 | 4.9 – 5.2 |
| pgvector, exact (1 GB `shared_buffers`) | 155 – 167 | 1.7 – 1.8 |

The same answers as numpy's throughout.  TPC-H unchanged
(`bench/phases.py reads`, alternating: the sums of the best times 8219 / 7531 → 7612 / 8110 ms).  What the profile
shows now: copies (the B+tree's overflow pages into the scan's chunk,
the chunk into the value) as much as the distance kernel, then
allocations — a value of each vector.  Left, measured when worth it: a
batch kind for fixed-width values (the bytes of a column of vectors in
one buffer, no value each) and the distance as a batch kernel over it.

**V2b — components by position, numpy and Arrow, pgvector's clients** ✅
(2026-11-06).

- `v[i]`, `v[lo:hi]` and `UPDATE … SET v[i] = x` — and the same for
  arrays, which had `a[i]` only: one mechanism, a type capability
  `Elements` (`element_type`, `count`, `get`, `slice`, `with`; 0-based,
  in range), which a registered type has or not, the binder and the
  evaluator doing SQL's 1-based positions, clamping and errors once for
  both.  PostgreSQL's array rules taken as they are (a slice's bounds
  clamped, an assignment past the end growing the array with NULLs, a
  NULL array taken as empty); a vector differs only where it must: an
  empty slice and a position outside the dimension are errors (`22023`,
  `2202E`), since no vector is empty and a column's dimension is fixed.
  pgvector has no subscripts (`subvector` only); its users ask for them.
- Python: a `vector` column of a batch is a `(n, d)` `float32` numpy
  array over the batch's bytes (no copy; NULL rows zeros and masked in
  `to_numpy`), and Arrow's `fixed_size_list<float32>` (`to_arrow`; the
  layout Arrow, Lance and pandas' Arrow types read).  A column whose
  vectors differ in dimension stays objects.
- pg-compat: `vector` has an oid (13201; pgvector's is whatever the
  extension got, and its clients look it up by name), a row in `pg_type`,
  `to_regtype(name)`, pgvector's text and its binary format both ways
  (`vector_send` / `vector_recv`: the dimension count, an unused `int16`,
  big-endian `float4`s).  Checked with pgvector's own Python client
  (psycopg 3, `register_vector`, numpy in and `Vector` out, text and
  binary).  `register_vector` also requires `bit` to exist: `pg_type`
  lists PostgreSQL's `bit` (nucleus has none; a value sent as one is
  refused).  *Not yet:* a driver that resolves an unknown oid through
  `pg_type` with oid-typed parameters and results — rust-postgres (so
  the `pgvector` crate), sqlx — needs pg-compat's catalog columns typed
  `oid` and `"char"` as PostgreSQL's are, where nucleus has `bigint` and
  `text`: a pg-compat step of its own (a wire type per catalog column),
  not a vector one.

TPC-H unchanged (`bench/phases.py reads`, alternating, the sums of the
best times: 8575 / 7888 → 7749 / 7874 ms).

*Done — V3, estimates on a kept sample* (2026-11-06).  A distance
below a bound was estimated as any comparison the statistics cannot
read: a third of the rows, whatever the bound.  Norms, a dimension's
spread or sampled distance quantiles (the plan) would estimate a
distance to a constant vector only, and only through a model of the
distance's distribution; the kept sample estimates any condition the
histograms cannot — a distance to a parameter, `LIKE`, `id % 7 = 0`,
`length(w) = 2` — by running it.  `ANALYZE` keeps up to 1,000 of the
rows it sampled (at most 1 MiB; drawn from Algorithm L's reservoir by
a partial Fisher–Yates shuffle) under `00 04 <table id> 01`; planning
reads them once per table and evaluates the condition on them (matches
/ rows, at least half a row); a subquery, a volatile call, an error on
a sampled row or columns changed since `ANALYZE` (the row width
differs; a type change or `DROP TABLE` deletes the rows) fall back to
the defaults.  Prior art: Oracle's dynamic sampling (it reads blocks at
planning time; nucleus stores the rows at `ANALYZE` and reads one key,
so planning never scans), PostgreSQL's defaults (`DEFAULT_MATCH_SEL`
and its fixed thirds, kept as the fallback), SQL Server's sampled
statistics.  Not taken: the dimension statistics and norms (M14.27
item 10 keeps them as aggregates); a sample per index. While testing:
the join order's dynamic programming planned a cross product of two
inputs a third joins (a star's dimensions) whenever its estimate was
lowest — a chain's subsets now join only along conditions (DPccp's
connected subsets, PostgreSQL's `have_relevant_joinclause`), a cross
product only for a chain they leave in pieces, as DOCS §8.8 already
said.  Tests: estimates within 5 % of the actual rows for a distance to
a constant and to a parameter, `LIKE`, `id % 7 = 0` and `length(w) =
2`; the default after `ADD COLUMN`, the sample again after `ANALYZE`;
a star's plan without a cross product (`10_statistics.slt`).
Measured, and a cost recalibrated: with `p_name LIKE '%green%'`
estimated right (10,800 parts, not 20,000) TPC-H Q9 on row tables took
an index nested loop into `lineitem` by `l_partkey` (324,000 rows
fetched) for the hash join over all 6M — 527 → 900 ms.  The lookup's
constants dated from before W7's scans: measured again on one thread, a
lookup into `orders` by its key 2.9 µs, a row the `l_partkey` index
finds 5.8 µs, a hash join's row 0.07–0.1 µs — `LOOKUP_ROWS` 12 → 40,
`FETCH_ROWS` 20 → 60, `COLUMN_LOOKUP` 4 → 1.2 (a column table's lookup
as it was); both joins then hash (`orders` 1,782 → 1,256 ms, `lineitem`
by part 1,908 → 1,092 ms).  Reading the kept rows whole cost planning
3–4 ms on ClickBench's 105 columns: each column is kept under a key of
its own, as a column table's segment encodes it (`segment::encode`,
decoded straight into the batch's vector), and only the condition's
columns are read and evaluated as a batch — `EXPLAIN` of ClickBench's
Q20 0.18 → 0.24 ms, Q22 0.23 → 0.74 ms (4.2 ms before).  Measured,
alternating with S8, two rounds, geometric means: TPC-H on row tables
228.6 / 225.7 → 210.2 / 208.8 ms (Q21 944 → 521, Q7 621 → 447, Q16 193
→ 157; Q18 469 → 674: its `HAVING sum(l_quantity) > 300` estimated a
third, as PostgreSQL does, 57 rows in fact — the old constants' lookups
won by mistake; the adaptive join below is the answer), column tables
76.4 / 75.6 → 73.4 / 76.0 ms (Q21 402 → 169 in some runs: its plan
follows the sample `ANALYZE` draws), ClickBench on column tables 18.1 /
17.5 → 17.4 / 17.8 ms, on row tables 144.6 / 139.0 → 139.5 / 135.4 ms.
Against PostgreSQL 18.6 (`docs/benchmarks.md`): TPC-H 0.25× on column
tables, 0.76× on row tables (0.77× before); ClickBench 0.10× and 0.83×
(0.11×, 0.89×); every result the same, but for ties at a `LIMIT`.

*Done — the adaptive join* (2026-11-06; agreed after V3 as important).  An inner hash join whose left side an index
could look up carries that plan too (`plan::Adaptive`, made last, after
narrowing, so its lookups read the columns the join's left side has) and
the inflection point: the right rows below which the lookups cost less
than the hash join (`LookupCosts::below`, from the costs `lookups`
compares).  Running, the join makes its right side as the hash join
does — on the workers, gathered in columns, or stashed — and counts it:
at most that many rows look the left table up (an index nested loop with
the right rows outside, its rows put back left columns first, the left
side never scanned), more run the hash join.  `vector::prepare` decides
for a pipeline (the few rows handed to `exec`'s join, the pipeline above
not run as one), `exec`'s join otherwise: one choice in every executor.
Prior art: Oracle 12c's adaptive plans (a statistics collector buffers
rows up to the inflection point the optimizer computed, then picks
nested loops or hash), SQL Server 2017's adaptive joins (the build input
counted against a threshold row count, batch mode).  Taken: the
threshold from the cost model and the build side counted, not sampled.
Differs: the decision costs nothing extra (the right side is made either
way, and kept), and both plans are planned (`EXPLAIN`: `Hash Join (…) or
lookups up to N rows`).  Not taken: the other direction (a lookup join
whose outer side turns out large switching to hashing — the outer would
have to be buffered past the threshold first); outer, semi and anti
joins (an inner join's rows only change order).  Tests: `adaptive.rs` —
few and many right rows, a condition beyond the keys, row and column
tables, five settings (the row path, batches of 3, workers, `work_mem`
64 kB): the hash join's rows, the same order in every executor within
`work_mem`, the lookups counted where they ran; mutation caught (the
columns not put back).  Not reached by them: `exec`'s choice over a
table it made itself (a join no pipeline prepared).  Seen while
testing, not from this step: `many_groups_past_work_mem_spill_on_the_
workers`'s "few groups spilled" failed once with the whole suite running
(a morsel's timing), passed alone four times.
Found while measuring: the first build took Q8 on column tables from
84 to 5050 ms — the lookups the adaptive plan carries were not walked by
`prune::needed`, so they decoded every column of `lineitem` (its
comments too) and pushed the column cache out; walked now (and a left
side holding a subquery is not given the alternative), Q8 is back to
330 ms on one worker and 91–95 ms on ten (base 324 / 90).  Measured
(`bench/phases.py`, against 7ba43b3, alternating, two rounds; geometric
means, base → new): TPC-H SF1 row tables 215.7 / 218.7 → 205.9 / 211.7
ms (Q18 655 → 438: its `HAVING sum(…) > 300` kept 57 of the 1.5 M
orders estimated as a third, so `lineitem` is looked up instead of
hashed), column tables 71.8 / 75.8 → 74.5 / 71.4 (Q18 177 → 165, the
rest within noise); ClickBench column 17.3 / 17.8 → 17.1 / 17.0, row
148.7 / 132.1 → 145.8 / 133.9 (no joins: noise).

*Left, as steps of their own:* **V4** the HNSW index (below: the design
agreed so far and the measurement that decides it).  **V5** filtered search as specified above (ACORN's
predicate-aware traversal, Patel et al. 2024, and Qdrant's filterable
HNSW are the prior art to read).  **V6** half, int8 and binary
quantization ✅ (V6a, below); sparse vectors ✅ (V6b).  **V7** what is
left of *Done when* and what the measurements found — the next step
(below, after V6b).

*V4, the HNSW index: where the graph lives* (planned 2026-11-06;
measured 2026-11-06/07, below).  The approximate index (Malkov and Yashunin 2018)
the optimizer uses for `ORDER BY v <-> q LIMIT k`, one per distance,
kept transactionally consistent with its table, with a recall suite on
public data (ann-benchmarks' method).  The open question is where the
graph lives; it is decided by measuring three storage models built as
prototypes on one HNSW core, before the engine gets any of them.

The three storage models:

- **(A) One shared graph** (pgvector's): a KV key per node (its vector
  and out-edges per layer); edges added to an existing node as keys of
  their own (`node, from`) so concurrent inserts do not write the same
  key; a node's list rewritten (pruned to `M_max` by the heuristic) only
  when its added edges pile up.  Always one graph, no merges.  Its cost:
  every hop of a search is a KV read (~3 µs measured for a point read),
  ~1,000–3,000 per search at 1 M vectors — some milliseconds — and a
  node read cannot be cached by version from above the KV contract.
- **(B) Segments** (Lucene's per-segment graphs, Milvus' growing and
  sealed segments): immutable graph segments, each keyed by a hash of its
  contents (as the column store's segments, §M14.20 V7), so decoded
  graphs are cached in memory and a hop is a memory access; rows written
  since go to a delta (a key per row: no conflicts), searched exactly;
  the delta sealed into a new segment after a statement wrote enough
  rows, in its transaction; segments merged by a tiered policy (Lucene's
  `TieredMergePolicy`; a merge inserts the smaller graphs into a copy of
  the largest, Lucene's merge on an initial graph).  Its cost: a search
  visits every segment; a large merge is a long write.
- **(C) The combination** (the design proposed): one graph, as (A), but
  stored as (B) stores — immutable, content-hashed pages: vector pages
  never rewritten (new nodes go to new pages), adjacency pages apart from
  them; a delta as (B)'s, searched exactly; the delta *folded into the
  one graph* in a batch (not a new segment), the new nodes' edges to
  existing nodes kept as small keys beside their page (as (A)'s) until a
  page's pile is large enough to rewrite it.  FreshDiskANN (Singh et al.
  2021) is the nearest prior art: an immutable graph, a fresh in-memory
  index beside it, merged in the background.  The two others are its
  extremes: a fold threshold of one row is (A); never folding is an
  exact search over the delta alone.

Common to all three: MVCC and crashes from the KV contract (a snapshot
reads the page directory, segments or nodes, and the delta, as of
itself; a rolled-back writer's nodes vanish with it); a row deleted or
updated after it entered the graph found on the way out (the row read in
the snapshot, the distance recomputed from the indexed vector), marked
deleted by a key per row (the column store's `DELETED`), repaired at a
fold, a merge or `VACUUM` (pgvector's repair of neighbours); the graph
built deterministically (a node's level from a hash of its key, inserts
in key order — principle 4's same results for the same data and
history).

Adaptive behaviour on top (any of the three): the optimizer chooses the
graph or an exact search by cost — exact on small tables and narrow
filters, and earlier on column tables, whose exact search reads only the
vector column; the fold (C) or seal (B) threshold from the same costs
(fold when scanning the delta costs a set share of a graph search), not
a fixed row count; a search that finds fewer than `k` visible rows
widens (`ef` doubled — pgvector 0.8's iterative index scans).  Settings,
as pgvector's: `WITH (m = 16, ef_construction = 64)` on the index,
`ef_search` (40) in the session; one mechanism, no switch between
models.  Row and column tables share the index: it keeps its own copy of
the vectors (pgvector's element tuples), so their difference is in the
costs only (a point read ~3.5 µs on a row table, ~8 µs on a column
table; the exact search far cheaper on columns).

*The measurement* (agreed 2026-11-06: build (A) and (B) beside (C) once,
to know, not to assume, that they do not pay):

- Where: a prototype crate `bench/ann/` (its own `[workspace]`, not a
  member of the repository's, so CI does not build it; target dir
  `target/ann`), on `nucleus-kv` (the real KV, through its contract, as
  the engine would use it), `nucleus-kernels` (the distance kernels) and
  `nucleus-platform`.  One HNSW core (insert with the neighbour-selection
  heuristic, layered greedy search, `ef`), three storage adapters behind
  one trait (nodes and edges in and out, the delta, fold / seal / merge).
- Data: ann-benchmarks' `sift-128-euclidean` (1 M, L2) and
  `glove-100-angular` (1.18 M, cosine), their HDF5 files from
  ann-benchmarks.com, converted once by a small Python script (`h5py`)
  to raw `f32` files with the ground truth, under `target/ann-data`
  (kept like `target/tpch-sf1`; `target/` pruning must spare it).
- Same parameters for all: `M` 16, `ef_construction` 64, `ef_search`
  10–400; each model in its best reasonable form — (A) with and without a
  node cache (bounded, by key and the version read), (B) with and
  without merges.
- Measured, each on 1 and 10 threads: recall@10 against `ef_search`
  and latency / QPS (the recall–speed curve); a build by inserts — the
  throughput, the conflicts and retries with concurrent writers, the
  bytes written to the KV per row (write amplification); searches during
  writes and from an old snapshot; memory; build time.  On a quiet
  machine (`uptime`'s load and `fseventsd` checked first), the server
  not running.
- Afterwards: the numbers and the decision recorded here; the losing
  adapters deleted; the winner built into the engine — an index kind
  registered (principle 12: `CREATE INDEX … USING hnsw (v
  vector_l2_ops)`, the index method a registry, not a hard-coded B-tree),
  maintained by DML, chosen by the optimizer, `EXPLAIN` showing it — with
  the recall suite in `bench/` and tests (the graph's results against
  the exact search's, MVCC, rollbacks, crashes in a fold).

*Done — the measurement* (2026-11-06/07; `bench/ann/`, decided: **(C)**).
The three models on the real KV, one HNSW core (`hnsw.rs`: the paper's
algorithms with hnswlib's heuristic, a node's level from a hash of it;
batch inserts as ParlayANN's — Manohar et al. 2024 — prefix-doubled
batches, each new node's candidates joined with its batch's as
NN-descent's local join, Dong et al. 2011, since a batch of neighbours
in SIFT's data order otherwise misses each other: recall 0.937 at
`ef` 40, against 0.940 one at a time and 0.915 unjoined, built 4–6×
faster).  The core matches hnswlib on the same data.  ann-benchmarks'
SIFT (1 M, L2) and GloVe (1.18 M, cosine): 90 % built at once, the rest
inserted by 10 writers (a transaction a row) while one thread keeps the
index up and one searches; `M` 16, `ef_construction` 64; 2,000 queries;
µs a search on one thread, recall@10.  10 threads:

| SIFT-1M | (A) shared graph | (B) segments | (C) combined |
|---|---:|---:|---:|
| built (900 k rows) | 51.6 s | 54.2 s | 54.9 s |
| after: `ef` 60 (recall 0.93) | 706 µs | 1,227 µs (11 graphs) | **252 µs** |
| inserts, 10 writers | 1,768 rows/s | **8,162** | 5,374 |
| a search while they write (`ef` 40) | 2,636 µs | 1,295 | **751** |
| bytes written per row inserted | 1,122 | 1,070 | 2,774 |

| GloVe-1.18M | (A) | (B) | (C) |
|---|---:|---:|---:|
| after: `ef` 80 (recall 0.74) | 1,068 µs | 868 µs (3 graphs) | **415 µs** |
| inserts, 10 writers | 864 rows/s | **6,853** | 5,964 |
| a search while they write | 2,554 µs | 2,164 | **1,459** |

(A) pays a KV read a hop (~3 µs; its vector cache leaves the edges): 3–4×
(C)'s search, a fifth of its inserts.  (B) inserts fastest (a seal
builds a small graph alone) but a search visits every segment: 2–5×
(C)'s once there are several, and its size tiers (log₁₀ of the rows)
split ~10,000-row segments across two classes, so they never merged.  On
one thread (writers, upkeep and build alike) the order holds; (C)'s
single-threaded fold (~380 µs a row) then falls behind the writers, its
delta grows to ~17–25 k rows and a search meanwhile takes 2.3–3.6 ms —
the fold must get workers in the engine, and the upkeep's sizes must
keep up with the inflow, not only weigh the costs below.

pgvector 0.8 (`pgvector/pgvector:pg18`, `bench/ann/pgvector_ref.py`: the
clients in the container, `pgbench` and `psql` on its socket) on the
same SIFT data and settings: built in 58.8 s on 10 workers; at recall
~0.93 a search 894 µs (`ef` 40) where (C) takes 256 µs (`ef` 60), 5,564
searches a second on 10 clients against 20,849; at 0.99, 2,818 µs against
~650–800; 2,141 rows/s inserted by 10 writers (against 6,795), a search
meanwhile 1,571 µs (against 909 at lower recall, ~1.2 ms at the same).
Its numbers include SQL (parse, plan, protocol: tens of µs); (C)'s are
calls in a process — the engine's will tell.

*How (C) got there* — each step measured A/B on SIFT-1M, alternating:

1. **Chunks in memory, pages on disk.**  (C) was 31–35 % slower than the
   same graph in memory (230–241 µs against 171–178 at `ef` 40); cache-
   line alignment of vector pages changed nothing; 4,096-node pages
   closed the gap — the cost was three dependent loads a distance
   (directory, `Arc`, buffer).  Pages stay 64 nodes (what a write
   rewrites); in memory they are decoded into 4,096-node chunks keyed by
   a hash of their pages' ids: 176–184 µs, `ef` 200 643 against 800–825.
2. **Sealed epochs** (an LSM's immutable memtable).  The open epoch is
   closed every few thousand rows, folded or not; a closed epoch no longer
   changes, so a search reads it from memory, only the open one from the
   KV: a search over ~8.5 k delta rows 2,250–2,355 → 796–849 µs.
3. **No VACUUM after a fold.**  With epochs a search starts past the
   folded keys; the VACUUM cost a fixed ~600 ms a fold: inserts 4,600 →
   7,300 rows/s.
4. **Folds sized by cost** — an economic order quantity (Harris 1913):
   B* = √(2Kλ/h), K a fold's fixed cost (the intercept of a line fitted
   to folds' times against their rows, compactions left out — their cost
   is the patches'), λ the rows a second, h a waiting row's cost to the
   searches a second (ns a row a search, measured: ~160 from memory,
   ~470 from the KV); a seal the same with a close's cost (its
   transaction and the writes it sends back) and the KV's extra.  Sizes
   land at 230–500 rows a fold, 1,100–1,300 a seal.
5. **A directory in parts** (path copying, as a persistent B-tree): the
   page list was rewritten whole by every fold (~260 KB at 1 M rows), now
   256-page parts and a list of them — a small fold's fixed bytes ~0.
6. **Folds take closed epochs, seals close them**: a close sends ~35
   writes back (a retry sees the old epoch until the close is visible),
   so only seals pay it.

Together, with writers and searches at once: a search 1,755–1,875 → 770–
815 µs, 2.4× the searches served, for 7 % of the inserts' rate and 4 %
more bytes.

*Deletes and updates* in (C): the table is the truth —
a search checks what it found against the row in its snapshot (gone, or
its distance from the query no longer the one found: passed over;
fewer than `k` left, `ef` doubled — pgvector 0.8's iterative scans); a
delete or update writes a key (epoch, row, hash of the old vector) that
a fold turns into the node's dead mark (passed over unread); once the
dead are 2 % of the graph (or a compaction is due) a fold cuts them out —
each list through a dead node rewritten from the dead node's own live
edges by the heuristic (FreshDiskANN's delete consolidation, Singh et
al. 2021, its algorithm 4), the entry point moved off.  SIFT-1M, the
table kept beside the index (`table=1 churn=50000`): checking costs a
search ~35 µs (220 against 185 at `ef` 40: ten point reads — the engine
reads the rows anyway); 50,000 deletes and 50,000 updates by 10 writers
at 4,587 a second, a search meanwhile 1,105 µs; 100,000 nodes marked dead,
cut out in 4 consolidations; then recall 0.865 / 0.984 at `ef` 40 / 200
(0.886 / 0.988 before, a tenth of the graph replaced), 237 µs; 16
searches widened.

Not taken: (A) and (B) (deleted after this record); cache-line alignment
(no gain); a VACUUM after folds (no need with epochs); fixed fold sizes
(the cost model's choice serves 2.4× the searches).  Open: a dead node's
vector stays in its page until the graph is renumbered; the fold's
patches grow faster with small folds (3 compactions in 100 k rows
instead of 2: 2,774 bytes a row against 1,937), a cost the sizes do not
weigh yet; the KV's ~35 retries a close.

*Next — V4b, the index in the engine* (the winner built in, as planned
above).  The engine has no index-method registry yet (every index a
B-tree through `exec::index_key`; `CREATE INDEX` parses no `USING
method`, operator class or `WITH`); in steps, each measured and
committed:

1. **An index-method registry** (principle 12, as `add_storage`): a
   method validates its operator class and options, keeps its entries
   on writes, builds over existing rows, checks itself and searches; the
   B-tree stays the built-in default.  `CREATE INDEX … USING method (col
   opclass) WITH (…)` parsed; the catalog's index records a method, its
   operator classes and options (`TABLE_VERSION` 14, older still read).
2. **HNSW as a registered method**: (C)'s storage under the index's
   prefix — epochs, sealed epochs, the folded graph's content-hashed
   pages, chunks and directory parts, dead marks; a write puts its row
   into the delta (and a delete or update its dead key) in the
   statement's transaction.  The upkeep runs where GIN's pending list is
   cleaned (PostgreSQL's `fastupdate`): by the session whose commit
   takes the sealed rows past the fold's size — workers for the fold, so
   it keeps up — no background thread needed, embedded editions alike.
3. **The planner**: an access path for `ORDER BY col <op> $q LIMIT k`
   when an index's metric is the operator's, costed against the exact
   top-k (small tables, narrow filters keep exact); results checked
   against the rows read (snapshot visibility), `ef` doubled while
   fewer than `k` pass (filters included); `EXPLAIN` shows it.
4. **Settings and checks**: `hnsw.ef_search` (40) in the session,
   `WITH (m = 16, ef_construction = 64)` on the index; the integrity
   check walks the graph against the rows; tests (the graph's answers
   against the exact search's, MVCC, rollbacks, crashes in a fold) and
   the recall suite against `bench/ann`'s numbers and pgvector's.

*Done — step 1* (2026-11-07): `index_method` — an `IndexMethod`
(`define`: the columns' types, operator classes and options, defaults
filled in; `write`: told each row's change in the writing transaction,
old and new; `build`: over a table's rows; `check`: for the integrity
check) registered by `Registry::add_index_method`, the B-tree the
built-in default; `CREATE INDEX … USING method (col opclass) WITH (…)`;
the index record's method, operator classes and options (table record
version 14); `nc_catalog.indexes`' `method`, `opclasses`, `options`;
pg-compat's `indexdef` with them.  Prior art: PostgreSQL's index access
method API (`IndexAmRoutine`: `ambuild`, `aminsert`, `amvalidate`,
`amoptions`) — its shape taken; not its delete path: PostgreSQL's
`aminsert` only adds, and dead entries wait for VACUUM's
`ambulkdelete` against the heap, where here a method is told the row as
it was and as it is (no heap, no VACUUM: it sees a delete when it
happens).  Measured against b4636a3 (`phases.py`, alternating, 2 rounds,
SF 1, row tables): the load 23.18 / 22.46 s before, 22.50 / 22.37 s
after (`COPY lineitem` 8.07 / 7.85 against 7.80 / 7.68 s); the 22
queries' best 5,736 / 5,677 ms against 5,587 / 5,696 — no change past
the noise (a write asks an empty list of methods).

*Step 2, as it goes in* — one change from the bench: no epochs.  A
writer there read the open epoch validated, so a seal's close sent its
transaction back (~35 retries a close); in the engine that is a user's
`INSERT` failing with `40001` — refused.  Instead every key a write puts
is new (the delta's and the dead keys carry the row's primary key and a
nonce), so no write ever touches a key another transaction writes; a
seal *moves* the delta it sees into an immutable object (the root lists
the sealed objects) and deletes those keys, in one transaction of its
own — what an LSM's memtable flush does.  A search reads the sealed
objects from memory and the rest of the delta from the KV, each row
exactly once in any snapshot.  The price: a sealed row's vector is
written twice (into the delta, then into the sealed object).

*Done — step 2* (2026-11-07): `hnsw` — `graph` (`bench/ann`'s core, the
distances in the SQL operator's order: squared Euclidean, cosine over
vectors stored normalized, negated inner product, taxicab), `store`
(model (C) with primary keys for rows: a node's page holds its row's
key; reads that fail inside the graph's walk are noted and returned
after it), `cache` (decoded objects for the process, 1 GiB, the least
recently used dropped).  pgvector's operator classes and options
(bounds included); `NULL` and, for cosine, zero vectors not indexed.
The upkeep after a commit (`IndexMethod::after_commit`), by the
committing session, each step in a transaction of its own: a seal past
the seal's size, a fold past the fold's, both sized by the bench's
economic order quantities (at most 10,000 rows) — and keyed by
database: the process's state for an index is by `Kv::instance` (new in
the KV contract: which open database a transaction is in), its table
and its id; keyed by the table and index alone, two databases' indexes
shared it, which the tests caught (two databases in one process, as a
server has).  `hnsw::nearest` — the graph, the sealed rows, the delta;
checked against the rows, ranked by the operator's distance, `ef`
doubled while fewer than `k` are left — is what the planner's access
path will call (step 3).  Tests: recall against the exact search after
the build, with a delta, after folds, marks below 2 % and a
consolidation past it, after a table rewrite; a snapshot from before an
upkeep; each operator class; refusals; the upkeep after commits.
Measured against 5d77d7b (`phases.py`, alternating, 2 rounds): the load
23.25 / 22.53 s before, 22.55 / 22.71 s after; the queries' best
5,731 / 5,622 ms against 5,667 / 5,619 — no change (a commit asks the
methods whether an index waits: one atomic load).

*Done — step 3* (2026-11-07): `ORDER BY col <op> q LIMIT k [OFFSET o]`
(the operator or its function, either way round; `q` the same for every
row — a constant, a parameter, an enclosing row's value) over a table
with an HNSW index on `col` whose operator class is that distance's is
a scan through `Access::Nearest` — the optimizer's last pass, where it
costs less than the scan the table had: about `ef · (3 + 2m / 10)`
rows' reads (each candidate's row read as through an index, its
neighbours' distances computed), `ef` the session's `hnsw.ef_search`
(40, pgvector's default and bounds) or what the filter's selectivity
takes to leave `k + o` rows, against the rows the scan reads; never
where `ef` passes the rows over `2m` — a search with that `ef` computes
about as many distances as reading every row does.  A table not
analyzed has the rows its index holds, and keeps a key range its filter
chose.  The executor gives the scan's filter (and the hash joins' key
filters) to the search, which widens `ef` (doubling) until `k + o` rows
pass or the index has no more; the sort and the limit above stay.  One
change from pgvector: where fewer than `k + o` rows pass — the index
holds fewer (a `NULL` vector is not in it and sorts last), or the filter
was narrower than estimated and `ef` reached its most — the scan reads
every row instead, so a query asking for `k` rows gets `k` when `k`
match (this milestone's *done when*).  pgvector 0.8's iterative scans
stop at `hnsw.max_scan_tuples` (20,000) and return fewer; its planner
(`hnswcostestimate`) costs the index from the pages a search touches —
taken: the iterative widening; not taken: returning fewer rows than
exist.  Qdrant and Weaviate also turn to a full scan under a filter's
cardinality (`full_scan_threshold`, `flatSearchCutoff`) — here that
point is where the two costs meet rather than a fixed count, and it is
decided twice: by the estimate when planning, by the rows found when
running.  Tests: the plans chosen and not (another distance, the
distance not the first key or descending, an expression of it, no
limit, a narrow key range, an `ef_search` as costly as reading every
row), recall against the exact search, `hnsw.ef_search` at 1,000 equal
to it, a filter with statistics, an offset, the fallback to every row
(and a `NULL` vector last), an enclosing row's vector (`LATERAL`-style
subquery: each row's nearest is itself), deletes and updates;
`45_vector.slt` the `EXPLAIN`.  Measured against b90a048 (`phases.py`,
alternating, 2 rounds): the load 22.40 / 22.43 s before, 23.10 / 21.79 s
after; the queries' best 5,719 / 5,716 ms against 5,872 / 5,559 — no
change past the noise (one more pass over the plan, which looks for a
`Limit` over a `Sort`).

*Done — step 4* (2026-11-07): `hnsw.ef_search` and the options came
with steps 2–3; the integrity check with step 2.  New: a crash test
(`crates/sql/tests/hnsw_crash.rs`: writes, rollbacks, seals and folds on
the simulation, a crash at each of ~180 I/O operations spread over the
run, dropped unsynced writes and torn ones; after reopening the rows are
a step's, the check is clean, a search at a wide `ef` gives the exact
search's answer and the upkeep goes on) and SIFT-1M through SQL
(`bench/ann/sql.py`, run as `pgvector_ref.py` runs pgvector).  The
first run found two faults, both fixed:

- *Rows left waiting.*  After the inserts a search took ~4.3 ms: the
  upkeep ran after commits only, and forgot an index once it had sealed
  — the sealed rows waited for the next writes, scanned by every search
  meanwhile.  Now the process notes the rows left sealed until a fold
  takes them, and the upkeep runs after reads too
  (`IndexMethod::after_statement`, renamed from `after_commit`): with
  the writes stopped their rate falls, the sizes (economic order
  quantities of it) fall with it, and the next search's session folds —
  4.3 ms → 0.49 ms at `ef` 40.
- *Every candidate read.*  A search read the row of each of its `ef`
  candidates to check it.  Now it reads them nearest first by the
  graph's distance and stops at the `k`-th that passes; a row whose
  vector changed is told by its distance (the graph's, to the bit) and
  passed over — its vector now is in the index under its own.  `ef` 60
  632 → 440 µs, `ef` 200 1,416 → 946 µs.

SIFT-1M through SQL against pgvector (`docs/benchmarks.md`): indexed in
55.1 s (58.8); at recall ~0.93 440 µs a search (`ef` 60) against 894
(`ef` 40) and 11,679 searches a second on 10 clients against 5,564; at
0.99, 946 µs against 2,818; inserts by 10 writers 4,689 rows/s against
2,141.  The SQL path costs ~200 µs over the bench's calls in a process
(the driver's round trip alone ~60 µs; a profile of the server puts two
thirds of a search in the distance kernel, as in the bench).  *Open:* a
search while the writers run takes 3.1 ms — pgvector 1.6, the bench's
(C) 0.9; the rows not yet in the graph are read one by one, and the
engine's backlog (delta and sealed rows under its sizes) is larger than
the bench's, its folds slower to catch up (inserts 4,689 rows/s against
the bench's 6,795).  Next: measure the backlog as the writes run, and
search the sealed objects faster than a scan (a small graph each, or
the fold sooner).  Measured against 056f9a9 (`phases.py`, alternating, 2
rounds): the load 23.29 / 22.33 s before, 23.78 / 21.91 s after; the
queries' best 5,625 / 5,623 ms against 5,570 / 5,507 — no change past
the noise (a read now asks the methods whether rows wait: one atomic
load).

*Done — searches while rows come in* (2026-11-07; step 4's open item).
Measured first, by timing each search's parts on the server (the graph,
the sealed rows, the delta, the rows read) and each upkeep step, on
SIFT-1M through `bench/ann/sql.py` — five causes, each fixed and
measured in turn (a search while 10 writers insert, `ef` 40, µs on the
client; the server's mean in brackets; one run each, ±~15 % run to run):

| step | a search meanwhile | what it was |
|---|---:|---|
| d01fab4 | 3,093–3,276 (6,433) | — |
| the delta's range vacuumed after each seal | 1,972 (2,243) | the seals' deleted delta keys left tombstones every scan of the delta stepped over: 2 µs a row at the start, 21 by the end (a delta of ~200 rows, 0.3 → 20 ms); now ~1 µs, flat |
| the patches' levels merged (logarithmic method) | 1,298 (1,050) | a list's lookup walked every patch since the last compaction (one a fold, ~100): the graph's part rose 250 → 290 µs between compactions |
| compaction apart from the folds; the upkeep the writers' | 1,178 (912) | a fold that compacted rewrote the whole adjacency (~15,000 pages, 2.7 s) while the delta piled up to 13–23 k rows, searches 5–35 ms; and a searching session sometimes did a fold (20–70 ms) |
| a compaction's pages put in 8 MiB transactions | 1,194 (932) | 128 MiB in one transaction took the KV's path for large ones: the others' commits stalled ~0.5 s |
| seal and fold sizes from the measured row costs; the checkpoint's flush a page at a time | 1,228 (940) | the sizes assumed 470 / 160 ns a delta / sealed row — measured ~1,000 / ~100; a checkpoint's write-back held a buffer-pool shard's write lock through the shard's I/O: a search after the build's checkpoint waited 3.8 s (now 0.55) |

The last row moves the trade-off (bigger folds, fewer delta rows) rather
than the mean; the checkpoint's fix matters where a checkpoint falls
inside the writes (1 GiB of WAL; none did in these runs).  Final run, the
whole curve, quiet machine: a search meanwhile **885 µs** (3,093), 21,918
searches in 20.8 s against 6,910; inserts 4,806 rows/s (4,689); after the
inserts `ef` 60 463 µs (526); idle `ef` 60 442 µs (440).  pgvector: 1,571
µs meanwhile (at recall 0.93; ours at `ef` 40 is 0.89 — the same recall,
`ef` 60, not measured meanwhile yet).

What was taken, from where: the range vacuum is RocksDB's queue pattern
(a queue's deleted head makes every scan step over tombstones; RocksDB
bounds the iterator past them or compacts the range — here the KV gets
`Kv::vacuum_range`, the vacuum's own sweep bounded, not a second
mechanism); the logarithmic method is Bentley and Saxe's (1980) for
making a static structure dynamic — sizes as a binary counter's bits;
the compaction beside the folds is an LSM's beside its flushes (and
FreshDiskANN's background merge), its output read into memory as
RocksDB's `prepopulate_block_cache` does; the upkeep by the writers is
PostgreSQL's GIN pending list, cleaned by its inserters; the flush under
a read lock is PostgreSQL's checkpointer (`SyncOneBuffer` writes a
buffer under a share lock on it).  Looked at, not taken (yet): a small
graph per sealed object (Milvus' interim index for growing segments,
Lucene's per-segment graphs) — the sealed rows stay under ~3,000, where
Milvus, Weaviate and Qdrant all still scan (their thresholds 5,000–
20,000); int8 or RaBitQ codes for the sealed scan (Lucene's int8, FAISS's
`IndexScalarQuantizer`) — the sealed rows are ~60 µs of a search now;
early abandoning (partial distances against the `ef`-th, Bei and Gray
1985) — not measured; a background thread for the compaction (the
engine's checkpointer is one) — a writer's statement pays ~0.7 s once
every ~50,000 rows instead, deterministic in the simulation.  Left: the
rows a search reads cost ~17 µs each while the table is written (5.6
idle) — the KV's, not the index's; the compaction rewrites every page
the patches touch (with random inserts, all: 128 MiB at 1 M rows,
~2.5 KB a row inserted).  Measured against d01fab4 (`phases.py`,
alternating, 2 rounds): the load 21.83 / 21.19 s before, 21.78 / 21.69 s
after; the queries' best 5,481 / 5,540 ms against 5,433 / 5,526 — no
change.

*Measured on the way* (2026-11-06): `docs/benchmarks.md`'s TPC-H table
re-run on a quiet machine (`fseventsd`, at 99 % and 11 GB after 137 days,
restarted) — 0.66× row, 0.24× column.  Its Q8 on column tables still
read 113 ms against ~80 before: not the adaptive join but a near tie.
The `part`–`lineitem` join's lookups (an index nested loop per `part`
row, ~1,333 estimated) against hashing `lineitem`'s matches cross at
~1,260–1,340 probes depending on `ANALYZE`'s sample, so about one load
in four looks up — 100–140 ms where the hash join takes 87–91 — the
base build (7ba43b3) as often (one in four loads) as this one.  Open: the
column-table lookup is costed ~10 % too cheap at that point
(`COLUMN_LOOKUP`), and a plan that flips on the sample's noise wants
either a calibrated cost or the adaptive join's own remedy — look up
when the probes turn out few, hash otherwise — in the other direction
too (an index nested loop that turns to a hash join past the crossing).

**V5 — filtered search** (2026-11-07).  Measured first
(`bench/ann/filtered.py`: SIFT-1M, a column `a` 0–999 from a hash of the
row's id — independent of its vector — and a B-tree on it; `ef` 40, 500
queries, recall@10 against the exact answer among the rows that pass, µs
a search on one client; pgvector 0.8 beside it in its three
`hnsw.iterative_scan` modes).  Before (405cb4a): the filter was applied
to the rows the graph found, `ef` doubled until `k` passed — 50 %:
recall 0.833 (unfiltered 0.89: `ef` 40 candidates of which ~20 pass);
10 %: 0.908, 1,178 µs; 1 %: 0.986, 8,733 µs (about a thousand rows read
to find ten that pass); 0.1 %: through the B-tree and sorted, 1,864 µs.
pgvector: without iterative scans 4.2 and 0.45 rows returned of 10 at
10 % and 1 %; relaxed 0.935 / 2,312 µs and 0.987 / 12,537 µs; the B-tree
at 0.1 %, 521 µs.

Three changes:

- **`ef_search` counts the rows that pass** — what a filtered HNSW
  search's result list holds (hnswlib's filter functor, ACORN's
  predicate-aware search): the graph is asked for `ef` over the share
  the optimizer expects to pass, and again, sized by the share that
  passed among the rows read near the query, where that falls under half
  the expected (a filter that goes with the vectors).  Recall then holds
  whatever the filter's share (`Ask`; the optimizer's `share`).
- **Few rows: searched among exactly.**  A filter with a B-tree range
  keeps it in the plan (`Access::Nearest`'s `within`; `EXPLAIN` `… among
  ix: …`); the executor reads the range's entries (primary keys only, a
  chunk at a time) and the index computes the distances of those rows'
  vectors from memory — found by a hash map of primary keys to nodes
  (the fold's, now a hash map behind a read-write lock), in two passes
  (the lookups, then the vectors in node order) on up to
  `parallel_workers` threads of 2,048 rows — then reads about `k` rows.
  Recall 1.  Where it stops paying: reading `p` entries (~70 ns each,
  ~35–115 ns to find each vector, ~0.17 µs in all) against the graph's
  search at the share `p / n` — asked for `ef · n / p` candidates (~3.7
  µs each) and reading ~`k · n / p` rows (~3.4 µs each) — equal at `p =
  √(n · (22 · ef + 20 · k))`, 33,000 of a million for `k` 10 at `ef` 40
  (`hnsw::among_most`); past it, or estimated past twice it (then the
  range is not read), the graph.  Fewer than `k` found: the range read
  and sorted, as before.  (The first cut, `√(20 · k · n)` without the
  graph's own cost, sent a group's 10,000 rows of 100,000 to the graph:
  4.7 ms against 1.4 searched among.)
- **Row-level security is a filter like any other**: the policy and the
  statement's filter above the barrier become the search's `checks`,
  evaluated in that order on each row read, so the search goes on until
  `k` rows the session may see pass and no predicate of the statement
  sees a hidden row (before, `Plan::Secure` stopped the planner from
  using the index at all).

| filter | before (405cb4a): recall, µs | now | pgvector 0.8 (`relaxed_order`) | what changed |
|---|---|---|---|---|
| 50 % (`a < 500`) | 0.833, 428 | **0.926, 587** | 0.891, 1,021 | `ef` counts passing rows: 80 asked |
| 10 % (`a < 100`) | 0.908, 1,178 | **0.982, 1,760** | 0.932, 2,533 | 400 asked (the range, ~100,000, not read) |
| 1 % (`a < 10`) | 0.986, 8,733 | **1.0, 1,425** | 0.987, 14,477 | 10,000 searched among, exactly |
| 0.1 % (`a = 7`) | 1.0, 1,864 | **1.0, 313** | 1.0, 604 (its B-tree) | 1,000 among, ~10 rows read instead of 1,000 |
| 0.01 % (`a = 7 AND id % 10 = 3`) | 1.0, 1,887 | **1.0, 516** | 1.0, 471 (its B-tree) | among `a = 7`'s 1,000; ~100 rows read for the rest of the filter |

(One run each, back to back, `bench/ann/filtered.py`; ±~10 % run to run
on this machine.  pgvector without iterative scans returns 4.1 and 0.44
rows of 10 at 10 % and 1 %; `strict_order` 0.851 and 0.944 there.)  The
50 % and 10 % rows trade time for recall — `ef_search` 20 gives back the
time; the recall now holds from 50 % to 0.01 %, as the step's *Done
when* asks.  The last row is the one pgvector wins: a filter part the
B-tree does not cover is checked on the rows, read one at a time (~3.4
µs each — the KV's point read; pgvector's bitmap heap scan reads its
1,000 in page order) — a batched, key-ordered read of the rows is the
remedy, a KV step of its own.

On the way (1 %, the search's parts timed on the server): the entries
read one at a time, allocated each, 110 ns → through `scan_chunks` 70;
the vectors found row by row (hash lookup, the node's row compared, the
vector) 177 ns → two passes, the rows' nodes then their vectors in node
order, 115 → on four threads, 35.

What was taken, from where: the exact search over a filter's rows when
they are few is Lucene's (its HNSW query builds the filter's bit set, and
searches exactly when its cardinality is at most `k` or when the graph
search visits more nodes than it holds) and Weaviate's
(`flatSearchCutoff`, 40,000 by default: an allow list from its inverted
index below it searched flat) — here the cutoff is derived from measured
costs rather than set, and the vectors come from the index's memory, not
the rows; `ef` counted among passing rows is hnswlib's (a filter functor
keeps rejected nodes out of the result list) and ACORN's (Patel et al.
2024); the second round sized by the share seen is pgvector 0.8's
iterative scan, sized rather than doubled.  Not taken: ACORN's
predicate-agnostic graph (γ·M neighbours a node, two-hop expansion over
passing nodes) and Qdrant's filterable HNSW (extra edges by payload
value) — both evaluate the filter per node during the walk, which needs
the filter's columns in the index (an `INCLUDE` list, as Qdrant's payload
or Milvus's scalar fields: later, measured against this); Milvus's and
Lucene's bit set for every filter — building it reads every passing
entry (100,000 at 10 %: ~7 ms), more than the search; software prefetch
of the vectors (hnswlib, FAISS) — the workspace forbids `unsafe`; the
two passes get the memory-level parallelism instead.

- **`PARTITION BY`: a graph per value of a column** — for tenants each
  too many rows for the exact search, a graph of their own size each:
  `CREATE INDEX … USING hnsw (v) PARTITION BY (tenant_id)`.  The
  columns follow the vector among the index's columns (so a column
  dropped or retyped takes the index with it, as any other's), the
  method's `partition_by` option their number; each graph's keys are
  under the index's prefix, `p` and the values key-encoded, with its own
  upkeep, delta, folds and map of rows.  A row whose values change moves
  between graphs (a dead key in one, a delta row in the other).  The
  optimizer uses it where the conditions — the filter's or a
  row-level-security policy's, a tenant's `tenant_id =
  current_setting(…)` — fix every partitioning column; not otherwise.
  A partitioned *table*'s partitions already have an index each, so a
  graph each: the search now goes into each partition for `k` rows
  (under an `Append`, the sort and limit above merging), or the one
  pruning leaves.

| ten groups of 100,000 (`g = 3`) | recall | µs |
|---|---|---|
| one graph (`ef` 400 asked: 10 % pass) | 0.981 | 1,882 |
| `PARTITION BY (g)`: the group's graph, `ef` 40 | 0.939 | 455 |
| pgvector, one graph, `relaxed_order` | 0.938 | 2,352 |
| a tenth of the group (`a BETWEEN 300 AND 309`), partitioned: among 10,000 | 1.0 | 1,405 |

Indexed in 45 s against 67 (ten graphs of 100,000).  A filter that goes
with the partition's value (here `a`'s range lies inside `g = 3`) is
estimated as independent — 1 % of the group instead of 10 % — so the
graph would be asked for ten times the candidates; the exact search
among the range covers it here, and cross-column statistics are the
optimizer's to add.

What was taken, from where: a graph per value is Milvus's partition key
(its collections hash a key column to partitions, each with its own
index, a search's `expr` on the key choosing one) and Weaviate's
multi-tenancy (a shard, and so an HNSW graph, per tenant), and Qdrant's
`is_tenant` payload index (which instead keeps one graph and orders its
storage by tenant, building extra intra-tenant edges); here the graphs
are an index's own and the choice the optimizer's — from the statement's
conditions or a policy's, so a tenant's row-level security picks its
graph without the application naming it.  Not taken: Milvus's hashing
of the key to a fixed number of partitions (many small tenants then
share a graph and filter again); a graph per value grows with the
values, and the many small ones are where the exact search pays.

Measured against 405cb4a (`bench/phases.py`, alternating, two rounds):
nothing outside vectors moved — TPC-H SF1 load 24.95 / 22.89 → 23.13 /
26.90 s, the queries' sums 6,288 / 6,069 → 5,947 / 5,941 ms (noise);
SIFT-1M unfiltered through SQL as before (`ef` 40: 0.891, 391 µs; while
rows come in 827 µs).

**V6a — quantized graphs** (2026-11-07).  `CREATE INDEX … USING hnsw (v)
WITH (quantize = 'half' | 'int8' | 'binary' [, rescore = n])`: the graph
holds codes — in memory, in its pages, in its delta — and takes its
distances between them; the search reads `rescore` rows for each it
returns (the graph asked for at least `k · rescore`), and the rows'
exact distances order the answer, as they did before.  Codes, one form
for the graph's inserts and the query alike (symmetric):

- `half`: IEEE halves, round to nearest even (`to_half`; the kernels
  decode a half by moving its bits and scaling by 2¹¹², Giesen's
  branch-free conversion — the first cut, a branch for subnormals, kept
  the compiler from vectorizing and built 2.5× slower than `f32`).
- `int8`: a byte a component, `m + s · byte` with the vector's own least
  component and step, after a 16-byte header (`m`, `s`, the bytes' sum,
  the norm² they stand for) — a distance is one `u8 · u8` integer dot
  product (`dot_u8`) and a few multiplications: Lucene's corrective
  terms over a per-vector range rather than its quantiles over a
  segment, so nothing is trained and a row coded once stays coded.
- `binary`: a bit a component, above the vector's mean or not; the bits
  that differ (`hamming`).  Above zero, pgvector's `binary_quantize`,
  gave 0.06 recall on SIFT, whose components are all positive; above
  the mean, 0.34 — bits are for embeddings of hundreds and thousands of
  components.

Measured (`bench/ann/sql.py --quantize`; 1,000 queries; ef 40; the
row-reading cost included):

| OpenAI `text-embedding-3-large`, 99,000 × 1536, cosine | a vector's bytes | built | recall | µs a search |
|---|---|---|---|---|
| `f32` | 6,144 | 43.2 s | 0.926 | 1,414 |
| `half` | 3,072 | 35.8 s | 0.926 | 1,356 |
| `int8` (rescore 2) | 1,552 | **11.1 s** | 0.929 | **1,033** |
| `binary` (rescore 5) | 192 | **5.3 s** | 0.922 | 1,588 |
| `binary`, rescore 10 | 192 | 5.5 s | 0.964 | 2,580 |

| SIFT-1M (900,000 built, 100,000 by ten writers) | a vector's bytes | built | recall | µs a search | inserts a second | a search while they come |
|---|---|---|---|---|---|---|
| `f32` | 512 | 54.9 s | 0.891 | 363 | 4,862 | 899 µs |
| `half` | 256 | 48.8 s | 0.891 | 405 | 5,033 | 712 µs |
| `int8` (rescore 2) | 144 | **29.0 s** | 0.892 | **291** | **6,275** | 708 µs |

(SIFT's components are integers up to 255: halves hold them exactly.
Run to run ±~10 %.)  `int8` is the one that pays everywhere — a quarter
of the memory, the build 1.9–3.9× faster, searches faster at the same
recall (a distance a quarter of the bytes, an integer dot product);
`half` halves the memory at no cost but gains no time; `binary` is a
thirty-second of the memory and builds 8× faster, with recall at the
`f32` graph's at `ef` 40 on embeddings, where its reads are cheap
enough.

Measured against 24c5627 (`bench/phases.py`, alternating, two rounds):
TPC-H SF1 load 25.89 / 26.26 → 25.59 / 24.10 s, the queries' sums
6,072 / 5,933 → 6,049 / 5,986 ms: noise.

Against pgvector on the same embeddings (its documented ways: an
expression index on `v::halfvec(1536)`; one on
`binary_quantize(v)::bit(1536)`, the nearest 50 re-ranked in a
subquery), the server's own time (`EXPLAIN ANALYZE`, 200 queries — the
clients differ) at equal recall: ~0.95, `f32` 1,149 µs and `int8` 634
against `vector`'s 1,571 (`halfvec` 1,229 at 0.947); ~0.99, 2,843 and
1,149 against 4,919.  Built in 42.5 s (`int8` 11.8) against 136.7
(`halfvec` 81.2, `bit` 22.7).  The bits are even (0.922 at 1,097 µs,
pgvector's 0.916 at 1,099).  pgvector reaches a recall at a lower
`ef_search` (0.949 at 40, here 0.951 at 60; SIFT the same) — its graph
is the better per candidate, nucleus's the cheaper per candidate; why,
is worth a look beside V7.  Found measuring: a query vector from a
subquery is not taken by the index (V7, 1).

The binary codes' searches are the rows they read: 50 rows of 6 KB at
~12 µs each (a vector of 1536 components is stored out of line,
§M14.26: a point read and a chunk's) — on the server, a search reads its
10 rows in 0.80 ms over `f32`, 20 in 0.48 over `int8`, 50 in 0.96 over
bits.  Reading a row's large values with it is the remedy, a storage
step of its own.

What was taken, from where: quantized graphs with the full vectors read
again for the answer are Qdrant's (scalar int8, binary; `oversampling`,
`rescore`), Elasticsearch's (`int8_hnsw`, BBQ; `rescore_vector.oversample`)
and Weaviate's (SQ, BQ, `rescoreLimit`); here the full vectors are the
rows, which a search reads anyway, so rescoring is the reading of more of
them.  Lucene's int8 corrective terms (the per-vector constants that make
a distance one integer dot product), over a per-vector range; FAISS's and
USearch's `f16`/`i8`/`b1` codecs.  Not taken: product quantization (FAISS
IVF-PQ, DiskANN) — trained codebooks, retrained as data drifts; RaBitQ
(Gao and Long 2024, Elasticsearch's BBQ) — a random rotation and a
centroid, so trained state per graph, and asymmetric distances (the query
kept in `f32`): measured against these bits later, where binary codes'
recall at few rows read is what is wanting; Lucene's quantiles over a
segment — the graph is never rewritten whole, so codes must not depend on
what came later.

**V6b — sparse vectors** (2026-11-07).  `sparsevec` / `sparsevec(n)`,
pgvector's type: the non-zero components only (positions and values),
text `'{1:0.5,4:2}/10'`, up to a billion dimensions and 16,000 non-zero;
equality and hashing; the four distances over the positions merged, as
operators and through the vector's functions (which take either type);
casts to and from `vector`; exact searches by `ORDER BY v <#> q LIMIT k`.
The first cut decoded both values into vectors for each distance (two
allocations a row, the query's too): 5.6 ms a search over 20,000 rows;
read in place from the bytes, 4.6.

Measured (`bench/ann/sparse.py`: SPLADE-like rows — a vocabulary of
30,522, ~120 terms a document and ~20 a query by a Zipf law, log-normal
weights — 100,000 rows, 200 queries, `ORDER BY v <#> q LIMIT 10`, one
client):

| 100,000 rows | loaded | recall | ms a search | how |
|---|---|---|---|---|
| nucleus | 6.1 s (`INSERT`s of 1,000) | 1.0 | 21.0 | a scan and a top-k on the workers (V7.3: not one thread, as first read) |
| pgvector 0.8 | 2.1 s (`COPY`) | 1.0 | 17.9 | a parallel scan, two workers and the leader |

(Read at first as one thread's ~210 ns a row, the sort above the scan
taken for serial; V7.3 found it on the workers already — ~670 ns a row
on one thread, 67 ms.)

What was taken, from where: the type, its text form and its limits are
pgvector's (0.7), so its clients' code works; positions and values apart
(two arrays, as pgvector's and Qdrant's sparse vectors) so a merge reads
each densely.  Not taken: an HNSW index over sparse vectors (pgvector's,
for up to 1,000 non-zero) — a graph over vectors that share few
positions finds its neighbours poorly, and its distances need the
merge; sparse retrieval's index is an inverted one — Qdrant's sparse
index, Elasticsearch's `sparse_vector` and Milvus's
`SPARSE_INVERTED_INDEX` are posting lists with WAND or block-max
pruning — which is §M14.21's full-text index with the weights as the
model's instead of BM25's: built there, for both.

Measured against b297042 (`bench/phases.py`, alternating, two rounds):
TPC-H SF1 load 23.44 / 22.15 → 22.81 / 22.21 s, the queries' sums 5,501 /
5,678 → 5,548 / 5,589 ms: noise.

**V7 — closing §M14.12** (recorded 2026-11-07, begun 2026-11-09).  What
*Done when* still asks and what V4b–V6 measured as wanting, in this
order (each measured as the steps before it, against the commit before,
and against pgvector where it has the same):

1. **The query vector from a subquery or a row** — `ORDER BY v <=>
   (SELECT v FROM items WHERE id = $1) LIMIT k` ("more like this item",
   the commonest search in an application after the client's own vector)
   is planned today as a scan and a sort: the optimizer's `nearest` takes
   a constant, a parameter or an enclosing row's value only.  An
   uncorrelated scalar subquery is one value for the statement: evaluated
   once before the search (as PostgreSQL's InitPlan, which pgvector's
   index takes), it is a query vector like a parameter.  Then `LATERAL`
   (each row's `k` nearest — a join's inner side searching per outer
   row), the form recommendation and deduplication queries take.  Found
   measuring V6 against pgvector, whose benchmark sends the query by an
   id for that reason.
2. **A row's large values read with it** — a vector of 1536 components
   (6 KB) is past §M14.26's 2,000-byte threshold, so every row a search
   reads is a point read and a chunk's scan (~12 µs on the server against
   ~3.4 for a row inline): at `rescore` 5 that is most of a binary
   search's time.  Options to measure: the threshold per type (a vector
   is read whole with its row nearly always — pgvector's `vector` is
   `STORAGE EXTERNAL`, TOASTed past 2 KB too, so the same cost is theirs),
   or the chunks keyed beside the row so one range read takes both.
3. **Exact searches in parallel** — a top-k per morsel, merged (the sort
   above a scan is serial: §M14.20 V4's *Left*); sparse exact searches
   then run on the workers (100,000 rows: 21 ms on one thread, pgvector
   17.9 on three), dense exact searches and every `ORDER BY … LIMIT`
   with them.
4. **`sparsevec` in the protocols and the six drivers** — a type of its
   own in each (today `Other { type_name, bytes }` / `nucleus.Ext`),
   pgvector's binary form and a `pg_type` row on pg-compat, so psycopg
   with pgvector's `SparseVector` works as it does for `vector`.

**V7.1 — the query vector from a subquery or a row** (2026-11-09).
A subquery that does not read the scanned row — uncorrelated, or reading
only enclosing rows — is a query vector like a parameter
(`optimizer::fixed`): evaluated once for the statement (its rows are
cached already) and handed to the index.  `LATERAL` before a subquery or
a function (functions are lateral without it, as PostgreSQL's): bound
with the items before it as the row it runs for, its join is an index
nested loop's (`JoinAlgo::Lookup`) — the executor's per-row run that
existed for lookups, its right side now any plan — so `FROM items a,
LATERAL (SELECT b.id FROM items b ORDER BY b.v <=> a.v LIMIT 10)` is a
search per row.  Those runs go to the workers as lookups do (a sort and a
limit above a scan or an index method's search), each worker running a
row's search whole; a worker's transaction now gives the index method
its shared reads and the database's instance (`parallel::SharedTxn`),
and a worker does not share its work out again (`budget::workers` 1 on a
worker).  `RIGHT` / `FULL JOIN LATERAL` are refused (`42P10`, as
PostgreSQL); a subquery naming an item before it without `LATERAL`
gets a hint to mark it.

Measured (`bench/ann/related.py`, OpenAI's 1536-dimensional embeddings,
99,000 rows, cosine, m 16, `ef_search` 40, both systems' clients sending
an id):

| | nucleus before | nucleus | pgvector 0.8 |
|---|---|---|---|
| "more like this": `ORDER BY v <=> (SELECT v FROM t WHERE id = $1) LIMIT 10` | 101 ms (every row read) | **1.23 ms** (server 0.91), recall 0.965 | 1.48 ms (server 1.55), recall 0.975 |
| 1,000 rows' ten nearest each, `LATERAL` | — (no `LATERAL`) | **158 ms** on the workers, 902 on one thread; recall 0.982 | 1,442 ms (one process), recall 0.987 |

pgvector's recall at the same `ef` is the higher, as V6 found.

What was taken, from where: an uncorrelated subquery evaluated once
and passed to the index scan as a value is PostgreSQL's InitPlan (which
pgvector's index takes); `LATERAL` is SQL:1999's, its semantics and
errors PostgreSQL's (9.3), and planned as PostgreSQL plans it when the
inner side has an index — a nested loop whose inner side is
parameterized by the outer row.  Not taken: decorrelating the lateral
subquery into a join (Neumann and Kemper, "Unnesting Arbitrary Queries",
2015; DuckDB's) — a top-k per row unnested becomes a window over every
pair (`row_number() OVER (PARTITION BY a.id ORDER BY distance)`), n × N
distances where the index searches n times; it pays for lateral
subqueries without an index to search, kept for when one is measured
wanting.  Beyond PostgreSQL: the per-row runs on the workers (its nested
loop's inner index scans run on one process unless the outer side is a
parallel scan).

**V7.2 — a row's vectors read with it** (2026-11-09).  A type says
whether its values are read with their row nearly always
(`Type::stored_with_row`: `vector` and `sparsevec` do); such a value
stays in the row however long (`large::out_of_line_value`), and the
KV's B+tree keeps a cell past 2,048 bytes on its own overflow pages —
one read of the row's key takes it, where §M14.26's chunks were a
second range read under another prefix.  Other long values (text,
bytea, arrays) keep the chunks: a scan reading a few columns skips them
unread.

Measured (OpenAI's 1536-dimensional embeddings, 99,000 rows, `bench/ann/sql.py
--no-inserts`, 1,000 queries; `bench/vectors/knn.py`, 100,000 × 768;
the build before against this one, alternating, two rounds):

| | vectors out of line | in the row |
|---|---|---|
| `binary` (rescore 5), `ef` 40: the server's µs | 1,080 / 1,107 | **997 / 979** |
| the same through the client | 1,611 / 1,608 | 1,513 / 1,495 |
| `binary` built | 8.3 / 6.9 s | 6.3 / 4.1 s |
| `f32`, `ef` 40: the server's µs | 916 / 906 | 916 / 875 |
| exact search, 100,000 × 768 | 42.0 / 41.7 ms | **36.4 / 36.3 ms** |
| "more like this" (`bench/ann/related.py`, `ef` 40): client, server | 1,226 µs, 907 | **1,097 µs**, 884 |
| 1,000 rows' ten nearest, `LATERAL`: workers, one thread | 158 ms, 902 | **154 ms**, 865 |

Less than V6 guessed (~12 µs a row, 600 of a binary search's 1,100):
a 6 KB value is still a page's read beside the row's, on the overflow
pages as it was in the chunks; what went is the second lookup, ~2 µs a
row by the difference.  The `f32` search reads
the vectors from its graph, not from the rows: unchanged.

What was taken, from where: a value kept with its row past a page's
share is SQLite's overflow chain (as nucleus's KV has had it); deciding
it per type is PostgreSQL's `STORAGE` (`PLAIN` / `MAIN` / `EXTERNAL`)
set by the type's default, which pgvector leaves at `EXTERNAL` — TOASTed
past 2 KB, its rows' vectors a second lookup as ours were.  Not taken: a
per-column `ALTER TABLE … SET STORAGE` — one rule per type until a
column is measured wanting another.

**V7.3 — exact searches in parallel: they were** (2026-11-09).  V6b's
"one thread" was wrong: `ORDER BY distance LIMIT k` over a scan has run
on the workers since §M14.20 V10 (`vector::sorted`: a top-k per morsel,
`spill::Top`, merged) — V6b read 21 ms as one thread's from the per-row
cost it guessed, not from a run with `parallel_workers = 1`.  Measured
now (`bench/ann/sparse.py`'s rows, 100,000 × ~120 terms, a query of ~20):

| | `parallel_workers` default (10 CPUs) | 1 |
|---|---|---|
| `ORDER BY v <#> q LIMIT 10` | 20.3 ms | 67–76 ms |
| `count(*)` (the scan alone) | 3.7 ms | 12.6 ms |

The workers give 3.4×, the scan alone the same — the page reads, not the
distances, bound it.  `crates/sql/tests/vector.rs`
(`exact_vector_searches_run_on_the_workers`) now holds dense and sparse
searches to it: one thread's rows, run on the workers.

Profiled on one thread (`sample`): the inner product ~145 ns a row of
~525, the rest the row executor's (values copied out of the page, the
parameter's `$1::sparsevec` cast evaluated per row, allocations).
Tried and not taken: a galloping intersection for a short vector against
a long one (Baeza-Yates; Lucene's and Roaring's) — the same sums, but
20.1 / 19.8 ms against 20.3 / 20.7 and 69–70 against 68–76 on one
thread: noise, so the merge stays.  What would cut the distance is the
query scattered once into a dense table (~120 lookups a row, no
comparisons) — a per-statement state for a constant argument that the
function interface does not have yet; and what cuts a sparse search is
not reading every row — §M14.21's inverted index.

**V7.4 — `sparsevec` in the protocols and the drivers** (2026-11-09).
Each driver has a type of its own: Rust `Value::SparseVector { dims,
indices, values }`, C `NC_SPARSEVEC` (`as.sparse`, `nc_sparsevec`), C++
`nucleus::SparseVector`, Python `nucleus.SparseVector`, JavaScript
`SparseVector`, Java `SparseVector` — positions from 0, ascending, sent
and read as `ncp` tag 11 with the type's canonical bytes, in rows and
batches.  pg-compat lists `sparsevec` in `pg_type` (oid 13203, `_sparsevec`
13204) and speaks pgvector's binary form (`sparsevec_send` /
`sparsevec_recv`: dimensions, count, an unused `int32`, positions,
values, big-endian).  Found writing it: a registered type's parameter
was checked through its text form but kept as sent — a sparse vector
with its positions out of order passed the check and was stored so,
unequal to the same vector in order — and malformed bytes reached
`Sparse::of` unmeasured; the parameter is now kept as the check reads it
back (canonical) and refused with the type's SQLSTATE.

Measured against d651f4e (`bench/phases.py`, alternating, two rounds):
TPC-H SF1 load 22.4 / 22.6 → 22.6 / 22.8 s, the queries' sums 5,733 /
5,713 → 5,630 / 5,521 ms: noise.

*Done when*, checked whole (2026-11-09): the law kit passes on `vector`
(V1) and on `sparsevec` (V6b); the capabilities above work through SQL,
`ncp` and the six drivers (V1, V2b; `sparsevec` V7.4), and through
pg-compat for pgvector's clients; exact search matches numpy's brute
force (`bench/vectors/knn.py`: 0 of 20 answers differ); the index reaches
recall 0.93 at 442 µs and 0.99 at 903 on SIFT-1M through SQL, and 0.95
at 634 µs (`int8`) on OpenAI's embeddings (V4b, V6a); filtered searches
keep recall 0.93–1.0 from 50 % of the rows to 0.01 %, `k` rows whenever
`k` pass (V5).  §M14.12 is done.

§M14.21 follows: the inverted
index there is sparse vectors' index too (term weights from a model in
place of BM25's), so learned sparse retrieval and hybrid search come as
one.  Not in V7: RaBitQ-style binary codes (asymmetric distances, a
rotation) — kept for when binary recall at few rows read is what an
application lacks.

**Done when:** the law kit passes on `vector`; every capability above
works through SQL, `ncp` and all six drivers; exact search matches a
brute-force reference; the ANN index reaches a set recall at a set
latency on a public dataset; and filtered searches keep that recall at
filter selectivities from 50 % down to 0.01 %, always returning `k` rows
when `k` matching rows exist.

### §M14.21 — Full-text and hybrid search ✅

Retrieval for AI applications (RAG) needs words and meaning together:
vectors alone miss exact names, codes and rare terms; keywords alone miss
paraphrases.  Both, with filters, in one query and one transaction:

- **Full-text search**: an inverted index (`CREATE INDEX … USING text
  (body)`) kept transactionally consistent with the table, BM25 ranking,
  phrase, prefix and fuzzy matching, highlighting and snippets.
- **Analyzers are modules** (principle 12): tokenizers, stemmers,
  stop-word lists and per-language rules registered like types, chosen
  per index or column; built in: a Unicode word tokenizer, the common
  European languages (Hungarian included) and a language-neutral n-gram
  analyzer.
- A search syntax in SQL — `body @@ 'nucleus AND (database OR engine)'`
  with `score(body)` — and PostgreSQL's `tsvector`, `tsquery` and
  `to_tsvector` as a compatibility layer over it (principle 11).
- **Hybrid search**: text and vector scores fused in one query —
  reciprocal rank fusion (`rrf(...)`) and weighted combination — with
  filters, as a single plan the optimizer runs in one pass over the
  candidates, not as two queries merged in the application.

  ```sql
  SELECT id, title FROM docs
  WHERE tenant_id = $1
  ORDER BY rrf(score(body @@ $2), v <=> embed($2)) LIMIT 10;
  ```
- Columnar and partitioned tables (§M14.20) are searchable the same way.

**Done when:** BM25 rankings match a reference implementation on a public
corpus; hybrid search beats text-only and vector-only on a public
retrieval benchmark (e.g. BEIR) at a set latency; and text indexes stay
consistent with their tables under crash and concurrency tests.

**The design and its steps** (recorded 2026-11-09, begun the same day).
What others do, read from their docs and sources:

- *Storage.*  Lucene and Tantivy write immutable segments; a delete is a
  live-docs bitset written anew per commit (Tantivy: `segid.opstamp.del`,
  never modified); merges are tiered (Lucene: 10 a tier) or logarithmic
  (Tantivy: 8 at once).  SQLite FTS5 — the nearest to an index inside a
  transactional store — keeps segments as blobs in its own tables, one
  new level-0 segment a transaction, a delete as a *tombstone entry* in
  the newer segment, merged incrementally by the writers (automerge 4).
  PostgreSQL GIN buffers writes in a pending list every search scans
  (fastupdate) and stores no term frequencies or lengths: `ts_rank`
  reads each matching row and has no IDF.  ParadeDB serializes Tantivy's
  segments into Postgres pages and checks each document's visibility;
  DuckDB's `fts` is tables rebuilt by hand or kept by triggers.
- *Postings.*  Lucene and Tantivy both settled on blocks of 128 bitpacked
  deltas (and 128 frequencies) with a VInt tail; SIMD-BP128 decodes ~2×
  faster than PFOR or varint (Lemire & Boytsov 2015).  Each block's skip
  entry keeps its best (frequency, length) pair — the impacts that
  block-max pruning reads.
- *Top k.*  Block-Max WAND (Ding & Suel 2011) made Lucene 8's term
  queries 3–7× faster and disjunctions up to 15×; Lucene moved top-level
  disjunctions to block-max MaxScore in 2022 (fewer per-hit costs, better
  at many terms or large k; Grand, Muir, Ferenczi & Lin 2020).  The price
  is an exact count of matches, which a top-k query does not need.
- *BM25.*  Kamphuis, de Vries, Boytsov & Lin (2020) compared eight
  variants: no significant differences, and exact lengths cost nothing
  over Lucene's one-byte norms.  Lucene's IDF ln(1 + (N − n + ½)/(n + ½))
  is never negative.  The reference with published four-decimal numbers
  on BEIR is Anserini (Lucene: k1 0.9, b 0.4, UAX #29 words, possessives,
  lower case, Lucene's English stop words, Porter): nDCG@10 SciFact
  0.6789, NFCorpus 0.3218, ArguAna 0.3970, FiQA 0.2361.
- *Hybrid.*  Reciprocal rank fusion (Cormack, Clarke & Büttcher 2009,
  k 60) needs no scores; a convex combination of normalized scores beats
  it in and out of domain with one weight to tune (Bruch, Gai & Ingber
  2024).  Elasticsearch, OpenSearch, Weaviate and Vespa offer both;
  pgvector's README fuses `ts_rank_cd` and vectors in the application.
  Learned sparse retrieval (SPLADE) is served from an ordinary inverted
  index with the model's weights as impacts and a dot product as the
  score (Anserini `-impact`).

What nucleus takes, and how it differs:

1. **(C) again** — the model §M14.12 V4 chose: writes go to delta keys
   in the statement's own transaction (nonces, so writers never
   conflict), seals and merges run apart, sized by cost (V4's economic
   order quantity), into immutable content-hashed segments.  But where
   the graph is only *close* to the table (a search re-reads its rows),
   a text index is *exact* in every snapshot: a segment is installed by
   the transaction that removes the deltas it holds, a delete is a
   tombstone (FTS5) that a seal turns into new alive-bitsets for the
   older segments it hits (Tantivy) and into negative counts for the
   statistics.  N, the total length and every term's document count are
   then those of the snapshot's rows — BM25 a function of the data, not
   of how far merging got (Lucene counts deleted documents until they
   merge away).
2. **Postings in blocks of 128** (bitpacked document deltas, frequencies,
   positions apart), exact lengths, and each block's maximum score
   contribution kept beside it; top k by **block-max MaxScore**.
3. **Analyzers are registered** (principle 12), resolved by name when an
   index is defined, chosen per index; built in: UAX #29 words (tables
   generated from Unicode 16, checked by its `WordBreakTest.txt`), case
   folding, stop-word lists, Snowball stemmers (BSD; generated Rust,
   checked by Snowball's vocabularies — English, Porter, Hungarian,
   German, French and the other common European languages) and an n-gram
   analyzer.  No dependency in the core.
4. **One meaning with or without the index**: `body @@ q` evaluated on a
   row analyzes it as the index on that column would; the index is an
   access path, never a different answer.

The steps, each measured as §M14.12's were:

- **T1 — analyzers**: the registry, the built-ins above, `tokenize(text,
  analyzer)` to see what an analyzer does.
- **T2 — the query language without an index**: `@@` with terms, AND,
  OR, NOT, phrases, prefixes and fuzzy terms, evaluated on rows.
- **T3 — index access for any method.**  The planner and executor call
  `hnsw` by name (`hnsw::serves`, `Access::Nearest`); a second method
  would copy them.  The index-method interface gains what a method
  serves (an ordering, a predicate) and how it is searched, written when
  the second method's shape is known (T4) — not before it; `hnsw` moves
  onto it unchanged.
- **T4 — the `text` index**: storage as in (1), `@@` in `WHERE` served
  by it, crash and concurrency tests (as `hnsw_crash.rs`).
- **T5 — BM25**: `score(body @@ q)` and top k by block-max MaxScore;
  rankings checked against Anserini on BEIR.
- **T5a — one ranking for every method**: HNSW's nearest rows and the
  text index's first rows by score through one `IndexMethod` hook, one
  access path and one optimizer pass, adaptive while it runs.
- **T5b — the found rows by a merge with the table's scan; block-max
  MaxScore and a parallel build**, measured on MS MARCO.
- **T5c — the ranking's scores read by `score()`**: the rows a method
  ranks come with their values, so the projection does not compute them
  again.
- **T6 — phrases, prefixes and fuzzy terms from positions; highlights
  and snippets.**
- **T7 — hybrid**: `rrf(...)` and a weighted combination in one plan
  with filters; `sparsevec`'s index on the same structure (impacts, dot
  product); BEIR against text-only and vector-only.
- **T8 — compatibility and reach**: PostgreSQL's `tsvector`, `tsquery`,
  `to_tsvector`, `ts_rank` over it in pg-compat; columnar and partitioned
  tables; the drivers where a type reaches them.

**T1 — analyzers** (done 2026-11-09).  `text::Analyzer` registered by
name (`Registry::add_analyzer`, `nc_catalog.text_analyzers`); built in:
`simple`, `porter` (Lucene's `EnglishAnalyzer`, for Anserini's
baselines), thirteen Snowball languages (Hungarian among them) and
`trigram`; `tokenize(text, analyzer)`.  Taken: UAX #29's rules over
tables generated from Unicode 16 (Lucene's `StandardTokenizer`
implements the same; the `unicode-segmentation` crate was not taken —
the core takes no dependency), checked by every case of Unicode's
`WordBreakTest.txt`; Snowball 3.0.1's stemmers as its compiler generates
them for Rust (BSD), each checked by its whole vocabulary (21–42
thousand words a language, at the data's commit of that release — the
data's later commits test Danish apostrophes 3.0.1 does not know);
Porter written here with the reference implementation's three
departures from the paper, which Lucene shares (`bli`, `logi`, words of
two letters) — and without them it gives Snowball's 42,603 words
exactly; Snowball's stop lists, Lucene's 33 English stop words for
`porter`; FTS5's trigram analyzer.  Lucene's possessive filter split a
word at a byte inside a multi-byte letter (`ñs`): found by the FiQA
corpus, fixed, and a property test now runs every analyzer over any
string (character boundaries, positions in order).

FiQA's 57,638 documents (44 MB) through SQL, `sum(cardinality(tokenize(body,
a)))`, best of three (`bench/text/analyzers.py`), against PostgreSQL 18's
`to_tsvector` (same machine, its container, `psql`; it counts distinct
terms a document):

| Analyzer | nucleus, 1 thread | nucleus, workers | PostgreSQL, 1 | PostgreSQL, parallel |
|---|---:|---:|---:|---:|
| `simple` | 1,058 ms | 177 ms | 3,176 | 1,138 |
| `porter` / `english` | 2,053 / 2,105 | 307 / 309 | — / 3,915 | — / 1,383 |
| `hungarian` | 2,059 | 310 | 4,319 | 1,521 |
| `trigram` | 2,796 | 449 | — | — |

Words are ~24 ns a byte, a stemmer doubles it.  Measured against
8d0a5fe (`bench/phases.py`, alternating, two rounds): TPC-H SF1 load
23.2 / 22.6 → 22.4 / 23.0 s, the queries' sums 5,874 / 5,927 → 5,839 /
5,840 ms: noise.

**T2 — the query language** (done 2026-11-09).  `text @@ query` on
rows (`text/query.rs`).  Taken: Lucene's query syntax, the subset one
column needs — words, `"phrases"`, `word*`, `word~N` (at most 2, a
transposition one edit, as `FuzzyQuery`), `+`/`-`, `AND`/`OR`/`NOT`,
parentheses — because people and models already write it; its boolean
clauses (`must`, `should`, `not`: with a required clause the others only
rank); its normalization of prefix and fuzzy words (lower case, no stem);
Tantivy's rule that a word the analyzer splits is a phrase of its parts
(`P/E`, `e-mail`, any word under `trigram`, which makes a trigram query
FTS5's substring match).  Words side by side are OR'ed (Lucene,
Tantivy, Elasticsearch; PostgreSQL's `websearch_to_tsquery` ANDs them):
ranked retrieval (T5) wants any of them, and `AND` or `+` asks for all.
Not taken: Lucene's error on a pure negation (`-spam` matches the texts
without it, as a predicate reads), field names, boosts, slop and ranges
(one column; T6 may add slop).  A malformed query is a `42601` error
with its character, not silently reinterpreted; punctuation outside the
syntax is a word break, so plain text is a query.  The operator's
function takes the analyzer as an optional third argument the binder
will give from the column's index (T4); a constant query is parsed once
a vector by the batch kernel.

FiQA (57,638 documents, 44 MB), `count(*) WHERE body @@ q` without an
index, best of three, against PostgreSQL 18's `to_tsvector('simple',
body) @@ q::tsquery` (same machine, its container; the counts differ a
little, its parser breaking words its own way):

| Query | nucleus, workers | nucleus, 1 | PostgreSQL, parallel | PostgreSQL, 1 |
|---|---:|---:|---:|---:|
| `market` | 122 ms | 779 | 1,139 | 3,252 |
| `stock market` (OR) | 121 | 786 | 1,138 | 3,258 |
| `stock AND market` | 128 | 791 | 1,140 | 3,249 |
| `"stock market"` | 127 | 784 | 1,145 | 3,239 |
| `invest*` | 119 | 778 | 1,139 | 3,231 |
| `markte~1` | 140 | 880 | — | — |

A query costs what analyzing the texts costs (a matched term is noted,
not kept: less than `tokenize`'s 1,055 ms); a fuzzy term first cost
2× — its edit distance allocated two vectors a term — and costs 1.13×
since ASCII is compared as bytes with its rows on the stack.  Measured
against 7451ebe (`bench/phases.py`, alternating, two rounds): TPC-H SF1
load 23.0 / 22.3 → 21.4 / 22.0 s, the queries' sums 6,148 / 5,733 →
5,846 / 5,678 ms: noise.

**T3 and T4 — index access for a filter, the text index** (done
2026-11-09).  T3 as the second method's shape showed it: a method may
give an operator an operand from the index on its column
(`IndexMethod::operand`, appended by the binder — the analyzer, so `@@`
means the same through the index and on the rows) and serve a filter's
condition exactly (`serves`, `rows_where`; `Access::Method`, chosen for
a full scan or a key range past 1,000 rows, the condition out of the
filter, every row read with it where the index cannot tell).  HNSW's
ordering stays `Access::Nearest` until text ranking orders rows (T5) —
the second ordering's shape decides that interface, as this one's did.

T4 as (1) said: deltas and tombstones under nonces (writers never
conflict), sealed at 1,024 rows by the session whose statement finds it
due, into segments of postings in 128-blocks (deltas and frequencies
bit-packed, positions apart, exact lengths), deleted objects per segment
(bits, counts, per-term counts), logarithmic merges by 8.  A row is
there as many times as its (key, text hash) was inserted less the times
it was deleted, so matches and statistics are counted per snapshot, not
read from how far the upkeep got.  Taken: FTS5's tombstones in the
newer data, Tantivy's delete bitsets written anew per change and its
logarithmic merges, Lucene's and Tantivy's 128-blocks, GIN's upkeep by
the writing sessions (as HNSW's).  Not yet: block maxima (T5 needs
them), SIMD unpacking, the economic order quantity for the seal size
(HNSW's meters; a fixed 1,024 first: a search reads a delta row in
about a microsecond, a seal costs milliseconds), a parallel build, and
reading many found rows by a merge with the table's scan rather than a
lookup each (below).  Found while testing: object ids `nonce | 1` made
two consecutive nonces one id — a seal's segment overwrote its deleted
object, and two databases' segments met in the process-wide cache; ids
are now nonces, never reused.  The crash test (any I/O point, dropped
and torn writes) and the snapshot test (a reader's answers and
statistics kept while others write, seal and merge) hold the index
exact.

FiQA (57,638 documents, 44 MB), `analyzer simple`, against PostgreSQL
18's GIN on `to_tsvector('simple', body)` (its server's execution time;
ours through the protocol), best of five:

| | nucleus `USING text` | PostgreSQL GIN |
|---|---:|---:|
| `CREATE INDEX` | 2,628 ms (one thread) | 3,145 ms (`psql` included) |
| `count(*)` `market` (7,337 rows) | 1.2 ms | 6.7 ms |
| `stock market` (either, 10,324) | 1.7 | 6.5 |
| `stock AND market` (2,379) | 0.5 | 4.1 |
| `"stock market"` (1,025) | 0.6 | 274.9 |
| `invest*` (9,784) | 1.9 | 7.5 |
| `markte~1` (7,337) | 4.7 | — |
| `sum(length(body))` of `market`'s rows | 16.2 | 12.1 |

A count reads no row — the index is exact for the snapshot, where GIN's
heap is visited for visibility; a phrase is checked on positions, where
GIN (no positions) re-reads every candidate's text.  Rows read cost us
more: a lookup by primary key each (≈2 µs), where PostgreSQL's bitmap
heap scan reads pages in order — the next step for the access path.
Measured against eaf8bf1 (`bench/phases.py`, alternating, two rounds):
TPC-H SF1 load 22.3 / 22.8 → 21.9 / 21.6 s, the queries' sums 5,874 /
5,762 → 5,837 / 5,877 ms: noise.

**T5 — BM25** (done 2026-11-09).  `score(body @@ q)` on a column with a
text index; `k1`, `b` as the index's options.  Taken: Lucene's
`BM25Similarity` (since 8.0: no `k1 + 1` factor, its never-negative IDF),
its phrase score (a frequency, the terms' IDFs summed), its constant
score for prefixes and fuzzy terms and its sums over a boolean query;
Lucene's, Elasticsearch's and Tantivy's 1.2 and 0.75 as the defaults.
Not taken: Lucene's one-byte lengths — exact ones cost nothing here
(Kamphuis et al., ECIR 2020: the variants barely differ).  The
statistics are the snapshot's (T4), read once a statement through an
uncorrelated subquery the index's binding adds (`IndexMethod::function`,
the third hook the shape needed); a statement ranking by the score has
the index give only its first `k` rows and the ties of the `k`-th
(`IndexMethod::ranks`, a `Rank` for `rows_where`, `MethodRank` on
`Access::Method`), the sort and the limit staying above — exact under
any later order key.  The index and a row score through one `Scorer`,
so `tests/text_index.rs` holds their first seven rows of eleven queries
equal to the last bit through writes, seals and merges.

HNSW's ordering did not move onto this hook in T5, against the plan's
T3 note: text ranked the rows its condition finds, nothing else
filtering them; HNSW ranks every row, checking the filter, policies and
partitions while its search walks (V5).  Reconsidered the next day
(T5a): the difference is what text lacks, not a second shape — a text
ranking dropped by any other filter reads every match and sorts them,
and the core still calls `hnsw` by name.  One mechanism a job.

Checked against Anserini's BM25 "flat" baselines (Lucene, title and text
one field, `porter` = its English analyzer, k1 0.9, b 0.4, queries as
bags of words, trec_eval's nDCG@10 over every judged query, ArguAna's
query document left out as BEIR and Anserini do), `bench/text/beir.py`:

| BEIR | nucleus | Anserini |
|---|---:|---:|
| SciFact | 0.6777 | 0.6789 |
| NFCorpus | 0.3206 | 0.3218 |
| ArguAna | 0.4029 | 0.3970 |
| FiQA | 0.2354 | 0.2361 |

Within 0.0012 but ArguAna, whose long documents feel Lucene's lossy
lengths most (not verified).  Against ParadeDB's `pg_search` 0.26.6
(Tantivy's BM25 in PostgreSQL 18, Docker; Snowball English, its stop
words, 1.2 / 0.75 — `english` and the defaults here; its time `\timing`
in `psql` in the container, ours the client's over TCP), nDCG@10 and the
mean time of a query's first 11:

| BEIR | nucleus | ms | ParadeDB | ms |
|---|---:|---:|---:|---:|
| SciFact | 0.6891 | 0.99 | 0.6839 | 0.91 |
| NFCorpus | 0.3291 | 0.80 | 0.3225 | 0.45 |
| ArguAna | 0.4964 | 3.11 | 0.4814 | 5.43 |
| FiQA | 0.2534 | 1.00 | 0.2500 | 1.62 |

Measured on the way (FiQA, 648 queries, mean): scoring each found row's
text — the first way — took 177 ms a SciFact query (the analyzer, 25
MB/s on one thread); scored from the postings, document at a time over
each leaf's cursor, 2.4 ms a FiQA query; a bag of words term at a time
into an array of the segment's documents (Lucene's and Tantivy's
`TermScorer` sums the same way in its exhaustive mode), 0.97 ms.
Counting a version by key and hash in a hash map for every match was
half of a `count(*)` (3.2 → 1.4 ms once only the versions a tombstone in
the delta names are counted), unions of many lists made pair by pair a
quarter of the rest (a bitmap now).  Not yet: block maxima and
block-max MaxScore (Ding and Suel, SIGIR 2011; Tantivy's block WAND) —
term at a time is linear in the postings, which these corpora keep short
(FiQA's longest query reads ~50,000); MS MARCO's 8.8 million passages
are where skipping blocks pays, measured there before it is built.
`CREATE INDEX` is 1.6–3.3× ParadeDB's (FiQA 3.1 against 0.95 s; theirs
in parallel): the parallel build above.  T4's FiQA counts after the
versions and unions above: `market` 1.2 → 0.6 ms, `stock market` 1.7 →
0.8, `stock AND market` 0.5 → 0.3, `invest*` 1.9 → 1.0, `markte~1` 4.7 →
4.0.  Measured against 898896c (`bench/phases.py`, alternating, two
rounds): TPC-H SF1 load 23.0 / 22.1 → 22.3 / 22.2 s, the queries' sums
5,922 / 5,703 → 5,945 / 5,737 ms: noise.

**T5a — one ranking for every method** (done 2026-11-10).  Two
mechanisms give an `ORDER BY … LIMIT k` its first rows from an index:
`Access::Nearest` (HNSW; the `nearest` pass, by cost; the core calling
`hnsw::serves`, `hnsw::rows`, `hnsw::nearest_rows`, `hnsw::among_most`
by name — principle 12 broken) and `Access::Method` with a `MethodRank`
(text; `method_ranks`, by rule, only where nothing else filters the
rows).  They become one:

1. **The hook.**  `IndexMethod::rank` — HNSW's `search` generalized: the
   rows first by the order (`Rank`), only those of the condition the
   index serves where it has one, among given rows where the plan has
   them, in the partition its other columns' values fix, each passing
   `keep` (the key filters, a row-level security policy, then the
   statement's filter); at least `k`, or `None` where the index cannot
   tell.  `IndexMethod::rank_cost`, the method's own cost (HNSW's `ef ·
   (3 + 2m / 10)`, moved into it).  The rows to search among come from
   the plan through a callback the method calls with the most it takes
   (HNSW's square-root rule, moved into it).
2. **One access path, one pass.**  `Access::Nearest` goes; `Access::Method`
   carries the condition served (optional — HNSW serves none), the rank,
   the rows within, the share passing, the checks and the partition.  One
   pass replaces `nearest` and `method_ranks`, by the method's cost,
   partitioned tables and row-level security for both.  `EXPLAIN` keeps
   its words.
3. **Adaptive while it runs, no switch** (as the join's `Adaptive`): the
   text index reads its scored matches greatest first in growing slices
   (`k` over the share expected, then by the share measured, as HNSW's
   iterative search grows `ef`), every row tied with the `k`-th passing
   one included, so the sort and the limit above stay exact; past the
   matches it has read them all — the old path, no worse.
4. **Combined.**  `within` may be another method's condition, not only a
   B-tree's range: `WHERE body @@ q ORDER BY emb <-> v LIMIT k` searches
   HNSW exactly among the text index's matches where they are few, the
   graph with `@@` as a check where they are many — T7's hybrid search
   built on it.

Kept apart: exact (text: the ties of the `k`-th) against approximate
(HNSW), HNSW's partitions as the method's own.

Built as planned.  `IndexMethod::ranks(ix, f, desc, nulls_first)`,
`rank_cost`, `rows` and `rank(txn, t, i, RankAsk, wanted, keep)` —
`keep` told whether a row came from `among`, so the condition that chose
those rows is not checked on them again; `Pks` moved to `index_method`;
`rows_where` lost its `rank`.  `Access::Method { index, served, rank }`
with `MethodRank { …, within, share, checks, partition }`;
`optimizer::ranked` replaces `nearest` and `method_ranks`;
`exec::method_rows` serves both, `exec::among` reads a key range's or
another method's rows.  The text index's `View::ranked` keeps the scored
matches and gives them greatest first in slices (a selection over the
rest, a sort of the slice).  Found on the way: under row-level security
a statement's `@@` stays above the barrier, so the text index never
served it (T3's gap, not the ranking's); it now chooses the scan's rows
as a key range's bound may (`Served::above`), staying above the barrier
and never evaluated below it where the index cannot tell the rows.  Left
in the core by name: the `hnsw.ef_search` setting's check (a module's
settings have no hook yet) and the text index's use of HNSW's `nonce`
and `retrying`.  `EXPLAIN` of a ranked text scan now shows `(rows=k)` as
HNSW's did, and ranks with a filter.

Measured.  BEIR, `english`, 1.2 / 0.75, a second condition `n % 100 <
p`, the mean ms of a query (second round), ranked against the old path —
every match read and scored on its text; the first round checks both
give the same rows (they did, all 2,677 queries):

| BEIR | 50 % | | 10 % | | 1 % | |
|---|---:|---:|---:|---:|---:|---:|
| | ranked | old | ranked | old | ranked | old |
| SciFact | 1.01 | 85.8 | 1.18 | 20.5 | 2.90 | 6.4 |
| NFCorpus | 0.76 | 17.8 | 0.83 | 4.8 | 1.21 | 1.5 |
| ArguAna | 3.09 | 233.9 | 4.31 | 74.6 | 5.30 | 18.0 |
| FiQA | 0.96 | 430.7 | 1.21 | 110.9 | 3.36 | 36.5 |

Without the condition as before (FiQA 0.97 ms, SciFact 0.98).  At 1 %
the rows read dominate — about a hundred for each that passes, a
primary-key lookup each: the merge with the table's scan (T4's note)
is what helps there, not MaxScore.  HNSW unchanged
(`bench/ann/filtered.py`, SIFT-1M, `ef` 40, against f5ea072, alternating,
two rounds): the same plans word for word, the same recall, µs within 1
% (50 % 559 / 545 → 551 / 538, 10 % 1,678 / 1,641 → 1,682 / 1,648, 1 %
1,377 / 1,411 → 1,392 / 1,416, 0.1 % 310 / 302 → 303 / 299, 0.01 % 460 /
460 → 458 / 460).  `bench/phases.py` against f5ea072 (alternating, two
rounds): TPC-H SF1 queries' sums, best of three, 5,597 / 5,483 → 5,442 /
5,439 ms: noise.

**T5b — the found rows by a merge with the table's scan** (done
2026-11-10).  A served condition's rows (`WHERE body @@ q`, and every
other method's) were read a primary-key lookup each, in the index's
order — a descent of the B+tree and a value decompressed a row, however
close the rows lay; T4's note and T5a's measurements (at 1 % the rows
read dominate) asked for a merge.  Taken: PostgreSQL's bitmap heap scan
(the found keys sorted, the table read in their order, a page read once
for all its rows) and its parallel form (the keys cut into runs, a
worker each), Lucene's `advance` (a cursor sought forward to the next
wanted document, or stepped through those between, whichever is
nearer).  Built:

1. **The KV reads by key first.**  The B+tree's `Range::next_key_into`
   and `value_into` (the value read and decompressed only when asked);
   the engine's `Scan::seek` forward and `next_chunk_where` — a chunk of
   only the keys a `want` predicate takes, the others passed by their
   keys, at most `most` of them; `KvChunks::seek` and `next_chunk_where`
   in the KV contract (a scan that cannot seek says so and reads on).
2. **`exec::entries_at`**: the keys sorted, each looked up where they
   lie far apart, the scan read on through the entries between where
   they lie close — by the share of the entries passed that are wanted,
   measured as it goes (a probe of 16 entries where it is not known and
   every 1,024th lookup, each chunk read on) and kept by the caller
   across its blocks: reading on pays above one in `SEEK_ENTRIES` (16,
   what a lookup costs in entries passed, measured).  The scan ends at
   the last key and is opened only where it reads.
3. **`TableStorage::rows_at`** (rows to a callback; a lookup each by
   default, the row storage's through `entries_at`) and
   **`exec::rows_at`** — past 256 keys a run each on the statement's
   workers over the transaction's shared reads.  `exec::method_rows`
   reads the found rows through it a block at a time — 8, twice the last
   up to 256, four times past it, up to 16,384 — so a limit above stops
   it having read at most twice its rows while they are few.

Not taken: the text index's ranking read the same way.  Its rows come
best first, a sample of the table's (the order by score is not the
keys'), until the `k`-th passes; read a block at a time by the merge,
the blocks overshot the `k`-th (an estimate of the share passing that
the optimizer guesses — `n % 100 < 1` as a third — corrected only as
rows pass), and where they were dense enough to read on (a few hundred
of SciFact's 5,183 rows) the merge still lost to stopping at the
`k`-th exactly: 1 % filters 5–9 % slower on every corpus, 10 % even.
It stays a lookup each.  Found on the way, each caught by a measurement
before it was kept: a scan that read on past the last key wanted (a
limit's first block passing thousands of entries); a probe that counted
the key it started at, so a sparse run looked dense; reading on bounded
by a chunk's entries rather than the keys left; a block's rows read
before the `k`-th's ties in the next slice were (`ties_at_every_limit`
now checks every limit, both orders among ties, with and without a
filter).

Measured.  FiQA (57,638 rows, the text index on `body`), a query's
best of 50 ms in two alternating rounds against bf946b4:

| query | rows found | before | merged |
|---|---:|---:|---:|
| `sum(n) … @@ 'market'` | 8,743 | 9.52 / 9.54 | 3.64 / 3.77 |
| `sum(length(body)) … @@ 'market'` | 8,743 | 17.13 / 17.20 | 9.04 / 9.25 |
| `sum(n) … @@ 'stock market'` | 12,404 | 13.31 / 13.28 | 4.32 / 4.20 |
| `sum(n) … @@ 'invest*'` | 9,784 | 10.77 / 10.76 | 3.93 / 3.79 |
| `sum(n) … @@ 'retirement'` | 2,166 | 2.53 / 2.55 | 2.05 / 1.99 |
| `sum(n) … @@ 'nvidia'` | 8 | 0.08 / 0.08 | 0.09 / 0.09 |
| `… @@ 'market' LIMIT 5` / `50` / `500` | | 0.34 / 0.45 / 1.45 | 0.36 / 0.46 / 1.37 |
| `sum(n)`, every row | | 2.69 / 2.74 | 2.73 / 2.71 |

BEIR with another condition (`bench/text/beir.py --filters 50,10,1
--unranked`, the mean ms of a query, second round; the same rows):
ranked as before (FiQA 0.97 / 1.21 / 3.37 against 0.97 / 1.21 / 3.48,
ArguAna 1 % 5.25 against 5.26), and the read-everything path — every
match read and scored on its text — faster where the rows read dominate:

| BEIR | 50 % | | 10 % | | 1 % | |
|---|---:|---:|---:|---:|---:|---:|
| | before | merged | before | merged | before | merged |
| SciFact | 84.7 | 82.2 | 20.2 | 18.0 | 6.25 | 4.34 |
| NFCorpus | 17.7 | 17.4 | 4.77 | 4.55 | 1.47 | 1.26 |
| ArguAna | 230.6 | 217.1 | 59.0 | 50.1 | 17.9 | 10.1 |
| FiQA | 439.6 | 417.3 | 111.2 | 92.4 | 36.4 | 18.3 |

Scans without keys to pass read as before (a plain scan's key and value
at once): `bench/phases.py` TPC-H SF1 against bf946b4, alternating, two
rounds, every query's best of three, sums 5,404 / 5,351 → 5,392 / 5,577
ms (the second round of both slower throughout); Q1, Q6, Q14 again,
best of seven: 327 / 327 → 328 / 328, 250 / 251 → 254 / 253, 231 / 231
→ 233 / 233 — within 1.2 %.

Next for found rows: the key-only pass is `mvcc::decode_key_onto`, a
byte at a time (about 38 ns an entry; SWAR or `memchr` would cut it),
and the rows are consumed a row at a time on the statement's thread
while the workers read — feeding the runs to the morsel pipeline
(§M14.20) would overlap them.

**T5b — block-max MaxScore and a parallel build** (done 2026-11-10).
T5 scored every match of a bag of words term at a time and read the
best; fine on BEIR's small corpora, not at MS MARCO's 8.8 million
passages, and the segment format was the time to change.  `CREATE INDEX`
analyzed on one thread (ParadeDB built FiQA 3.1× faster).  Taken:
Turtle and Flood's MaxScore (1995) as Lucene 10's `MaxScoreBulkScorer`
runs it — windows of documents, the leaves whose bounds together stay
below the threshold non-essential, the essential ones summed term at a
time into the window's buffer, the candidates then matched a leaf at a
time (Lucene's recent term-at-a-time rewrite of the window, PR #14701,
and Adrien Grand's note on vectorized disjunctions) — with Lucene's
per-block impacts (Ding and Suel's block-max bounds, from the Pareto
frontier of (frequency, length) pairs, not scores: IDF and the average
length are the snapshot's); PISA's and JASS's quantized scores for the
sums; Tantivy's and Lucene's indexing threads (a segment each) for the
build.  Built:

1. **Segments, version 2** (`segment.rs`): a term in more than one block
   has a skip list — per block its last document, its bytes, its
   impacts (`impacts`: the pairs no other beats).  `Segment::blocks`
   walks it undecoded, `Segment::block` decodes one block anywhere;
   version 1 is read, its skip lists made as it is decoded.
2. **Scores in fixed point** (`score::fixed`, units of 2⁻³²): a text's
   parts summed as integers, in the index and on rows, so any order of
   adding gives the same number — the contract that the index's scores
   are a row's to the last bit (T5) held while MaxScore adds in its own.
   A first version kept floating point and re-added a document's leaves
   in the query's order wherever a non-essential one held it (a counting
   sort of the window's postings): exact, and on FiQA twice a full
   scoring's cost at `n` 100.
3. **Windowed MaxScore** (`store.rs`, `max_score`, `Cursor`): the
   threshold the `n`-th best of the versions not named by the delta
   (`Top`); a window of 4,096 documents from the first one a leaf
   essential over the segment holds; per window the leaves' bounds there
   (blocks overlapping it, their impacts' best score plus a unit, so a
   floating-point score cannot pass its bound), the non-essential prefix,
   the essential postings summed, the candidates (live, and able to reach
   it), each non-essential leaf best bound first matched against them —
   read through where it holds four times the candidates or fewer, else
   each looked up (`Cursor::advance`, blocks skipped by their last
   documents) — and the candidates that can no longer reach it dropped.
   A version below the threshold is left out only where it cannot be
   among the `n` best: the ties of the `n`-th stay.  A term in one block
   (most of a small corpus's) is decoded into its cursor once, its bound
   the score of its greatest frequency in its shortest text; a window
   whose leaves are all essential hands its sums on as they are.  Every
   match wanted (`n` unbounded) is scored term at a time, as T5 did
   (`every_match`): the windows' bookkeeping with no threshold to skip by
   cost a long query a seventh more (ArguAna, measured).
4. **The ranking asks for `n`** (`View::ranked(q, sc, n)`,
   `Ranked::complete`): `k` without other conditions, four times the rows
   the expected share takes with them; every match scored where `n` is a
   64th of the matches or more (about: Σ df, at most the documents); the
   first `n` run out before `k` passed → a second pass of four times
   the rows the share seen so far needs (one more passing and read, so
   that none passing is not a share of nothing), by the same rule; a
   third scores every match.  On MS MARCO a second pass of every match
   was 13 ms of a 1 % filter's 21.
5. **The build on the statement's workers** (`TextIndex::build`): the
   rows' texts read, batches of 4,096 to 100,000 analyzed and made
   segments a worker each, the merges due run a round at a time
   (`Store::merge_due`: a round's groups at once, each group's terms in
   runs on its share of the workers, `segment::merge` cut at terms of
   the part with the most and joined in order — the same bytes as one
   run); `Doc::analyze` and `of_docs` sort and hash where they kept
   trees (a fifth of a build was a `BTreeMap`'s string comparisons).
   The batches' segments are then merged into one (`Store::merge_all`
   on the workers) where they hold 3 GB or less together, else by tiers
   (`merge_due`): an index just built is read before it is written
   (Lucene's `forceMerge(1)` for an index done with), a search looks
   each term up once a segment — ArguAna's 200-word queries in three
   segments 9 % slower than in one — and one merge reads each document
   once where tiers read it once a tier.
6. **`--index-cache <size>`** (the server; `object_cache`'s budget, a
   fixed 1 GB before): MS MARCO's index (1.09 GB of segments) past the
   budget was decoded again on every search — 2.2–3.5 s a query.  By
   default it is now a quarter of the memory the process may use
   (`nucleus_platform::real::physical_memory`: `hw.memsize` on macOS,
   `MemTotal` on Linux or a container's cgroup limit below it; 1 GB
   where the OS does not say) — PostgreSQL's advice for
   `shared_buffers`, DuckDB's `memory_limit` sized by the machine (80 %
   there, for all its memory; this is one cache, and a bound, not memory
   taken).

Not taken:
- **The threshold from the `k`-th row that passed `keep`**, as planned
  (scoring and the filter interleaved): every document that rises above
  it on the way must have its row read and checked — about `k / share ·
  ln(N · share / k)` reads where reading in score order takes `k /
  share` (FiQA at 1 %: four times the rows).  Instead `n` is sized by the
  expected share, and a wrong guess costs one full pass at most.
- **Growing `n` and scoring again**, as HNSW grows `ef`: the share
  optimizers guess (a third for `n % 100 < 1`) took up to seven passes;
  measured slower than one full pass on every BEIR corpus.
- **MaxScore for queries with phrases** (a word the analyzer splits is a
  phrase, many of ArguAna's queries): they are scored document at a
  time, every match, as before.
- **A first pass padded more, or full scoring sooner with a filter**
  (`n` 16× or 64× the expected rows; full scoring where `n` is a 256th
  or a 1,024th of the matches): FiQA's 1 % filter gains up to 6 % (the
  first pass the optimizer's guess — a third — sizes runs out, and
  every match is scored after it), its 10 % and 50 % lose 3–6 %; no
  setting wins on every corpus, so 4× and a 64th stay.

Found on the way: a bound of floating-point sums in another order than a
score's needed a margin (gone with fixed point); a window's frequency
matrix cleared per document cost ArguAna (long queries, 200 leaves) 2×;
probing non-essential leaves a document at a time lost to reading them
through when the threshold is low; `Segment::postings` decoding a block
at a time through `Segment::block` cost a full scoring 4 % more than one
loop over the blocks (restored); two-thirds of a short BEIR query is
`score()` analyzing the returned rows' texts again — the ranking could
hand its scores to the projection (T5c).

Measured against 6fa170f (`bench/text/beir.py`, the mean ms of a query,
the second of two rounds; base the second of two alternating runs).
MS MARCO passage — 8,841,823 passages, BEIR's dev split, its first
1,000 queries, `english`, 1.2 / 0.75, the server's `--index-cache 4GB`
(base's segments fit the 1 GB the cache had; the new, 1.09 GB, did not —
the default is now a quarter of the machine's memory, 8 GB here):

| MS MARCO | 6fa170f | T5b | ParadeDB |
|---|---:|---:|---:|
| nDCG@10 | 0.2605 | 0.2605 | 0.2578 |
| `CREATE INDEX` | 231.2 s | 42.0 s | 67.7 s |
| query, mean / p50 | 14.02 / 13.76 | 2.01 / 1.52 | 5.48 / 5.69 |
| with a 50 % filter | | 3.22 / 2.55 | |
| with a 10 % filter | 14.65 / 14.48 | 4.54 / 3.51 | |
| with a 1 % filter | 21.47 / 20.97 | 16.27 / 15.30 | |

ParadeDB's `pg_search` 0.26.6 (Docker on the same machine, timed by
`\timing` in `psql` inside the container, its English stemmer and stop
words, 1.2 / 0.75): our query 2.7× faster at the mean, 3.7× at the
median, the index built 1.6× faster.  A 1 % filter's time is now its rows: about 1,100 found rows read a
lookup each before 11 pass (8.8 million rows; the merge with the scan
was measured slower for ranked rows in T5b's first part).

BEIR, the same nDCG@10 to the last digit on every corpus and setting:

| BEIR | `CREATE INDEX` (`porter`) | | query (`porter`) | | query (`english`) | |
|---|---:|---:|---:|---:|---:|---:|
| | 6fa170f | T5b | 6fa170f | T5b | 6fa170f | T5b |
| SciFact | 480 ms | 375 ms | 0.97 | 0.96 | 0.96 | 0.96 |
| NFCorpus | 363 ms | 334 ms | 0.76 | 0.77 | 0.76 | 0.77 |
| ArguAna | 592 ms | 305 ms | 3.65 | 3.52 | 3.03 | 2.89 |
| FiQA | 3,086 ms | 683 ms | 1.55 | 1.28 | 0.98 | 0.86 |

With a filter (`english`, ranked; 50 / 10 / 1 %): SciFact 0.98 / 1.15 /
2.85 → 1.00 / 1.17 / 2.87, NFCorpus 0.75 / 0.80 / 1.18 → 0.75 / 0.81 /
1.18, ArguAna 3.08 / 3.32 / 5.29 → 2.95 / 3.17 / 5.14, FiQA 0.96 / 1.20 /
3.42 → 0.97 / 1.22 / 3.60 (the first pass lost, above).  ArguAna's
median rises (1.41 → 1.52 ms, `english`): its long bags of words, 1–2
ms, are 6–8 % slower — 200 leaves' cursors, bounds and windows on a
corpus of 8,674 documents, where little is skipped — while its queries
with phrases, scored document at a time over one segment, are 12 %
faster.  Unchanged elsewhere: `bench/phases.py reads` TPC-H SF1,
alternating, two rounds, sums of the best of three 5,537 / 5,500 →
5,516 / 5,503 ms.

**T5c — the ranking's scores read by `score()`** (done 2026-11-10).
The text index ranked by the scores it summed from its postings, then
dropped them: `score()` in the projection analyzed each returned row's
text again — two-thirds of a short BEIR query (T5b's profile:
`TextScore::call` → the analyzer).  Prior art: Lucene's collectors keep
each hit's score in its `ScoreDoc` and Elasticsearch's `_score` is that
number, never recomputed; ParadeDB's custom scan hands `pdb.score(id)`
the score Tantivy computed, as an output column of the scan; DuckDB's
`match_bm25` and PostgreSQL's `ts_rank` compute per row.  Taken: the
score as a column of the scan — here one any method may give, by one
hook.  Built:

1. **`IndexMethod::values(ix, function)`**: whether its rank gives each
   row's value of the function with it — the very number the function
   computes of the row.  The text index's BM25 is (T5b's fixed-point
   sums: a row's text and the postings give the same integer); by
   default none.  `RankAsk::values` asks for it; the text index pushes
   each row's score past its columns.
2. **`MethodRank::value`**: the ranked call, its value one more column
   past the table's (`Plan::width`, `Access::valued`).
   `optimizer::ranked` sets it where the method gives values and a
   projection under the sort computes the call, and rewrites the call,
   wherever it is in the projection's expressions, to that column.
   Where the index cannot rank after all (a `score` of another query
   than the condition's), `exec` computes the call on each row the scan
   reads, so the plan's shape is the same either way; `prune` reads the
   call's columns for it.  `EXPLAIN`: `first k by f(col) DESC with
   values`, the projection `#0, #2`.

Not taken:

- **A memo of scores by row for `text_score`** (the rank fills it, the
  function looks a row up): the function sees a text, not a key — it
  would have to be keyed by the text, or the rows carry a hidden key
  anyway.  The column is the same information, explicit in the plan.
- **HNSW's distances**: not checked to be the distance function's
  number to the bit (its search computes them its own way), and a
  vector's distance is cheap next to an analysis.  It keeps the
  default.
- **Partitioned tables** (`Append`): every partition's scan would have
  to give the value, ranked or not; left out until a partitioned text
  index is asked for.
- **Not reading the text at all where only its score is wanted**: the
  scan reads the column for where the index cannot rank; reading it
  only then needs `prune` to keep two sets of columns for one scan.
  Not measured apart: `k` texts read and decoded, not analyzed.

Measured against e61d55f (`bench/text/beir.py`, `english`, 1.2 / 0.75,
the mean ms of a query, the second of two rounds, base and new
alternating; the first round within 0.05 ms):

| BEIR | e61d55f | T5c | 50 % filter | 10 % filter | 1 % filter |
|---|---:|---:|---:|---:|---:|
| SciFact | 0.94 | 0.16 | 0.98 → 0.20 | 1.15 → 0.39 | 2.84 → 2.25 |
| NFCorpus | 0.75 | 0.11 | 0.74 → 0.14 | 0.79 → 0.24 | 1.27 → 0.93 |
| ArguAna | 2.85 | 2.13 | 2.90 → 2.15 | 3.16 → 2.40 | 5.14 → 4.37 |
| FiQA | 0.86 | 0.34 | 0.97 → 0.47 | 1.21 → 0.70 | 3.65 → 3.07 |

nDCG@10 the same to the last digit.  ArguAna's median 1.51 → 0.78 ms
(T5b's 6–8 % loss on its long queries is gone under it); `porter`, 0.9
/ 0.4: 0.17 / 0.12 / 2.65 / 0.46 ms.  Every corpus is now faster than
ParadeDB's (0.91 / 0.45 / 5.43 / 1.62 ms; SciFact and NFCorpus were
slower).  MS MARCO (1,000 dev queries, one run each): 2.05 / 1.45 →
1.81 / 1.22 ms mean / p50, a 50 % filter 3.11 → 2.99, 10 % 4.43 →
4.28; a 1 % filter 31.46 → 23.56 — its ~1,100 row lookups, which vary
from run to run (16.27 in T5b), not this step.

**`@@` estimated by the text index** (2026-11-10, found measuring T7).
On an analyzed table a text query took 46 ms on SciFact, not 0.2: the
planner estimated `body @@ q` on the sample `ANALYZE` keeps (§M14.12
V3: up to 1,000 rows, 1 MiB of them) by evaluating it — every kept
text analyzed, at every planning (`bench/text/beir.py` does not
analyze, so its numbers never showed it).  Prior art: PostgreSQL's
`tsmatchsel` estimates `@@` from the most common lexemes
`ts_typanalyze` keeps from a sample, a query's lexemes as independent
(`tsquery_opr_selec`: AND the product, OR s1 + s2 − s1·s2, NOT 1 − s),
one not among them at a floor; Lucene has no planner to estimate for —
its cost is the postings' lengths (`ScorerSupplier::cost`).  Taken:
PostgreSQL's combination, over the index's exact document counts (the
counts BM25 reads, T5) in place of a sample's most common:
`IndexMethod::selectivity`, asked by the statistics before the sample,
the text index's by `query::share`; a prefix or a fuzzy word (a sum
over the dictionary's terms) is left to the sample.  SciFact, analyzed:
46.3 → 0.27 ms a query (0.20 not analyzed; e61d55f analyzed 47.3).
Not taken: remembering the sample's answers across plannings — each
new query text would pay again.

**T6 — phrases within a slop; highlights and snippets** (done
2026-11-10).  Prior art: Lucene's `"a b"~N` (`SloppyPhraseMatcher`:
each term's position less its offset in the phrase, the least moved on,
an occurrence where the span is at most the slop, weighing 1 / (1 +
span) in the frequency BM25 sees — `sloppyWeight`); PostgreSQL's
`<N>` is an exact distance, not a bound (T8).  Highlights: PostgreSQL's
`ts_headline` (`StartSel`/`StopSel` `<b>`/`</b>`, `MaxWords` 35, the
best fragment by the query's cover); SQLite FTS5's `highlight()` and
`snippet(…, ellipsis, ntokens)` (a phrase's whole occurrence one mark;
the window with the most distinct phrases, then the most hits, the
earliest of equals); Elasticsearch's unified highlighter (passages
scored by BM25); ParadeDB's `pdb.snippet` (Tantivy's
`SnippetGenerator`: fragments up to so many characters, scored by their
terms' idf).  Taken:

1. **Slop**: `"…"~N` (0–1,000) in the query syntax; one function,
   `query::phrase`, gives a phrase's occurrences and weights wherever
   positions are read — a row's text, a segment's postings, BM25's
   frequency, a highlight — exact by binary search, within a slop by
   Lucene's matcher.  The index and the rows agree on every row and
   score (`tests/text_index.rs` runs two slopped phrases among its
   queries; `60_text_highlight.slt` checks scores worked by hand).
2. **`highlight(col @@ q [, start, stop])` and `snippet(col @@ q [,
   start, stop, ellipsis, words])`**, functions of the condition as
   `score` is: a new `ScalarFunction` pair, `of_condition()` (the
   operator it takes) and `bind_condition()` (bound from the operator's
   typed operands), so the text is analyzed by the column's index's
   analyzer (`simple` without one) and the query means what `@@` means.
   FTS5's marks and window order, PostgreSQL's defaults (`<b>`, `</b>`,
   35 words), `...` at either side where the text goes on, the window
   moved to centre its marks.

Not taken: scoring passages by BM25 (Elasticsearch's) — the window with
the most distinct words found answers "why this row" as well on short
texts, and needs no statistics; several fragments a text (`ts_headline`'s
`MaxFragments`) and character limits (Tantivy's) — later if asked;
escaping HTML (neither `ts_headline` nor FTS5 does: the caller knows the
markup); Lucene's rule for a term twice in one phrase (its repeats may
share a position here).

Measured (`bench/text/beir.py --snippets`, `english`, 1.2 / 0.75, the
mean ms of a query, the second of two rounds; against 63e0d41
alternating, the CPU otherwise idle): the query alone unchanged —
0.17 / 0.12 / 2.19 / 0.35 → 0.18 / 0.12 / 2.26 / 0.42 ms (the first
pair 0.18 / 0.14 / 2.45 / 0.39 → 0.24 / 0.15 / 2.46 / 0.36: noise), the
same nDCG@10; with each of the 11 rows' snippet — its text analyzed
again, about 0.08 ms a row — against ParadeDB's `pdb.snippet(body)`
(Tantivy's 150 characters, ours 35 words):

| BEIR | query | with snippets | ParadeDB with snippets |
|---|---:|---:|---:|
| SciFact | 0.18 | 1.13 | 1.49 |
| NFCorpus | 0.12 | 0.91 | 0.93 |
| ArguAna | 2.26 | 3.27 | 7.26 |
| FiQA | 0.42 | 0.97 | 2.23 |

Not measured apart: a snippet from the positions the index holds (the
text is analyzed again; the index has the positions but not the
offsets — Lucene's postings-with-offsets highlighter) — later, if
snippets dominate a query.

**T7a — hybrid: `rrf` and `convex` in one plan** (done 2026-11-10).
Prior art: reciprocal rank fusion (Cormack, Clarke and Büttcher, SIGIR
2009: Σ 1 / (60 + rank), k = 60 everyone's default); a convex
combination of normalized scores (Bruch, Gai and Ingber, "An Analysis
of Fusion Functions for Hybrid Retrieval", TOIS 2024 — it beats RRF
when tuned, and min-max normalization is as good as any); every engine
fuses each ranking's first `rank_window_size` rows (Elasticsearch's
`rrf` and `linear` retrievers, OpenSearch's hybrid query with its
`min_max` normalization, Weaviate's `rankedFusion` and
`relativeScoreFusion`, Vespa's `reciprocal_rank_fusion` over its
first-phase hits), a row missing from a list 0 in it; ParadeDB and
pgvector users write RRF in SQL — a CTE a ranking, `RANK()`, a `FULL
OUTER JOIN` (ParadeDB's documentation).  Taken: the same meaning, as
two functions the optimizer plans — no CTEs to write, one table's
filter and row-level security on every list:

1. **`rrf(r1, r2, …)`** (2–8 rankings) and **`convex(r1, r2, α)`**,
   only as the first key of `ORDER BY … DESC LIMIT k` over one table
   (elsewhere `0A000`).  A ranking is greatest first, a distance nearest
   first; `ScalarFunction::found` names the rows a function ranks
   (`score(body @@ q)`'s are `@@`'s), so the text's list holds matches
   only.
2. **`hybrid::fused`**, an optimizer pass: each ranking's list a
   `Limit` (100, or `k` + offset) over a `Sort` of the scan — which
   T5a's `ranked` serves from the index on its column, the text index's
   with its scores (T5c), HNSW's nearest — a window over it (`rank()`,
   or `min`/`max`), the lists `UNION ALL`, one row a candidate by an
   aggregate over the primary key and the columns read above, fused.

The first version took every ranking of every candidate again and
ranked them among the candidates (the scores of rows outside a list are
known then): 15.6 ms a SciFact query on a busy machine, 13 of them
`text_score` analyzing the 188 candidates' texts again.  Each list now
carries its own values, as the engines above do: 3.2 ms on the same
busy machine, 1.49 idle.  Not taken: weights per ranking
in `rrf` (Elasticsearch has none either; `convex` is the weighted one),
more than two rankings in `convex`, `rank_constant` other than 60 — each
an argument away when asked.

Measured (`bench/text/hybrid.py`: BEIR's four corpora with
BAAI/bge-small-en-v1.5's vectors by `bench/text/embed.py`, 384
dimensions, cosine, an HNSW index with pgvector's defaults; the text
index `english`, 1.2 / 0.75; α 0.5; the mean ms of a query, the second
of two rounds, the CPU otherwise idle) against ParadeDB 0.26.6 with
pgvector 0.8.6 — the same vectors, HNSW's defaults, `hnsw.ef_search`
100 for its RRF (else its HNSW gives 40 rows at most), RRF as its
documentation writes it:

| nDCG@10 | text | vector | `rrf` | `convex` | ParadeDB text | vector | RRF |
|---|---:|---:|---:|---:|---:|---:|---:|
| SciFact | 0.6891 | 0.7126 | 0.7329 | 0.7445 | 0.6839 | 0.7161 | 0.7327 |
| NFCorpus | 0.3291 | 0.3163 | 0.3602 | 0.3633 | 0.3225 | 0.3339 | 0.3636 |
| ArguAna | 0.4964 | 0.5973 | 0.5981 | 0.6169 | 0.4814 | 0.5950 | 0.5890 |
| FiQA | 0.2534 | 0.3669 | 0.3570 | 0.3826 | 0.2500 | 0.3730 | 0.3598 |

| ms a query | text | vector | `rrf` | `convex` | ParadeDB text | vector | RRF |
|---|---:|---:|---:|---:|---:|---:|---:|
| SciFact | 0.27 | 0.38 | 1.49 | 1.72 | 0.88 | 0.42 | 7.41 |
| NFCorpus | 0.16 | 0.31 | 1.29 | 1.47 | 0.43 | 0.38 | 4.99 |
| ArguAna | 2.33 | 0.37 | 3.67 | 3.96 | 5.98 | 0.36 | 6.74 |
| FiQA | 0.39 | 0.48 | 2.12 | 2.31 | 1.62 | 0.70 | 3.86 |

Fusion beats either ranking alone on every corpus but FiQA's RRF (its
BM25 is far weaker than its vectors; `convex` still gains), as Bruch et
al. found; `rrf` is 1.8–5.0× faster than ParadeDB's, about the sum of
its two lists' times.  Found on the way: our HNSW finds less than
pgvector's at the same `m`, `ef_construction` and `ef_search` 40 —
NFCorpus 0.3163 against 0.3339 (exact 0.3375; ours at `ef_search` 100,
0.3355), FiQA 0.3669 against 0.3730, SciFact 0.7126 against 0.7161
(exact 0.7203) — §M14.12's to look into, not this step's.

**T7b — the sparse vector index** (done 2026-11-10).  Prior art:
learned sparse retrieval (SPLADE: Formal et al. 2021, SPLADE++ 2022) is
an inverted index's — Elasticsearch's `sparse_vector` and OpenSearch's
neural sparse search store a token's weight as a Lucene term frequency
(`FeatureField`, its value in a float's high bits) and rank by block-max
WAND over impacts; Vespa's `wand`, Milvus's `SPARSE_INVERTED_INDEX` /
`SPARSE_WAND`, Seismic (Bruch et al., SIGIR 2024) prune further,
approximately; pgvector indexes `sparsevec` with HNSW (at most 1,000
non-zero, approximate).  Taken: Elasticsearch's idea on T4–T5b's
storage, kept exact:

1. **`USING sparse (sv)`** on a `sparsevec` column: the text index's
   store with *weighted* documents — a dimension a term (eight hex
   digits), a weight the term's frequency as the `f32`'s bits in the
   numbers' order (`segment::weight_bits`: exact, not quantized, so a
   block's impacts give its greatest weight), no positions.  The delta
   marks a weighted term by 0 positions (a text's term has one at
   least: old deltas read as they did); merges copy weights.
2. **`Scorer::dot`**: each leaf's query weight times a posting's, in
   T5b's units; the same block-max MaxScore ranks it.
3. **Exact `<#>`**: the index's sum and the function's `f64` differ by
   at most 1,025 units a leaf (products under 2^16), so the index gives
   every row within that of the `k`-th passing and the sort above keeps
   the first `k` by `<#>` itself.  Where a row the index does not find
   could be among them — a query weight not positive, fewer than `k`
   rows sharing a dimension (the rest tie at 0), a product past 2^16 —
   the rows are read.

Not taken: quantized weights (Lucene's `FeatureField` keeps 9 bits of
mantissa; a bound from them would need a wider margin and more rows
read — the postings are 4 bytes a weight here);
approximate pruning (Seismic's, or WAND with a raised threshold) —
exactness first, an option later; `inner_product(sv, q) DESC` (the
same ranking, another name) and cosine (`<=>`, which needs norms).

Measured (`bench/text/hybrid.py --sparse`: SPLADE++
(`prithivida/Splade_PP_en_v1`, fastembed) for documents and queries in a
`sparsevec(30522)` column beside T7a's; `ORDER BY sv <#> $1, id LIMIT
11`, and `rrf` of BM25, the dense vectors and the sparse ones; ParadeDB
0.26.6 with pgvector 0.8.6's HNSW on `sparsevec_ip_ops`, `ef_search`
40; nDCG@10, the mean ms of a query):

| BEIR | SPLADE, nucleus | ms | pgvector | ms | `rrf` of three | ms | index built |
|---|---:|---:|---:|---:|---:|---:|---:|
| SciFact | 0.7093 | 1.12 | 0.7025 | 0.80 | 0.7373 | 2.97 | 141 ms |
| NFCorpus | 0.3426 | 0.37 | 0.3208 | 0.66 | 0.3587 | 2.04 | 100 ms |
| ArguAna | 0.4742 | 3.96 | 0.4726 | 1.36 | 0.5844 | 8.46 | 246 ms |

Exact ranks above pgvector's approximate graph on every corpus
(NFCorpus by 0.022: its graph misses relevant rows); faster on
NFCorpus's short queries, slower on SciFact's and on ArguAna's — whole
arguments as queries, each a few hundred dimensions, every one a leaf
of the MaxScore, where the graph's cost does not grow with the query.
The fusion of three ranks above `rrf` of two on SciFact only (0.7373
against 0.7329; NFCorpus 0.3587 against 0.3602, ArguAna 0.5844 against
0.5981): SPLADE adds little beside BM25 and dense vectors.  Not taken yet, from this:
the approximate pruning above for long queries (Seismic's per-query
cut of the smallest weights).

**T8a — PostgreSQL's text search in pg-compat** (done 2026-11-11).
Prior art: PostgreSQL's `tsvector` and `tsquery` (`tsvector_op.c`'s
`TS_execute`: positions of a phrase's parts merged per operator —
`TS_phrase_output` — a `NOT` under a phrase as the positions it does not
match, `&` under a phrase the positions both match, weights filtering
positions, a term without positions a "maybe"; `tsquery_cleanup.c`'s
`clean_stopword_intree`: a stop word dropped, the phrase distance across
it kept, `cat <-> the <-> mat` is `cat <2> mat`; `tsquery.c`'s parser and
printer); the GIN index on `to_tsvector(cfg, col)` that serves
`to_tsvector(cfg, col) @@ query` by matching the expression.  ParadeDB
does not serve `@@` with its BM25 index (its own `|||`, `@@@`).  Taken,
in the module (principle 11: nucleus's own `@@` keeps its Lucene
syntax):

1. **The types** `tsvector` and `tsquery`, PostgreSQL's text forms both
   ways (quoting, positions and weights, the printer's parentheses —
   `!( 'a' & 'b' )`, a right phrase in them), `regconfig` a name for
   `text`; **the makers** `to_tsvector`, `to_tsquery`, `plainto_`,
   `phraseto_`, `websearch_to_tsquery` (`[cfg,] text`), the
   configurations nucleus's analyzers by name (`english`, `hungarian`,
   … — `pg_catalog.` dropped; `simple` without one); **`@@`** between a
   `tsvector` and a `tsquery` either way round, and `text @@ tsquery`.
   Evaluated as `TS_execute`, its phrase execution too.
2. **The rewrite**: `ScalarFunction::rewrite(args)`, a new hook — the
   call that gives the same value on every row, bound in this one's
   place.  `@@` gives `to_tsvector(cfg, e) @@ maker(cfg, 'literal')`
   as nucleus's `e @@ q` by the analyzer `cfg` — what a text index on
   `e` analyzing so serves — where `q` says the query exactly: each
   operand a quoted phrase of its words (the analyzer finds the same
   terms in it as in the word), a prefix as its stem and `*`, `&`, `|`,
   `!` as `AND`, `OR`, `NOT`, a phrase of words a step apart as one
   phrase (only if the analyzer gives the words joined the positions
   PostgreSQL's chain has); not weights, other distances, phrases over
   operators, the empty query, nor a parameter (left to the values).
   PostgreSQL 13 on, `!x` matches the empty vector, as nucleus's `NOT x`
   matches a text without terms: no condition on that.

Departures, documented: nucleus's analyzers (UAX #29 words, Snowball's
stemmers and stop lists — PostgreSQL's parser splits some tokens
otherwise); positions not capped at 16,383 nor 256 a term (the caps
lose phrase matches; the text index keeps every position, and the
rewrite must find the same rows); values sent as text (oid 25), not
PostgreSQL's binary `tsvector`.  Tested: 2,400 cases PostgreSQL 18
answered (`bench/text/tsearch_pg.py`: random vectors against random
queries — phrases at distances 0–3 over operators, negations,
prefixes, weights, terms without positions — and the makers' queries,
their text forms) all alike; 400 random queries of the four makers by
the rewrite and over stored `tsvector`s alike, without the index and
through it (306 rewritten); the plans.

Measured (`bench/text/tsearch.py`: each BEIR query's words, a count of
the rows `plainto_tsquery` — all — and `websearch_to_tsquery` of them
joined by `or` — any — match; nucleus through PostgreSQL's protocol with
psycopg, a query's time at the client; PostgreSQL 18 with `CREATE INDEX
… USING gin (to_tsvector('english', body))`, the server's time; the
mean ms of a query, median in parentheses, the second of two rounds):

| | all, nucleus | all, GIN | any, nucleus | any, GIN | index built |
|---|---:|---:|---:|---:|---:|
| SciFact | 0.16 | 0.17 | 0.42 (0.42) | 47.75 (0.89) | 398 ms, GIN 906 |
| NFCorpus | 0.12 | 0.14 | 0.19 (0.17) | 0.26 (0.20) | 367 ms, GIN 691 |
| FiQA | 0.25 | 0.32 | 1.56 (1.49) | 9.07 (5.76) | 682 ms, GIN 1,877 |

The rows matched agree within the analyses' difference (FiQA any:
11.47 million against 10.60).  Without the rewrite — over a stored
`tsvector` column, the values computed per row — SciFact takes 17.7 ms
a query, NFCorpus 13.6: the rewrite is what makes the GIN pattern an
index's.  Not taken yet: a binary canonical form for the values (each
row's text form is parsed again: the 17.7 ms); a parameter's query
(`@@ to_tsquery('english', $1)`, what drivers send) rewritten —
it needs the translation at execution, where it may fail; `CREATE INDEX
… USING gin (to_tsvector(cfg, col))` as a text index; `ts_rank`,
`ts_rank_cd`, `ts_headline` (T8b).

**T8b — PostgreSQL's ranking and headlines in pg-compat** (done
2026-11-11).  Prior art: PostgreSQL's `tsrank.c` (`ts_rank`:
`calc_rank_and` — each pair of the query's terms by its nearest
positions, `word_distance`'s inverse, the weights' table —,
`calc_rank_or`, the normalizations by length, unique words, log length,
the rank plus one; `ts_rank_cd`: `Cover`'s covers, each the inverse of
its width weighted, `get_docrep`'s positions of the query's terms),
`SortAndUniqItems` over `sort_template.h`'s `qsort`; `wparser_def.c`'s
`prsd_headline` (`hlCover`: the covers of the query by the AND'ed
locations `TS_execute_locations` gives, each checked by `TS_execute` on
the words inside; `mark_hl_words`: the best cover stretched to
`MinWords`–`MaxWords` words, not ending on a short word or a number;
`mark_hl_fragments`: covers cut to `MaxWords`, the ones with the most
query words, then the fewest words, each grown both ways by half the
rest; `get_next_fragment`), `ts_parse.c`'s `hlparsetext`, `hlfinditem`
(a word matching two operands repeated, once each) and
`generateHeadline`; the planner's `make_sort_input_target` (an expensive
output expression computed above the sort and the limit).  Taken:

1. **`ts_rank`, `ts_rank_cd`** (`[weights,] vector, query
   [, normalization]`) as PostgreSQL computes them, to the bit:
   `ts_rank` in `float4` with C's promotions (a `sqrt`, `1.0 - x` in
   `double`), `ts_rank_cd` in `double`.  Which of two operands of the
   same word (`a`, `a:*`) counts depends on PostgreSQL's `qsort`, which
   is not stable past six items: ported (insertion sort below 7, the
   presorted check, median of three or nine, Bentley–McIlroy's
   partition) — with a stable sort, 7 of 1,500 cases differed.
2. **`ts_headline([cfg,] document, query [, options])`**: the options as
   `deserialize_deflist` reads them (`key = value`, quoted values,
   PostgreSQL's errors) and `prsd_headline` checks them (`MaxWords` 35,
   `MinWords` 15, `ShortWord` 3, `MaxFragments` 0, `HighlightAll`,
   `StartSel`, `StopSel`, `FragmentDelimiter`); the text a list of words
   — `simple`'s, the text between two words one token, a number never a
   headline's end — with the configuration's terms at their positions;
   the selection and output ported as they are.
3. **The output after the limit** (the optimizer's `postponed`): under
   `LIMIT`, a projection's expressions the sort does not order by are
   computed above the limit, the sort carrying the columns they read —
   `ts_headline` on the ten rows kept, not on every match.  PostgreSQL
   postpones only expensive or volatile ones (more than ten operators'
   cost); nucleus every one that is not a column or a constant: the sort
   keeps `k` rows, so carrying a column costs no more than computing on
   it, and an error a row not kept would raise is not raised.

Departures: the words are UAX #29's (PostgreSQL's parser splits `don't`
and hyphenated words, keeps `3.5` and URLs whole), HTML tags are text
(PostgreSQL's parser replaces a tag with a space).  Tested: PostgreSQL
18's answers (`tests/tsearch_pg.tsv`, 5,100 cases) — 1,500 random
vectors and queries ranked six ways each (`ts_rank`, `ts_rank_cd`,
normalized by a random mask, weighted by random weights; 1,273 with a
`ts_rank` above 0, 441 a `ts_rank_cd`) alike to the bit, and 1,200
headlines of random English (words, numbers, commas, stops) for random
queries over prefixes, phrases and every operator, with 11 sets of
options, alike to the byte; the options' errors in
`slt/03_text_search.slt`; `tests/sort.rs` for the postponement (the plan,
and `1000 / x` not computed on a row with `x` 0 the limit drops).

Measured (`bench/text/tsearch.py --rank`: the ten best matches by
`ts_rank(to_tsvector('english', body), q)`, and with each one's
`ts_headline`; as T8a, a query's mean ms, median in parentheses):

| | any, nucleus | any, GIN | with headlines, nucleus | GIN | nDCG@10, nucleus | GIN |
|---|---:|---:|---:|---:|---:|---:|
| SciFact | 262.0 (270.2) | 376.4 (351.4) | 263.3 (273.5) | 377.7 (347.2) | 0.5720 | 0.5693 |
| NFCorpus | 53.1 (22.7) | 70.6 (30.3) | 53.9 (23.6) | 72.0 (32.4) | 0.3039 | 0.3004 |

(all the words: 0.17 ms against 0.19 on SciFact, 1.17 against 1.42 on
NFCorpus, nDCG@10 0.0749 and 0.2075 against 0.0749 and 0.2045.)  Before
the postponement the headlines doubled nucleus's time (517 ms SciFact,
103 NFCorpus): each match's was computed.  A profile of the SciFact
query: 206 of its 254 ms is `to_tsvector` — every match's text analyzed
again, as in PostgreSQL —, `ts_rank` the rest; nucleus's own BM25
(`score(body @@ q)`, T5c) ranks the same queries in 0.16 ms at nDCG@10
0.6891.  Not taken yet: `ts_rank` over a text index's positions (the
`to_tsvector` would not be computed: the index has each row's terms
and positions, but a row's are spread over its postings — a
forward index, or `score()` by the rewrite, PostgreSQL's ranking
dropped); the other `tsvector` functions (`length`, `setweight`,
`strip`, `ts_rewrite`, `ts_stat`, `ts_debug`); FiQA ranked (its any
queries match 16,000 rows each: minutes a round).

**T8c — every kind of table; the types on the wire** (done 2026-11-11).
Prior art: PostgreSQL's partitioned indexes (`CREATE INDEX` on the
parent made on each partition; an ordered `LIMIT` over a `Merge Append`
of each partition's index scan); Elasticsearch's shards, which score by
their own statistics unless asked for `dfs_query_then_fetch` — the
term statistics gathered from every shard first, so a document scores
as in one index; PostgreSQL's `tsvectorsend` / `tsvectorrecv` and
`tsquerysend` / `tsqueryrecv` (`tsvector.c`, `tsquery.c`: a term's
bytes and a zero, `WordEntryPos`'s weight in the top two bits; the
items in prefix order, right operand first, `findoprnd`'s checks) and
rust-postgres's `TS_VECTOR` / `TSQUERY`.  Found: a text and a sparse
index on a `column` or `row,column` table already worked (an index is
told every row written, whatever the storage), and on a partitioned
table each partition's index served `@@` and ranked its first `k` —
but BM25's statistics were read from the partitioned table's own index,
which holds no rows (scores 0.22 for 0.56), and `rrf` / `convex` over a
partitioned table were left to the rows (the fusion's rewrite took one
table's scan).  Taken:

1. **BM25's statistics a partitioned table's**: documents, length and
   each term's documents summed over its partitions' indexes
   (`text::index::stats`; each partition's index of the same definition
   — method, columns, operator classes, options —, looked for past the
   table's place when the partition has indexes of its own before it):
   Elasticsearch's DFS always, at no cost — the statistics are each
   index's, read in the statement's snapshot, no round of its own.  A
   row scores as it would in one table, whichever partition holds it.
2. **The scores read from each partition's index**: the ranking of a
   partitioned table asks each partition's index for its values (`with
   values`, T5c) and reads them where every partition's gives them (the
   rows of an `Append` are of one width), else none does.  Before,
   each partition's first `k` rows' scores were computed again on their
   texts: SciFact 3.37 ms a query, NFCorpus 2.46, FiQA 4.19; after, 0.43,
   0.32 and 0.81.
3. **`rrf` and `convex` over a partitioned table**: `hybrid::fused`
   takes an `Append` of scans as it takes one scan — each list the
   partitions' first rows by their indexes, the condition of a `found`
   ranking on each.
4. **`tsvector` and `tsquery` as PostgreSQL's types on its wire**: oids
   3614 and 3615 (and `pg_type`'s rows, their arrays', `format_type`'s
   names — `\d` showed `text`), binary format both ways as
   PostgreSQL's send and receive functions, checked as they check
   (`22P03`), and `tsvectorsend()` / `tsquerysend()` in SQL.  A query's
   tree is read from its prefix order backwards with a stack — no
   recursion, a depth bound (10,000) because the server walks the tree.
   Found by the fixture: `<N>` takes 16,384 (`MAXENTRYPOS`), not 16,383,
   and PostgreSQL's clean-up adds the distance across stop words
   unchecked (`cat <16384> the <-> dog` is `<16385>`): both as it.
5. **nucleus's own protocol**: its sessions do not have the module, and
   a value is sent as any registered type's (tag 11: its name and its
   binary form, the text form for these two): the drivers give it as a
   type they do not know (`Value::Other`, `Ext`), and a text parameter
   is taken where a column holds one.  Typing `tsvector` in nucleus's
   drivers was not taken: a module's type, PostgreSQL's (principle 11),
   which nucleus's own text search (`body @@ q` on `text`) does not
   need.

Departure, documented: a `tsquery` is what its text form reads back as.
PostgreSQL prints `a & (b & c)` as `'a' & 'b' & 'c'`, which reads back
as `(a & b) & c`, a value PostgreSQL holds unequal to the first; a
nucleus type's text form must read back as its value (§M14.19's laws), so
nucleus's `tsquery` of either is the second — the same matches, ranks
and headlines (`&` and `|` are associative; ranking sorts the operands),
its binary form that of `'a' & 'b' & 'c'`.  Tested:
`tests/text_tables.rs` (a `row`, a `column`, a `row,column` table and
one partitioned into partitions of each storage, the same writes —
loads, inserts, updates, deletes, rows moved between partitions, a seal
— and at each step eight queries' rows, first seven by score under three
filters, highlights and snippets, the first by `<#>`, `rrf` and `convex`
alike to the bit, each served by the indexes; without the statistics'
sum the partitioned table's scores differ from the first query on);
pg-compat's rewrite test on a `column` and a partitioned table; 305 of
PostgreSQL 18's binary forms (`tsearch_pg.tsv`: random values and the
edges — positions past 16,383 and 256, quoted terms, the longest
distance) alike to the byte; rust-postgres sending and reading them
binary; the Rust driver over `ncp`.

Measured (`bench/text/beir.py --layout`, `porter`, k1 0.9, b 0.4, the
mean ms of a query through the protocol; the nDCG@10 the same in each):

| | `row` | `column` | partitioned (hash, 4) | `CREATE INDEX`, `row` / partitioned |
|---|---:|---:|---:|---:|
| SciFact | 0.21 | 0.19 | 0.43 | 406 / 449 ms |
| NFCorpus | 0.13 | 0.12 | 0.32 | 336 / 334 ms |
| FiQA | 0.49 | 0.50 | 0.81 | 770 / 1,178 ms |

A partitioned table's query is four searches, each with its own fixed
cost, and the statistics read by each partition's scan (eight reads of
four indexes' statistics a query, an uncorrelated subquery each).  Not
taken yet: one read of the statistics a statement; a `Merge Append` of
the partitions' first rows (the sort above has `P · k` of them, few);
the partitions' indexes built at once (FiQA's four one after another,
each on every core: 1.18 s against 0.77).

### §M14.22 — Embeddings in the database ✅

The step every AI application repeats — text to vectors, kept in sync —
done by the database:

- **`embed(text [, model])`** returning a `vector`.  Embedding providers
  are modules: local models (ONNX runtime, run by nucleus itself) and
  HTTP providers, configured in the catalog, with their credentials kept
  as secrets (never shown in `nc_catalog` or logs).
- **Embedding columns kept current**: `v vector(1024) GENERATED BY
  EMBEDDING (title || ' ' || body) USING model` — when the source
  changes, the vector is recomputed in the background.  A network call
  never runs inside a transaction: the write commits, a durable job
  queue (on the KV, transactional with the write) computes the vector,
  retries with backoff, and marks rows whose vector is pending, so a
  query can wait for, skip or include them.
- **Model versions**: a column records which model and version made each
  vector; changing the model re-embeds online (old vectors served until
  the new ones are ready), like an encoding upgrade (§M14.19).
- **Chunking**: table functions that split documents into overlapping
  chunks by tokens, sentences or structure (Markdown headings), so a
  document table feeds a chunk table with embeddings in one statement.
- Batching and rate limits per provider; the cost and latency of calls
  visible in the analyzer (§M14.9).

**Done when:** a document table with an embedding column stays in sync
through inserts, updates, deletes, provider outages and crashes; a model
change re-embeds online; and a local model embeds without any network.

**The design and its steps** (recorded 2026-11-12).  What others do, read
from their docs:

- *The pipeline beside the database.*  Timescale's pgai Vectorizer
  (`ai.create_vectorizer(source, chunking, formatting, embedding,
  destination)`): triggers write a queue table, a worker *outside*
  PostgreSQL (Python) polls it, chunks, calls the provider in batches and
  writes a destination table or column; schema changes, drops and worker
  restarts must be coordinated with that process.  pgEdge Vectorizer does
  the same with background workers inside PostgreSQL: a poll interval
  (1 s), at most 3 attempts with exponential backoff, HTTP 429 deferred
  rather than counted as a failure, errors that cannot succeed moved to a
  failed state at once, `queue_status` and `failed_items` views.
  Supabase's automatic embeddings: a trigger puts a job on `pgmq` (a
  transactional queue), `pg_cron` drains it every few seconds in batches
  through `pg_net` to an Edge Function that calls the provider and writes
  the vector back; failed jobs reappear when their visibility timeout
  passes.  In all three the vector is stale or absent between the commit
  and the worker's write, and nothing in a query says which rows are.
- *The model inside the database.*  Oracle 23ai imports an ONNX model
  into the database (`DBMS_VECTOR.LOAD_ONNX_MODEL`, a BLOB of at most
  2 GB, the tokenizer and pooling folded into the ONNX graph) and computes
  `VECTOR_EMBEDDING(model USING text AS data)` in the statement, with
  `UTL_TO_CHUNKS` and `UTL_TO_EMBEDDINGS` for chunks; the model travels
  with the database and its backups.  PostgresML runs Hugging Face models
  in a Python interpreter inside the server (`pgml.embed`).
- *The search engines.*  Elasticsearch's `semantic_text` field: the
  inference endpoint (local or a provider's) is named in the mapping, the
  field chunks and embeds at ingest, chunking `sentence` by default
  (`max_chunk_size` 250 words, `sentence_overlap` 0 or 1; `word` and
  `none` the others); changed chunking applies only after a reindex.
  Weaviate's vectorizer modules embed at import, synchronously, in
  batches.  LanceDB's embedding functions are named in the schema and run
  in the client.
- *Inference.*  fastembed runs ONNX Runtime (MLAS kernels) with quantized
  BERT-family models; Hugging Face's text-embeddings-inference batches
  by a *token budget*, not a row count, and packs sequences without
  padding (variable-length attention); candle and llama.cpp (ggml) run
  BERT encoders from safetensors / GGUF.  For small encoders the time is
  in matrix products (BLIS's micro-kernel method, Goto & van de Geijn
  2008) and, past a few hundred tokens, in attention.
- *Chunking.*  LangChain's recursive splitter (paragraphs, then lines,
  then words; 1,000 characters, 200 overlap by default), LlamaIndex's
  `SentenceSplitter` (1,024 tokens, 200 overlap), Unstructured's
  `by_title`, pgai's recursive splitter; sentences by UAX #29.

What nucleus takes, and how it differs:

1. **Models are catalog objects; providers are modules** (principle
   12).  `CREATE MODEL m USING provider (options)` — `local` (weights in
   the database), `openai` (any OpenAI-compatible endpoint: OpenAI,
   Mistral, vLLM, Ollama, LM Studio, text-embeddings-inference),
   `cohere`, `voyage`; a provider is a `Registry::add_model_provider`
   implementation, its options checked when the model is created.  A
   model knows its dimensions, its maximum input, its batch and rate
   limits, and how a *query* differs from a *document* (e5's `query:` /
   `passage:` prefixes, bge's instruction, Cohere's and Voyage's
   `input_type`) — a quality loss nobody sees when it is left to each
   caller.  `embed(text, m)` embeds a document, `embed(text, m, 'query')`
   a query; `v <=> embed($q)` with no model takes `v`'s model and embeds
   a query.
2. **Secrets** are catalog objects (`CREATE SECRET k VALUE '…'` or
   `FROM ENV 'NAME'`), used by name in a model's options, `USAGE` granted
   like any privilege (§M14.25); their values never reach `nc_catalog`,
   statement statistics, the audit log or an error, and are kept
   encrypted (ChaCha20-Poly1305, RFC 8439, in `nucleus-crypto` without
   dependencies) under a key outside the data directory, so a copied
   directory or a backup does not reveal them.
3. **Local models run in nucleus, on its own kernels** (decided
   2026-11-12 over ONNX Runtime and candle: no C++ library in the
   module, the same code in WASM and on phones (§M14.23), and the
   arithmetic is nucleus's to make fast and reproducible).  The BERT
   family first — BERT and XLM-RoBERTa encoders, WordPiece and
   SentencePiece-Unigram tokenizers, mean / CLS pooling — which covers
   bge, e5 (multilingual-e5 reads Hungarian), MiniLM and GTE; weights
   read from safetensors and `tokenizer.json` and stored in the database
   as large values, so a model is backed up and replicated with the data
   and `embed()` never needs a network.  Packed batches by token budget,
   no padding.  One build gives the same vector on any number of
   threads (a fixed reduction order), and a vector is computed once and
   then stored as data, never recomputed by a replica.
4. **Embedding columns are exact about what they are not.**  `v
   vector(384) GENERATED BY EMBEDDING (title || ' ' || body) USING m`:
   a write that changes the source enqueues the row under a queue key in
   the same transaction (one entry a row, so five updates while pending
   are one job, the newest text wins); workers take entries in batches,
   read the row, call the provider, and write the vector in a
   transaction that checks the source's fingerprint is still the one
   embedded — a row changed meanwhile keeps its newer entry.  Retries
   with exponential backoff and full jitter, a provider's `Retry-After`
   obeyed, a circuit opened per provider while it is down, errors that
   cannot succeed (a bad key, an input too long) recorded at once;
   `nc_catalog.embedding_jobs` shows the queue.  Each row knows whether
   its vector is current, so a query chooses: `embedding.pending =
   include` (vectors as they are, the default), `skip` (rows with a
   pending vector left out), `wait` (the statement waits for the jobs
   committed before its snapshot).  The workers are the engine's, not
   the server's (principle 7: the embedded library keeps its columns
   current too), and run on the platform's clock, so the simulator
   drives crashes and outages (principle 4).  Beside it, standard
   `GENERATED ALWAYS AS (expr) STORED` columns — where a local model is
   fast enough to embed inside the statement, the vector is never
   pending.
5. **Model versions as an encoding upgrade** (§M14.19).  Each vector
   records the model and version that made it; `ALTER … SET MODEL m2`
   fills a shadow column in the background (its index built as it
   fills), queries read the old one until the new one is complete, and
   one transaction swaps them — dimensions may change.
6. **Chunks are table functions, and a chunk table can be kept.**
   `chunks(text, by => 'sentences' | 'tokens' | 'markdown', size,
   overlap [, model])` returns position, offsets, heading path and text;
   sizes in the model's own tokens, so a chunk fits its input.  Sentences
   by UAX #29 (tables generated from Unicode 16, checked by its
   `SentenceBreakTest.txt`, as words were in §M14.21 T1); Markdown by
   CommonMark's headings, then paragraphs, then sentences.  A chunk
   table declared from a document table is re-chunked through the same
   queue when a document changes and emptied with it (the lineage
   §M14.24's `FORGET` follows); a chunk whose text is unchanged keeps its
   vector (a content-addressed cache by model and text hash), so an edit
   to one paragraph embeds one chunk, not the document.
7. **Costs visible.**  Batches sized by each provider's limits (inputs
   and tokens a request), requests and tokens a minute by token bucket,
   calls in parallel to a set limit; calls, tokens, cache hits, latency
   and errors in `EXPLAIN ANALYZE` and the statement statistics (§M14.9).

The steps:

- **E1 — this study.**
- **E2 — models, secrets, `embed()` over HTTP**: the provider registry,
  `CREATE MODEL` / `CREATE SECRET`, the OpenAI-compatible, Cohere and
  Voyage providers on one HTTP client (the oidc module's, moved to
  where both reach it), a scripted provider for tests; query and
  document embeddings.
- **E3 — the local model**: the tokenizers (checked against Hugging
  Face's `tokenizers` on every text of a corpus), the encoder on
  nucleus's kernels, weights in the database; checked against the
  publishers' own f32 ONNX exports run by ONNX Runtime (cosine ≥ 0.9999
  a vector, the same nDCG@10 on BEIR) and measured against fastembed on
  one core and on all.
- **E4 — columns kept current**: `GENERATED ALWAYS AS … STORED`, then
  `GENERATED BY EMBEDDING`, the queue, the workers, retries, pending
  rows; crash, outage and concurrency tests in the simulator.
- **E5 — model versions**: re-embedding online.
- **E6 — chunking functions**: sentences, tokens, Markdown.
- **E7 — chunk tables kept current**, lineage, the content-addressed
  cache.
- **E8 — batching, rate limits, costs in the analyzer**; the *Done
  when* checked whole on BEIR, documents to ranked chunks.

E2 is done in two: **E2a** the secrets, **E2b** the models, the
providers and `embed()`.

**E2a — secrets** (done 2026-11-12).  `CREATE [IF NOT EXISTS] | ALTER |
DROP SECRET`, `VALUE '…'` sealed or `FROM ENV 'VAR'` (administrators
only: the process's environment is theirs), `GRANT USAGE ON SECRET`,
`nc_catalog.secrets` (never a value), catalog `00 14`.  Taken: RFC
8439's ChaCha20-Poly1305, written in `nucleus-crypto` from the RFC
(poly1305-donna's 26-bit limbs, the tag compared in constant time) and
checked by its four vectors, which Python's `cryptography` reproduces;
a random nonce a value (96 bits: no reuse in any realistic number of
secrets), the secret's name as associated data (a record moved to
another name does not open), the key's id beside it (another key is
refused by name).  The key lives outside the data directory — a
`Keyring` set for the process (principle 12: a KMS or Vault later is
another one), the server's `--secrets-key` file by default
`<dir>.secrets-key`, made with mode 0600 when the first secret is —
as PostgreSQL's TDE proposals and Supabase Vault (pgsodium's root key
outside the database) keep theirs.  Not taken: Vault's
decrypted view (a value is never readable through SQL — only a
provider's call uses it).  Found on the way: the audit log kept a
`CREATE | ALTER ROLE … PASSWORD '…'` as written; a statement naming a
secret or a password is now recorded, and logged by `--log-queries`,
with its strings as `'***'` (`statements::redacted`).  Tests:
`tests/secrets.rs` (nothing in the clear anywhere in the KV, the audit
masked, privileges, owners through role renames and drops, `FROM ENV`,
rotation, a damaged record, another key, no key).

**E2b — models and `embed()`** (done 2026-11-12).  `CREATE | DROP MODEL m
USING provider WITH (…)` (catalog `00 15`), `GRANT USAGE ON MODEL`,
`embed(text, m [, 'query' | 'document'])`, `nc_catalog.models` (with
the process's calls, texts, cache hits, errors and time) and
`model_providers`.  Providers are registered (`ModelProvider`: `define`
checks the options and gives dimensions and batch without a network,
`embed` takes a batch of one kind); the core keeps `secret` (`USAGE` on
it needed to name it — a model's URL is its maker's choice, so a secret
they may not use must not travel there), `query_prefix` and
`document_prefix` (e5, bge).  Built in, `hashing`: Weinberger et al.'s
feature hashing *without* its random signs — the signs keep inner
products unbiased, but two words of a short text cancelled into a zero
vector, which has no cosine (found by the first run of the CLI).
Modules: `nucleus-http` (the oidc module's client, now shared and
general) and `nucleus-embeddings` — `openai` (the de facto protocol:
OpenAI, Ollama, vLLM, LM Studio, text-embeddings-inference), `cohere`
(v2, `input_type`), `voyage` (`input_type`); their batches by each API's
limit; one retry after `Retry-After`; `58030` for what may succeed later
(E4's queue will retry those), `55000` for what will not.  Taken from
LangChain's `CacheBackedEmbeddings` and E2's design item 6: a cache by
SHA-256 of (the model's definition, the kind, the text), bounded, which
makes `embed()` stable — a ranking's query vector is computed once
(checked: one provider call for `ORDER BY v <-> embed(…)` over 40 rows,
none the second time) — and the vectorized executor's `call_batch` sends
a batch's texts together (2,000 rows of a column table: 24 calls at a
batch of 100 — 20 full, the rest the ends of the executor's batches).  Not yet: connection reuse (one TLS handshake
a call — batches carry it; E8), the one-argument `embed($q)` taking a
column's model (E4, when columns have one).

E3 is done in three: **E3a** the tokenizers — WordPiece behind
BERT's normalizer (bge, MiniLM, GTE) and SentencePiece's Unigram behind
its precompiled normalizer (XLM-R: multilingual-e5), read from
`tokenizer.json`; **E3b** the encoder — an f32 matrix product in
`nucleus-kernels`, BERT's layers, CLS and mean pooling, weights from
safetensors; **E3c** the weights in the database, `CREATE MODEL …
USING local`, and the measurements.

**E3a — the tokenizers** (done 2026-11-12).  `nucleus-local-models`
reads `tokenizer.json`: WordPiece behind BERT's normalizer and
pre-tokenizer, Unigram behind SentencePiece's precompiled map, runs of
spaces squeezed and Metaspace; special tokens in a text matched as
`tokenizers` matches them; anything else refused by name.  Found by
comparing code point by code point: `tokenizers` classifies for BERT
by Unicode **8** (the `unicode_categories` crate's tables) — with
Unicode 16, 560 code points differ (punctuation and nonspacing marks
added since, U+166D moved from Po to So), with 8 none — so
`scripts/gen-tokenizer-tables.py` generates BERT's classes and
decompositions from 8 and the grapheme clusters (UAX #29, its 1,093
test cases passing) from 16.  Taken from SentencePiece / `tokenizers`:
the precompiled map is a darts-clone double array read as is, applied
by grapheme cluster with the *shortest* matching key (what `tokenizers`
does, not SentencePiece's longest); Viterbi over bytes with unknowns at
the lowest score less ten, fused.  Checked by `bench/embed/tokens.py`
against `tokenizers` 0.23.3 on bge-small-en-v1.5 and
multilingual-e5-small: 103,821 texts of BEIR and twelve languages' word
lists, 2,224,128 code-point probes and 200,000 random mixtures (letters,
marks, emoji, controls, special tokens) — all the same ids (the first
run found one difference: a text that normalizes to a leading space
gets no second `▁`).  Speed on one core, those corpora (73 MB): 1.9 s
WordPiece; Unigram 27.7 s by binary search in the sorted pieces, 7.6 s
over a byte trie (`tokenizers`, one thread from Python: 17.9 s and
19.6 s).

**E3b — the encoder** (done 2026-11-12).  `nucleus-kernels` gained
the forward pass's kernels, reached by modules as `ext::kernels`: an f32
matrix product after GotoBLAS / BLIS (Goto and van de Geijn 2008 —
the right side packed once into panels of `NR` columns, the left a block
at a time into panels of `MR` rows, an `MR × NR` register tile by fused
multiply-adds: 8 × 12 on NEON, BLIS's ARMv8 choice; 6 × 16 on x86),
written as plain loops LLVM vectorizes (checked in the disassembly: 24
by-element `fmla` a step, 24 of the 32 registers accumulating); GELU by
Eigen's / XLA's rational `erf` (within 5·10⁻⁷), softmax by Cephes'
`expf` (within 10⁻⁷ relative), layer normalization.  Every output of
the product is one chain of fused multiply-adds in the order of the
shared dimension — a tile is loaded and continued, never summed in
parts — so the tests hold it to the naive chain *bit for bit* at every
CPU level, and a vector does not depend on the blocking, the SIMD width
or the threads.  The encoder (`modules/local-models`: BERT and
XLM-RoBERTa from `config.json` and safetensors in F32, F16 or BF16; CLS
or mean pooling from sentence-transformers' `1_Pooling`) packs a batch's
texts end to end without padding — linear layers over all tokens,
attention over each text's own — and gives threads whole texts, so a
text's vector is the same bits alone, in any batch and on 1, 4, 8 or 12
threads (checked).  Block sizes measured on an M2 Pro core: KC 512 / MC
48 gave 82–93 GFLOP/s of the core's 112 (KC 256: 78–86, 128: 68–76).
Against the publishers' f32 ONNX exports in ONNX Runtime 1.31
(`bench/embed/encoder.py`, BEIR and twelve languages): cosine ≥
0.9999998 a text on bge-small-en-v1.5 and multilingual-e5-small, the
first run.  One thread: 1,555 tokens/s (ONNX Runtime, one text a call:
about 1,740) — 84 % of the time in the linear layers at ~90 GFLOP/s,
16 % in attention's narrow products (k = 32, about 55 GFLOP/s), left for
later; 8 threads 8,970 tokens/s, 12 (6 + 4 cores) 9,990.

**E3c — local models in the database** (done 2026-11-12).  The `local`
provider (`modules/local-models`): `CREATE MODEL bge USING local WITH
(huggingface = 'BAAI/bge-small-en-v1.5')` fetches `tokenizer.json`,
`config.json`, `model.safetensors` and `1_Pooling/config.json` once
(redirects followed; a file from the hub's large-file store checked
against the SHA-256 the hub announces), or reads them from a directory
(`path`, administrators only, as `endpoint`); the core keeps them in the
database by content — `00 16 <SHA-256> <chunk>`, 32 KiB chunks, two
models from the same files one copy, dropped with the last model — so a
backup or a replica carries the model, and no network is needed after
`CREATE MODEL` (Ollama, fastembed and pgai's local paths keep a cache
on the host's disk instead, outside the database's backups).  A process
loads a model once, by its files' identity (four kept, least recently
used dropped).  Measured (`bench/embed/speed.py`, 2,000 texts of
`encoder.py`'s BEIR and word-list sample, an M2 Pro; fastembed 0.9.1 on
ONNX Runtime 1.31.1, batches of 32, both on the publisher's own f32 ONNX
export and on its default, Qdrant's optimized one for bge):

| model | threads | nucleus | fastembed (publisher's f32) | fastembed (Qdrant's) |
|---|---:|---:|---:|---:|
| bge-small-en-v1.5 | 1 | 1,600 tokens/s | 497 | 502 |
| bge-small-en-v1.5 | 10 | 9,929 | 2,286 | 2,265 |
| multilingual-e5-small | 1 | 1,557 | 488 | — |
| multilingual-e5-small | 10 | 7,714 | 1,772 | — |

3.2× on one core, 4.3× (bge) and 4.4× (e5) on ten.  The likely reason
(not yet measured apart): fastembed pads a batch to its longest text and
runs attention over the padding, while nucleus packs texts end to end
(E3b); one text a call through ONNX Runtime — no padding — ran at 1,740
tokens/s in E3b, about nucleus's speed, so the kernels are even and the
layout wins.  e5 gains less than bge on ten threads (4.9× its one-thread
speed against 6.2×); threads take whole texts, and why e5's spread less
evenly is left to look into.  Not yet measured here: nDCG@10 on BEIR
through the local model (the vectors are within cosine 0.9999998 of
ONNX Runtime's, so E8's end-to-end check covers it).

E4 is done in two: **E4a** standard `GENERATED ALWAYS AS (expr) STORED`
columns, **E4b** embedding columns — the queue, the rounds, retries,
pending rows.

**E4a — stored generated columns** (done 2026-11-12).  Computed by the
table's writer on every write of the row (`Writer::complete`: `INSERT`,
`COPY`, `UPDATE`, foreign keys' cascades, `ALTER COLUMN … TYPE` of a
column read), before the row's constraints, so they are checked, keyed
and indexed like any column; never given (`428C9`).  As PostgreSQL: the
expression may not read a generated column nor call a volatile function
(`42P17`), nor hold a subquery; not with `DEFAULT`; not in the
partitioning; `ADD COLUMN` computes it for each row.  Unlike PostgreSQL,
an `INSERT` or `COPY` without a column list takes values for the columns
that are not generated (PostgreSQL wants `DEFAULT` in their places, and
`COPY` already leaves them out) — a row is written as its data, not as
its layout.  Not taken: `VIRTUAL` (PostgreSQL 18's default) — a stored
value is what an index or a vector search reads, and the embedding
columns need one.  Table record version 15; pg-compat shows them as
PostgreSQL's catalogs do (`attgenerated`, `pg_attrdef`,
`information_schema.columns.generation_expression`).  Tests:
`slt/61_generated_columns.slt`.

**E4b — embedding columns** (done 2026-11-12).  `v vector GENERATED BY
EMBEDDING (expr) USING m` — design item 4 as written, with what building
it decided:

- *A job is a key, not a log.*  `00 17 <table> <column> <pk>`, put by the
  writer's transaction when a column the expression reads changed
  (compared as values: an update of another column costs nothing), moved
  with the row's key, deleted with the row.  The worker needs no
  fingerprint: it reads the job and the row again when it writes, and
  writes the vector only if the job is still the one it took and the
  row's text still the one it embedded — otherwise the newer job is the
  next round's (checked: a writer changing the text while the call is
  out, `a_writer_racing_a_round_wins`).  Snapshot isolation's write-write
  conflict on the row and the job does the rest.  As pgai's, the table
  needs a primary key: a query tells a row's job by it.
- *Failures* by their code: `58030` (and conflicts, timeouts) back off
  exponentially with full jitter (Brooker's "Exponential Backoff and
  Jitter": a draw from zero to min(5 min, 1 s × 2ⁿ), which spreads the
  retries of many jobs best) or obey the provider's `Retry-After`
  (carried in the error's hint by `models::retry_hint`); others fail the
  job at once (pgEdge's failed state) — a batch refused as a whole is
  split, so one bad text fails alone — and are tried again in an hour or
  when the row's text changes.  Five failures in a row open the model's
  circuit (Nygard's circuit breaker, *Release It!*): 30 s, doubling to
  5 min.
- *Who works.*  The engine's sessions, a round after their statements
  when a job may be due (a flag a put sets; the earliest retry's time) —
  so the embedded library keeps its columns current with no thread, as
  HNSW's upkeep is paid — unless a background worker serves the
  database: the `nucleus` server starts one (`embedding::serve`), asleep
  until a job is put or comes due.  One round at a time a database.
- *Pending rows*: `embedding.pending = include | skip | wait` as
  designed; `skip` is a condition the binder puts over the table's scans
  (a job's key looked up a row); `wait` runs rounds before the query's
  snapshot is taken, on the database's clock (`Kv::sleep`, new: the
  simulation passes the time), within the statement's time (`57014`
  past it), and is refused in a transaction whose snapshot exists
  (`25001`); jobs failed for good do not hold it up.
- `embed($q)` without a model beside such a column (`v <=> embed($q)`)
  embeds a query by the column's model — design item 1, left from E2b.
- `nc_catalog.embedding_jobs` (state, attempts, next attempt, error).
  `ALTER COLUMN … TYPE` on such a table makes every vector again (the
  models' cache answers unchanged texts).

Tests (`tests/embedding_columns.rs`): kept current through inserts,
updates of the text and of other columns, key changes, deletes, `ADD
COLUMN`, an HNSW index over the column; the refusals; an outage — the
backoff, the circuit opening after five failures and closing, a
`Retry-After` obeyed to the second, `skip`, `include`, `wait` past its
time and after the provider is back; a refused text in a batch; a
writer racing a round; and a crash at any I/O point of a workload of
writes and rounds (83 runs for the default seed in each of two crash
modes — unsynced writes dropped, the last write torn — every one crashed
and recovered) — after each, a state some step left, and every vector
its row's text's once the jobs left are done.

**E5 — a column moved to another model** (done 2026-11-12).  `ALTER
TABLE t ALTER COLUMN v SET MODEL m2` — design item 5, with what building
it decided:

- *Beside, then at once.*  The column records its model, a version and
  the next model (table record: `2 <expr> <model> <version u32>
  <next>`).  Setting one puts a job a row in the column's slot marked
  `0x8000`; the rounds make `m2`'s vectors into a store beside the table,
  `00 18 <table> <column> <pk>` → the components (none: a NULL text),
  while queries keep reading `m`'s — a search never mixes the two
  models' spaces.  Writes during the move put both jobs (and a key
  change or a delete moves or removes the vector beside).  When a round
  finds no next-model job left, one transaction takes them: the rows
  rewritten with the new vectors, the column's type the new model's
  dimensions (a move may change them), its HNSW index defined and built
  again, its model `m2` and version one more, the store and the old
  model's jobs gone.  A crash or a conflicting writer leaves the move
  where it was; the next round tries again.
- *The column's own work first.*  A round fills its 256 texts with the
  columns' jobs, then the next models' in the room left; `wait` waits
  for the columns' jobs only.
- *Given up* by setting the column's own model again; another model
  starts over.  A model being moved to is not dropped (`2BP01`).
- `nc_catalog.columns` shows `model_version` and `next_model`,
  `nc_catalog.embedding_jobs` a job's `kind` (`current`, `next`).

Deviation from the design: the new HNSW index is built at the swap, in
its transaction, not as the vectors come in — an index over the store
beside would need a second index per column for the move's length;
building once over the finished vectors is what `ALTER COLUMN … TYPE`
does and keeps the swap one commit.  Taken from: online schema change
tools' shadow-and-swap (gh-ost, pg_repack: build beside, cut over in one
step), applied to one column.  Tests (`tests/embedding_columns.rs`,
`a_column_moves_to_another_model`): a move from 2 to 3 dimensions with
the provider down and back, writes, a key change and a delete during it,
nearest-neighbour search on both sides of the swap, the refusals, a move
given up; the crash workload gained `SET MODEL` steps both ways (every
seed of 8 had one to seven).

**E6 — chunks** (done 2026-11-12).  `chunks(text, by, size [, overlap
[, model]])`, a table function — design item 6, with what building it
decided:

- *Units, packed greedily, never broken without need.*  `sentences`
  packs whole UAX #29 sentences, `tokens` word segments, `markdown` a
  section's blocks (heading line, paragraphs, a fenced code block whole);
  only a unit over `size` is cut finer (block → sentences → segments),
  and its pieces are packed only with each other — a chunk is whole
  units or the pieces of one, never the end of one sentence glued to the
  start of the next (LangChain's recursive splitter merges across).
  Sections by CommonMark headings (ATX and setext, not in fences) are
  never crossed, and each chunk carries its heading path (Unstructured's
  `by_title`, LlamaIndex's `MarkdownNodeParser`).
- *Sizes in the model's tokens*, as text-embeddings-inference and
  LlamaIndex count them: `ModelProvider::count_tokens` (new; the `local`
  provider counts by its own tokenizer, no prefix or special tokens;
  remote providers say they cannot — `0A000`, their tokenizer is the
  server's).  The pieces' counts are summed while packing and the chunk
  is counted again whole, given fewer units while over — so `size` is
  the model's own count of the chunk.  Without a model, words.
- *Overlap* in the same units, from the end of the chunk before, only
  where the next chunk then has room for a unit it did not have (a
  chunk that is all overlap says nothing new).
- Offsets in characters, as `substr` takes them; `headings text[]`.
- Sentences by UAX #29's default rules (SB1–SB11), tables generated from
  Unicode 16 by `scripts/gen-sentence-breaks.py`, all 512 cases of
  `SentenceBreakTest.txt` passing — the default rules break after
  "Mr." (Lucene's and ICU's tailorings with abbreviation lists are not
  taken: a list is a language's, and a sentence cut short only makes a
  chunk start earlier).

Deviation: positional arguments, not `by => …` — nucleus has no named
notation yet; a later step can add PostgreSQL's `name => value` for all
functions.  Found on the way: table functions could return one column;
the binder now takes several, named by the function.  Speed (one M2 Pro
core, BEIR FiQA's 57,600 texts, 44 MB, size 200 words, overlap 20):
sentences 1.0 s (45 MB/s), markdown 1.0 s, tokens 1.7 s (26 MB/s).
Tests: `chunks.rs` (packing, overlap, Markdown sections, ATX headings as
CommonMark reads them), `text::sentences` (Unicode's cases),
`slt/62_chunks.slt` (with `LATERAL` over a table, offsets against
`substr`, the refusals), local-models `tests/provider.rs` (sizes in a
model's WordPiece tokens).

**E7 — chunk tables** (done 2026-11-12).  `CREATE TABLE parts CHUNKS OF
docs (body) BY markdown SIZE 200 [OVERLAP 20] [USING e5]`, then `ALTER
TABLE parts ADD COLUMN v vector GENERATED BY EMBEDDING (chunk) USING
e5` for the vectors — design item 6's second half, with what building
it decided:

- *Lineage as a foreign key.*  The chunk table's key is the document's
  and `position`, and its foreign key to the documents is `ON DELETE
  CASCADE ON UPDATE CASCADE`: a deleted document takes its chunks, a
  changed key moves them, through the machinery foreign keys already
  have and test; `DROP TABLE docs` needs `CASCADE`, which takes the chunk
  tables (as PostgreSQL's views go).  §M14.24's `FORGET` will follow the
  same edges.
- *In the writer's transaction, not the queue* (a deviation from the
  design): chunking is local and fast — no provider is called; a model's
  token count is its own tokenizer's — so a document and its chunks are
  never out of step, and the queue keeps what needs the network: the
  embedding columns over the chunks (E4), which `wait` / `skip` cover.
- *The cache is the rows.*  Instead of a separate content-addressed
  store, a document chunked again is matched with its stored chunks by
  text: an unchanged chunk keeps its row — and its vector, with no job —
  and only its position and offsets move (through negative positions,
  so the key never collides); the rest are deleted or inserted.  An edit
  to one paragraph embeds the chunks it changed (tested: one job, the
  others' vectors kept; a paragraph put first moves four chunks and
  embeds one).  Texts the same across documents are the models' cache's
  (E2b).
- Rows are the documents': `INSERT`, `DELETE`, `COPY` and `UPDATE` of a
  derived column are refused; added columns are the user's.

Measured (one M2 Pro core, the release build, 20,000 FiQA documents in
batches of 500, `BY sentences SIZE 100`): inserting 0.16 s without a
chunk table, 0.79 s with one (chunking is half of it, at E6's 45 MB/s;
the chunk rows the rest); 1,000 one-row updates that change a text
0.22 s and 0.28 s.  Tests: `tests/chunk_tables.rs` (the columns,
offsets and headings, a paragraph edited, a paragraph put first, key
changes, deletes, NULL texts, `INSERT … SELECT`, the refusals, two chunk
tables of one table, `DROP … CASCADE`, the consistency check), and the
crash workload of `tests/embedding_columns.rs` keeps a chunk table:
after a crash at any I/O point, every document has its chunk.

E8 is done in two: **E8a** batching, rate limits and costs, **E8b** the
*Done when* checked whole on BEIR.

**E8a — what calls cost** (done 2026-11-13).  Design item 7, with what
building it decided:

- *Limits are the model's, for any provider.*  `CREATE MODEL … WITH
  (batch_tokens = 8000, requests_per_minute = 3000, tokens_per_minute =
  1000000, parallel = 4)` — the units providers publish their limits in
  (OpenAI's RPM and TPM tiers, Voyage's and Cohere's per-minute limits),
  kept in the model record (version 3), shown in `nc_catalog.models`.
  Batches are packed in order by both the provider's text count and
  `batch_tokens`.  Tokens are estimated as UTF-8 bytes / 3 (OpenAI
  counts about 4 characters a token in English, fewer elsewhere; three
  bytes errs toward smaller batches), or counted by a local model's
  tokenizer where a token limit is set.
- *Token buckets, on the database's clock.*  Two a model (calls,
  tokens), each as large as a minute's limit and refilled steadily —
  the classic token bucket, the model providers' own (a burst up to the
  minute's limit, then the steady rate); per database, since each has
  its clock, so the simulation passes the waits without waiting.  A
  statement waits within its deadline and is refused past it (`53400`,
  the wait in its hint); a round of the embedding columns never waits:
  its jobs come back after the hint, like after a provider's
  `Retry-After` (E4), and the model's own limit does not open the
  provider's circuit.
- *Parallel calls only where they help.*  A remote model's batches go
  out on `parallel` threads (an HTTP call waits on the network, not the
  CPU); a local model already spreads a batch over its `threads`.
- *Costs as the statement's.*  The tokens a provider reports
  (`Request::report_tokens`: OpenAI's and Voyage's `usage`, Cohere's
  billed units; the local encoder's own count), else the estimate.  A
  statement's calls, texts, cache hits, tokens, errors and time are
  charged as they happen to a counter its parallel workers share (as
  rows read are, §M14.20 V4) — `EXPLAIN ANALYZE`'s `Models:` line,
  `nc_catalog.statements`' `model_*` columns (§M14.9).

Tests: `tests/models.rs` (`limits_batches_and_costs`: four calls at
once, token-packed batches, a text over the budget alone, the
`Models:` line with and without workers, the statement statistics, a
rate limit waited for on the simulated clock and refused past a
deadline), `tests/embedding_columns.rs` (`a_rate_limit_spaces_the_rounds`:
a round told to come back after a minute, no circuit, the call made kept
by the cache), `modules/embeddings/tests/providers.rs` (each provider's
usage read, nine texts in five calls three at a time, in order).

**E8b — the *Done when*, whole, on BEIR** (done 2026-11-13).
`bench/embed/beir.py` runs it in a server, per corpus: `CREATE MODEL
bge USING local WITH (path = …)` (bge-small-en-v1.5's files from disk;
no call leaves the machine), a document table with an embedding column,
a chunk table (`BY sentences SIZE 200 USING bge`) with its own, HNSW on
both, all made empty; the corpus loaded, the server's background worker
embedding, the server killed (`SIGKILL`) halfway through the queue and
started again; then every judged query ranked by `ORDER BY v <=>
embed(q, 'bge', 'query')` — documents directly, and by their best chunk.
Then a tenth of the documents rewritten and a twentieth deleted, the
queue emptied, all put back, the server killed again once the queue's
first round committed; once it is empty, every chunk row and vector must
equal the one before, bit for bit, and every document have its chunks.

| nDCG@10 | documents, HNSW | exact | fastembed, exact | chunks | after the edits: documents | chunks |
|---|---:|---:|---:|---:|---:|---:|
| SciFact | 0.7200 | 0.7200 | 0.7203 | 0.7087 | 0.7200 | 0.7097 |
| NFCorpus | 0.3375 | 0.3375 | 0.3376 | 0.3401 | 0.3375 | 0.3384 |
| ArguAna | 0.5975 | 0.5975 | 0.5973 | 0.5772 | 0.5975 | 0.5770 |

What it showed:

- *The same ranking as the reference.*  The documents' vectors, made
  inside the database by nucleus's encoder, rank as fastembed's vectors
  of the same model (ONNX Runtime) do, within 0.0003; the HNSW index,
  filled as rows arrived, finds what exact search finds (T7's gap —
  NFCorpus 0.3163 against 0.3375 exact on a bulk build — does not show
  here).
- *In sync through everything.*  Two kills, 518 to 868 rewrites and 259
  to 434 deletes a set, put back: the chunks and vectors bit for bit as
  before (the encoder's fixed reduction order, E3b), no document without
  chunks, no job failed.  The rankings after differ only where HNSW's
  graph, changed by deletes and re-inserts, does (chunks ±0.002).
- *Chunks rank about as whole documents here* — BEIR's documents are
  abstracts, mostly within the model's 512 tokens, so a document's
  vector already sees all of it (ArguAna's arguments read best whole,
  −0.02); chunks are for documents longer than the model reads.
- *Speed is the encoder's.*  48–64 vectors and 9,600–10,100 tokens a
  second on the server's worker (bge on ten threads: E3c measured
  9,929); loading and chunking 0.7–1.0 s a set.  A new rewrite's jobs
  were mostly cache hits (the texts embedded earlier in the process).

Outages and the circuit are the simulation's to test
(`tests/embedding_columns.rs`: a provider down, `Retry-After`, the
circuit, a crash at any I/O point, a rate limit), since a local model is
never down.  With E8b, §M14.22's *Done when* holds: a document table
with an embedding column stays in sync through inserts, updates,
deletes, provider outages and crashes; a model change re-embeds online
(E5); a local model embeds without any network.

**Review** (2026-11-13).  The milestone was reviewed whole, for security
and for correctness, and these were fixed (tests beside each):

- *A model called without `USAGE`.*  An embedding column's text is
  computed by the database's worker, so an `embed(…, 'theirs')` inside
  it would have called a model its maker may not use; the expression may
  not call a model now (`0A000`).
- *One row stopping every column.*  An expression that failed for one
  row (a division by zero) failed the whole round, every table's; it
  fails that job now.  A round that panics (a provider's answer) is an
  error, the driver freed.
- *Jobs a DDL did not see.*  `ADD COLUMN … GENERATED BY EMBEDDING`, `SET
  MODEL` and `CREATE TABLE … CHUNKS OF` read the table at their
  snapshot, so a row committed while they ran went without a job or its
  chunks; their scans are validated now (a conflict, run again), and
  `swap` gives a row without a next vector a job instead of NULL.
- *The server's worker asleep with work.*  A job put woke the worker
  before its writer committed; it is woken again after.
- *A chunk table's model.*  It could be dropped under the table (writes
  then failed), and writers needed `USAGE` on it; neither now.
- *Files a model repository chooses.*  `config.json`'s numbers sized
  allocations (a server abort), a tensor's shape could wrap, and a
  200 KB Unigram piece overflowed the stack; bounded and checked now.
  Redirects are followed only to `https://`; `nucleus-http` refuses URLs
  with spaces or control characters (a request line injected) and chunk
  sizes that wrap.
- Batches answered beside a failed one are kept (they were paid for);
  `ATTACH PARTITION` refuses a table with chunk tables.

A model maker's `url` could name loopback or internal hosts (SSRF):
decided (2026-11-13) — administrators' models may call anywhere, anyone
else's only URLs under the database's `model_urls`, an administrator's
list (scheme, host and port the same, the path under the prefix's at a
`/`), checked when the model is made and at every call, so a URL taken
off the list stops being called; a provider's own address needs no
listing.

Done in §M14.29 S2: a fetched model's file may be at most a quarter of
the machine's memory (and 8 GB) — it arrives in memory and its weights
run there, so one larger could never run — and its download ends within
the statement's time and an hour; a remote call ends within the time
its statement has left (at most five minutes, the retry's pause
included: `nucleus-http`'s `total`), so an endpoint dripping a byte
within each read's timeout no longer holds it.

Open, for the security milestone (§M14.29) or a decision: `USAGE` on a
secret is, in effect, reading it, and revoking `USAGE` does not touch
models and columns already made; the server's key file's permissions
are not checked.  The rate limit's wait is not cancellable without a
statement deadline.

### §M14.23 — Embedded editions ✅

The quickest start there is — no server, no setup, `open` and query — is
how a developer, or an AI agent choosing a tool, meets a database first
(SQLite's reach, DuckDB's, PGlite's on the browser-based coding
platforms).  The same engine, SQL, types and vectors as the server, as a
library, so a prototype that grows moves to a server without code changes.
Not a fork and not a "lite" engine: editions are build profiles over the
same core, choosing modules (§M14.19), so a phone build leaves out what it
does not need and stays small.  In order:

1. **In memory and on a file, inside Python and JavaScript.**
   `nucleus.open("app.db")` or `nucleus.open(":memory:")`; Python (a
   native wheel per platform, the same DB-API as the network driver) and
   Node / Bun / Deno (a native addon), with the network drivers' API, so
   code switches by changing the connection string.  Results reach numpy
   and Arrow without a copy.  In-memory mode is the platform layer's
   memory file system made a product: no files, optionally saved to one.
2. **The browser (WASM).**  The engine compiled to `wasm32`, storage in
   OPFS (persistent) or in memory, run in a worker; a JavaScript / TypeScript
   package with the same API.  For in-browser development platforms,
   playgrounds (§M14.17), tests and offline web applications.
3. **Phones.**  iOS (Swift package, `XCFramework`) and Android (Kotlin
   library, AAR) over the C API, with each platform's durability rules;
   a size budget for the mobile profile.
4. **Sync with a server** is §M16.7.

What it costs, and how that is met: more platforms in CI (each edition
runs the platform conformance and slt suites on its targets); native
packages per platform (built by CI, like the release packages); a crash
in the engine takes the host process with it (the engine already runs
under the simulator's crash and fault tests); one process per data
directory (the directory lock holds — an embedded process and a server do
not open one database at once); and a file format that lives on many
devices for years, so D15 must be settled before the first embedded
release, and every older format stays readable.

**Done when:** each edition passes the slt suite and the platform
conformance tests on its targets; a Python and a JavaScript example open a
file, query it, then run unchanged against `nucleus server` by changing
only the connection string; the browser edition persists across reloads;
and the iOS and Android libraries run the examples on a simulator within
the size budget.

**Step 1 ✅ (2026-11-13): Python and JavaScript, in memory and on a
file.**  D15 settled first (each format versioned on its own and
checked on open, one release version).

- **How a database gets into the process.**  `editions/embedded`
  (`nucleus-embedded`, a `cdylib`, `staticlib` and `rlib`) opens the
  engine as `nucleus server` does — the maintenance thread, the
  embedding columns' worker, the same registry — and serves each
  session with the nucleus protocol's own server over two pipes in the
  process instead of a socket, behind a C API of nine functions
  (`nucleus_embedded.h`).  The drivers' protocol code then runs
  unchanged over it, so the embedded edition *is* the network driver's
  API, not a second one to keep level with it: the whole Python and
  JavaScript conformance suites pass on it (logins skipped: there are
  none in the process) and the slt suite passes through it.  Prior art
  weighed: SQLite and DuckDB expose a call-level API per language (fast,
  but a second API in every language, which nucleus's drivers would
  have to mirror); PGlite runs PostgreSQL in WASM behind its wire
  protocol in memory — the shape taken here, for the same reason, one
  protocol and so one API.  The price is a pipe instead of a call: one
  copy each way and a thread hand-off.
- **In memory** is `nucleus_platform::memory::MemoryPlatform` — the
  platform layer's memory file system made a product (one byte vector
  per file, no durable image and no faults, unlike the simulation),
  under the machine's clock; it runs the platform conformance tests.
- **Closing.**  `Database::close` ends the sessions, stops the
  background threads (`nucleus_server::Stop`, `maintain_until`;
  `embedding::stop`), checkpoints and frees the directory, so a
  process can open and close databases at will; opening a directory the
  process has open already shares it.
- **Python** (`ctypes`, so one wheel per platform for every Python 3:
  `py3-none-<platform>`, 5.3 MB on macOS arm64; `build_wheel.py`, the
  standard library only) and **JavaScript** (a Node-API addon in C,
  `native/addon.c`, linked with the engine into one `nucleus.node`; a
  reader thread per session hands bytes to the event loop through a
  thread-safe function, at most 16 chunks queued, so a slow consumer
  holds the session back as a socket would).  Both drivers take
  connection strings — `file:app.db`, `:memory:`,
  `nucleus://user:password@host:port/db?tls=true` — so
  `examples/{python,js}/anywhere` run in a directory, in memory and
  against a server with only the string changed (`driver-tests.sh
  python-embedded js-embedded`, in CI).  Bun and Deno load Node-API
  addons; CI runs the example under both.
- **Measured** (M-series Mac, Python, 10 000 round trips after a
  warm-up): `SELECT 1` 30 µs over TCP loopback to `nucleus server`,
  18 µs over the pipe with a condition variable, **10.5 µs** once a
  reader spins 50 µs before it sleeps (adaptive spinning, as glibc's
  adaptive mutexes and the LMAX Disruptor's wait strategies do: a reply
  usually comes within microseconds, and parking and waking a thread
  costs more than that).  A million rows of two columns into numpy:
  29–37 ms in the process, 31–36 ms over TCP — the engine and decoding
  bound it, not the transport.  An in-memory database first had a
  32 MiB page cache; a million-row scan through it took twice as long
  (74 ms against 36 ms), so it keeps the file's 128 MiB default (pages
  are allocated as first cached).  Sizes: the shared library 13.7 MB
  (macOS arm64, release, both model modules in).
- **Not yet:** the platform packages are built by `build_wheel.py` and
  `native/build.sh` on each machine; CI builds and tests them on Linux,
  and publishing (PyPI, npm with one package per platform) follows the
  first tagged release (D15).  Windows' addon needs node-gyp's import
  library (the Python wheel builds there as is).  An in-memory database
  cannot yet be saved to a file (planned: `nucleus backup` into a
  directory, §M14.18).  An embedded database has no keyring for
  `CREATE SECRET … VALUE` yet (secrets `FROM ENV` work).

**Step 2 ✅ (2026-11-13): the browser (WASM).**  A page opens
`connect("file:app.db")` — the same driver API — and gets a database in
the page, kept in the origin's OPFS across reloads and browser restarts.

- **The target: `wasm32-wasip1`, not `wasm32-unknown-unknown`.**  The
  engine uses the standard library's clocks, randomness and console, and
  WASI gives them through fifteen imported functions, which the driver
  provides in the page (`wasm.ts`, about 100 lines; no files — the engine
  has none of its own there).  On `unknown-unknown` each would need a
  stub inside the engine.  Tested under Node's WASI (`scripts/wasi-run.mjs`
  as cargo's runner; CI's `wasm` job): platform conformance, storage,
  transactions and SQL with the whole slt suite.  WASI has no threads, so
  a statement's workers are 1 there (`budget::workers`), and the places
  that started a thread even for one part — a sort's parts, HNSW's
  `par_map` and its searches among rows, a remote model's parallel calls —
  run on the caller's thread; the engine took the process id for a seed
  (no pids on WASI: now `nucleus_platform::process_id`, 0 there).  Tests
  that exercise threads themselves are skipped there (`cfg_attr(target_os
  = "wasi", ignore)`), none of the rest.  WASI has no file locks either:
  `RealPlatform` falls back to a lock within the process.
- **Sessions without a thread.**  The engine reads from its client in the
  middle of a statement (`COPY` data, a fetch's next batch), so a session
  needs something that can wait — a thread, which a browser's
  WebAssembly has only with cross-origin isolation (COOP / COEP headers a
  page cannot always set).  Rewriting the engine as async was weighed and
  not taken (every executor path would change for one platform); instead
  `nucleus_server::Exchange` is the same server driven by the bytes given
  to it (sans-IO, as `rustls` and `quinn-proto` are driven): each `feed`
  runs the whole messages it has.  Where a statement would read more,
  a stepped session does what can be done without waiting: a `COPY`'s
  data must come with the `COPY` (one without fails, `57014`; data after
  it is dropped, as PostgreSQL drops it); a fetch's batches follow one
  another without an `S` to answer; one statement's reply holds at most
  256 MB (`54000` past it — the wasm32 address space is 4 GB).  PGlite
  takes the same shape (its PostgreSQL single-user in WASM, a whole
  message at a time).  The client is *eager* there: `copy` and `load`
  gather the data and send it with the `COPY` (`load` learns the
  columns' types by a `LIMIT 0` query first).  `Database::exchange`
  gives one in Rust; the slt suite passes through it.
- **Storage: a pool of OPFS files opened ahead.**  A browser opens an
  OPFS file for synchronous access only in a worker and only by an
  asynchronous call — which the engine, synchronous, cannot make
  mid-statement.  Prior art: SQLite's `opfs` VFS proxies each call to a
  second worker and blocks on `Atomics.wait` (needs `SharedArrayBuffer`,
  so cross-origin isolation again); its `opfs-sahpool` VFS (and
  wa-sqlite's access-handle pool) opens a pool of files ahead and keeps a
  file's path in each one's header — no isolation needed, one tab at a
  time.  Taken: the pool, written in Rust over a `Slots` trait
  (`nucleus_platform::slots`, DOCS §3.7) so that it runs the platform
  conformance tests and the KV contract natively; made crash-safe beyond
  sahpool's single header — two header copies with a generation and a
  checksum, so a torn header write and a rename cut short both resolve to
  a whole state.  The host tops the pool up between calls (eight free,
  sixteen added).  A database uses few files (its data file, WAL
  segments, the lock), so sixteen slots go far.
- **The JavaScript driver, one API.**  Its protocol code had Node's
  modules in it (sockets, `Buffer`, `crypto`); it is now `core.ts`, with
  none: `bytes.ts` is Node's `Buffer` or, without one, a `Uint8Array`
  with the methods the driver uses, and SCRAM runs on WebCrypto (Node 20
  has it; the Node suites pass on it, TLS channel binding included).
  `index.ts` (Node: sockets, the addon) and `browser.ts` (the package's
  `browser` export) share it.  In a page the engine runs in a module
  worker (`worker.ts`); in a worker, Node, Bun and Deno on the calling
  thread, so the same module is tested outside a browser: the whole
  JavaScript suite passes on it in Node, on a pool of files
  (`node-slots.ts`) and in memory (`driver-tests.sh js-wasm`).
- **In a real browser** (`drivers/js/test/browser.mjs`: headless Chrome
  over the DevTools protocol, no dependencies; in CI):
  `examples/browser/index.html` counts its visits in OPFS — a new
  profile's first, after a reload, left open (no close: the tab's end as
  a crash), after a reload of that, and after Chrome restarted on the same
  profile: 1, 2, 3, 4, 5.
- **Measured** (M-series Mac, Chrome 157 / Node 24).  The first answer
  (engine fetched, compiled and started, the database opened from OPFS,
  three statements) 95–140 ms after the page starts, 52–106 ms once
  cached.  In Node on the same module, in memory: a 200,000-row
  `INSERT … SELECT` 229 ms, a `GROUP BY` over it 44 ms, a point query
  26 µs a round trip.  Size: 7.1 MB, 1.65 MB with brotli (2.3 MB gzip),
  the model modules left out (no provider's key belongs in a page, and no
  local model runs there).  How it was chosen: the default release build
  8.5 MB / 1.74 MB; with LTO and one codegen unit 7.1 MB / 1.65 MB at the
  same speed — taken (`wasm/build.sh`); `opt-level = "s"` 1.50 MB but
  inserts and sorts 20 % slower, `"z"` 1.44 MB and 50 % slower — not
  taken: 0.2 MB of transfer is worth less than the speed.  `wasm-opt`
  (Binaryen) is used when installed.
- **Found on the way:** the engine read the process id for the HNSW
  seed — a panic on any platform without one.
- **Not yet:** a statement in the browser cannot be read lazily (a
  stream's result is produced whole, bounded as above); one tab holds an
  origin's storage at a time (a second gets an error, not a shared
  database — sharing it across tabs, e.g. through a `SharedWorker` or the
  Web Locks API, is a later step); the npm package with `wasm/` is
  published with the others (D15); no `wasm-opt` in CI yet.

**Step 3 ✅ (2026-11-13): iOS and Android.**  An app opens a database in
its own directory — `Database.open(named: "notes")` in Swift,
`Database.open(context, "notes")` in Kotlin — and gets the driver API it
would use against a server.

- **The shape: the engine as a static library, the drivers over its C
  API.**  Prior art weighed: SQLite is on both systems already, and apps
  reach it through a call-level layer per language (GRDB and SQLite.swift
  on iOS, Room on Android); Realm and Couchbase Lite ship a C++ core
  with a C API under per-platform SDKs; DuckDB's Swift package wraps its
  C API.  Taken: the core with a C API, as those do — but the language
  layer is not new, as in step 1: the session is the nucleus protocol
  over `nce_send` / `nce_recv`, so iOS gets the **C driver** (compiled as
  it is into the Swift package, with one new entry, `nc_connect_io`: a
  login over a transport of the caller's, `nc_io`) and Android the
  **Java driver** (`NucleusClient.connect(in, out, transport, config)`,
  over streams of JNI calls).  The Swift package is a driver of its own
  too (`Nucleus`, against a server; on Linux as well), with the same
  conformance cases as the others; on a device they run against a
  database in the app.
- **iOS.**  `drivers/swift/build-xcframework.sh` builds the static
  library for `aarch64-apple-ios`, the simulator (`aarch64` and
  `x86_64`) and macOS, and makes `NucleusEmbedded.xcframework`; the
  package's `NucleusEmbedded` product is there when it is (a binary
  target: SwiftPM's way to ship a prebuilt library; publishing it as a
  release asset with a checksum comes with D15's first release).  The
  database lives in Application Support (kept and backed up; `Caches`
  may be purged by the system).  Durability as on macOS: a sync is
  `F_FULLFSYNC` (`fsync` alone leaves the data in the drive's cache —
  Apple's own advice, and SQLite's `fullfsync`).
- **Android.**  `drivers/android/build-native.sh` links the static
  library with a JNI glue of eight functions (`nucleus_android.c`) into
  `libnucleus_android.so` per ABI with the NDK's clang (API 26 —
  Android 8, the library's `minSdk`), exporting only `Java_*` so
  `--gc-sections` drops what they do not reach, aligned for 16 KB pages
  (Android 15 devices may use them, and Google Play requires it of new
  apps).  Gradle (AGP 8.13, Kotlin 2.2) builds the AAR: Kotlin's
  `Database` and the Java driver's sources.  The Java driver needed
  changes to run there: it used APIs newer than Android 8 has
  (`Stream.toList`, `ByteArrayOutputStream.writeBytes`, `ByteBuffer`'s
  absolute bulk `get` / `put`, `Files.readString`) — replaced by older
  ones, and Android's lint (`NewApi`) now checks it.
- **The size budget: 8 MiB for arm64,** what the engine adds to an app
  (the model modules left out: a phone app calls a provider through its
  own backend, and a local model is a dependency of its own).  Measured
  on iOS as an app links it — a program calling the C API, dead code
  stripped, then stripped — and on Android as the `.so`: **7.0 MiB on
  iOS** (7,359,552 bytes; 2.4 MB with xz, as the App Store compresses),
  **7.45 MiB on Android arm64** (7,813,912 bytes; 3.8 MB gzip, 2.6 MB
  xz) — x86_64 9.3 MB, for emulators only.  Near the browser's 7.1 MB,
  from the same source with the same settings (LTO, one codegen unit,
  speed over size, §M14.23 step 2).  SQLite is under 1 MB; nucleus
  carries its vectors, text search, columnar execution and the protocol
  server, and the budget is checked by both build scripts, in CI, so it
  only grows on purpose.  *Grown on purpose to 9 MiB* (2026-11-14):
  JSON (§M14.15 J1–J3 — the type, SQL/JSON paths, the SQL/JSON
  functions, `JSON_TABLE`) took Android arm64 to 8.17 MiB (8,570,648
  bytes, +757 KB) and iOS to 7.71 MiB (8,081,696 bytes).  Of the growth, about 270 KB is
  JSON's own code with what it inlined; the rest is the parser's forms,
  sort and drop instantiations and their unwind tables.  Not taken:
  `panic = "abort"` (unwind tables are ~950 KB, but the C API turns a
  panic into an error rather than ending the app) and `opt-level = "s"`
  (20 % slower, as measured in step 2).
- **Tested** on the iOS simulator (iOS 18) and an Android emulator
  (API 35, arm64 with 16 KB pages; x86_64 in CI): the Swift suite (19
  tests: the conformance cases on a database in memory and in a
  directory, a directory kept after closing, a closed database refusing
  sessions), the Android library's instrumented tests (the Java driver's
  conformance cases on a database in memory and in a directory, the
  directory kept, a closed database refusing sessions), and the Notes
  examples (`examples/ios/Notes.swiftpm`, SwiftUI;
  `examples/android/notes`), launched twice — the second launch finds
  the first one's notes.  The engine's own suites run on the phones'
  targets with a runner (`xcrun simctl spawn` on iOS,
  `scripts/adb-run.sh` on Android): platform conformance, storage,
  transactions, KV and SQL with the whole slt suite — 487 tests pass on
  the iOS simulator, 486 on the Android emulator (one long test ignored
  everywhere), in CI too (the `ios` and `android` jobs).
- **Found on the way:** opening a directory synced every ancestor
  directory; Android lets an app open none above its own
  (`/data/user`), so a database in an app's files failed (`EACCES`).
  Now only the parents of the levels made are synced — which is all
  durability needs.
- **Not yet:** the XCFramework and the AAR are built on each machine
  (and in CI); publishing them (a Swift package release with the
  XCFramework as a binary asset, Maven Central) follows the first
  tagged release (D15).  The C driver in the Swift package has no TLS
  (a server from a phone: Network.framework's TLS would be the way).
  Sync with a server is §M16.7.

### §M14.24 — Memory for AI models ✅ (R1–R7, 2026-11-14)

A model's weights are a poor place for knowledge: a fact in them cannot be
corrected without retraining, cannot really be deleted, is the same for
every user, and cannot say where it came from.  Held in nucleus and read
while the model generates, the same fact can be all four — so models
become reasoning engines over a memory that nucleus keeps, built from what
the milestones before this give (vectors, search, embeddings, row-level
security, time travel, embedded editions):

- **Updatable**: knowledge and memories are rows, changed in transactions
  and visible to the next retrieval; a model (or an agent) writes what it
  learns back — `remember` / `recall` / `forget` as SQL functions, driver
  helpers and MCP tools (§M14.10) — with near-duplicate merging by
  similarity, confidence, and expiry or decay per memory.
- **Erasable, provably**: `FORGET` removes a subject's data everywhere it
  lives — tables, vector and text indexes, the chunks and embeddings
  derived from it (lineage from §M14.22), time-travel history (§M14.14,
  overriding retention) — and from WAL and backups by crypto-shredding:
  data is encrypted with a per-subject key, and destroying the key erases
  every copy.  The erasure is recorded, and a check confirms nothing
  readable is left.
- **Permissioned**: retrieval runs under row-level security (§M14.25), so
  a model answering a user knows only what that user may see — in
  search, in ranking and in what is cited.
- **Auditable**: every retrieved row carries its source and version, so
  an answer can cite them; a generation can record what it read (its
  read set, which the transaction engine already tracks) under a request
  id; and `AS OF` (§M14.14) answers what the system knew at a given
  moment.
- **Fast enough to read while generating**: a retrieval API for inference
  servers — prepared retrievals, many queries per call (one per sequence
  in a batch, or per step), results as vectors and ids without
  conversion — in process through the embedded editions (§M14.23), on
  the vectorized executor (§M14.20), with a latency target per lookup
  for a hot working set in memory.
- Documented schemas and runnable examples beside the agent patterns
  (§M16.5): a knowledge base with citations, per-user memory, erasure on
  request.

**Done when:** a fact changed or forgotten is reflected in the next
retrieval; after `FORGET`, no table, index, derived row, history version,
WAL segment or backup yields the subject's data (checked by scanning
every copy); retrieval never returns or cites a row the user may not see
(leak tests); an answer's citations and recorded read set match what was
retrieved; and in-process retrieval meets its latency target.

**The study (2026-11-13).**  What exists, what others do, what nucleus
takes and why, and the steps it leads to — to be agreed before they are
built.

*What exists to build on* (surveyed in the code):

- **Retrieval under row-level security** already: every table read is
  wrapped in the policy's barrier (`Plan::Secure`), and an HNSW or text
  ranking (`IndexMethod::rank`) evaluates the policy, then the
  statement's filter, on each candidate, searching on until `k` visible
  rows pass; a partitioned HNSW index picks a tenant's graph from a
  policy's equality.  One gap: BM25's statistics (N, document
  frequencies, the average length) count the rows the policy hides, so
  hidden rows shift visible rows' scores — a statistical side channel.
- **Embeddings and their lineage** (§M14.22): an embedding column lives
  in its row; a chunk table's rows reference their document by a foreign
  key `ON DELETE CASCADE`, so deleting a document deletes its chunks and
  their vectors in the same transaction.  No lineage beyond foreign keys.
- **Read sets**: the transaction engine records the keys and key ranges
  a serializable transaction read (`ReadSet`, FoundationDB-style
  validation) — physical keys (rows, index entries, catalog), private,
  per transaction; not what a citation needs (rows and their versions).
- **Crypto**: ChaCha20-Poly1305 (RFC 8439, `nucleus-crypto`), used for
  `CREATE SECRET` under a keyring key kept outside the data directory.
  Table data, pages and the WAL are not encrypted.
- **Time**: older snapshots by `TxnOptions::at` within `history`
  commits (0 by default) — no `AS OF` in SQL (§M14.14), no history by
  time.  No backups yet (§M14.18).  No row expiry (credentials expire;
  storage policies convert partitions, they delete nothing).
- **MCP** (§M14.10): `list_tables`, `describe_table`, `sample_rows`,
  `summarize_table`, `query`, `explain`, `lint`, `execute` — no memory
  tools.
- **Where a deleted row's bytes stay after `DELETE` and `COMMIT`** —
  the core of "erasable, provably":
  1. its old versions in the B+tree, until a commit rewriting their leaf
     or `VACUUM` collects them (the server vacuums when idle), and they
     come back by WAL replay after a crash until then;
  2. copy-on-write: every page a change copied goes to the free list
     with its old image whole — not zeroed — until reused;
  3. the WAL: its records until a checkpoint (after 1 GiB by default);
     then on ext4 / XFS / NTFS a dropped segment becomes a spare,
     renamed, not overwritten (on APFS unlinked);
  4. the HNSW index: a deleted row's vector stays in its node's page
     ("nodes are never renumbered") and in the object cache until the
     index is rebuilt — the graph only cuts dead nodes from its edges;
  5. the text index: postings and positions stay in a sealed segment
     until a merge rewrites it — with tiered merging, possibly never;
  6. column storage: a segment row's bytes until a merge rewrites the
     segment;
  7. the model cache (32 MB in memory, keyed by a hash of the text):
     the text's vector;
  8. statement statistics (memory only): each statement's last text
     with its constants, kept for the index advisor;
  9. the audit log (persistent, hash-chained): statement texts, literals
     included, when statements are audited — and removing one breaks
     the chain;
  10. beneath the file system: on flash, a block overwritten in place is
      written elsewhere (wear levelling), so the old block may survive
      any overwrite the database makes (NIST SP 800-88 counts overwriting
      as unreliable on flash; cryptographic erase is its answer).

  A vector is not safe to keep either: text embeddings can be inverted
  back to much of their text (Morris et al., *Text Embeddings Reveal
  (Almost) As Much As Text*, 2023).

*Prior art.*

- **Memory layers over a store**: Mem0 (Chhikara et al., 2025) extracts
  facts from a conversation by a model, then decides per fact by a
  model's tool call — ADD, UPDATE (merge into a similar memory), DELETE
  (a contradicted one), NOOP; on LoCoMo, a p95 latency 91 % below
  putting the whole history in the context.  Letta / MemGPT: memory
  blocks pinned in the context and edited by tools, recall memory (the
  conversation, searchable) and archival memory (searched by
  similarity).  Zep / Graphiti (2025): facts as edges of a temporal
  graph, **bi-temporal** — when a fact held (valid time) and when the
  system learned it (transaction time); a contradicting fact closes the
  old one's validity instead of deleting it.  LangGraph's store:
  namespaced key / value items with a similarity search.  All of them
  sit *over* a database (Postgres with pgvector, Neo4j, a vector store):
  transactions, permissions and erasure are the application's.
- **Benchmarks**: LongMemEval (Wu et al., ICLR 2025: 500 questions over
  ~115k-token histories; information extraction, multi-session and
  temporal reasoning, knowledge updates, abstention; evidence sessions
  labelled, so retrieval itself can be scored) and LoCoMo.
- **Erasure**: SQLite's `secure_delete` (deleted content and free-list
  pages overwritten with zeros; its `FAST` mode leaves the free list,
  and FTS5 shadow tables need their own option); crypto-shredding in
  event-sourced systems (Verraes; Akka's GDPR support: data encrypted
  per subject, the key destroyed; keys need their own retention, and a
  key cached in memory outlives its deletion); NIST SP 800-88
  cryptographic erase.  The legal reading is not settled — some hold
  that encrypted personal data is still personal data whoever holds the
  key — so shredding alone is not taken as erasure where the bytes can
  be removed.
- **Retrieval while generating**: RETRO and kNN-LM read a store per
  chunk or per token — the reason for batched, low-latency lookups.

*What nucleus takes, and why.*

1. **Memories are rows; judgement stays with the model.**  Deciding
   that a new fact contradicts an old one is a model's call (Mem0 makes
   it with an LLM).  The database does what it alone can do well:
   store, find near-duplicates deterministically, version, permission
   and erase — transactionally.  So `remember` merges only what is
   near-identical (cosine similarity above a threshold, within the same
   subject and only among rows the caller may see) and otherwise
   inserts; a contradiction is recorded by the caller naming what the
   new memory replaces, which closes the old one's validity
   (bi-temporal, as Graphiti: valid time in columns, transaction time
   from the engine) instead of deleting it.  `forget` deletes a memory
   (a row; as built in R1 — a correction is `REPLACING`, which closes
   validity); `FORGET` erases a subject everywhere.
2. **A memory store is an ordinary table with known columns**, made
   by a module (principle 12): subject (whose memory), kind, content,
   an embedding column, confidence, valid from / to, expiry, source
   (provenance), replaces; an HNSW and a text index on it; recall as a
   hybrid ranking (both indexes, `rrf`) filtered to what is valid and
   unexpired, with optional decay by age.  Everything SQL does on a
   table works on it, and an application can design its own table and
   still use the functions.
3. **Erasure in two layers.**  *Scrubbing* — on this node, the engine
   removes the bytes from every place listed above, then proves it by
   scanning every file of the directory for the subject's values, their
   encodings and vector bytes.  *Crypto-shredding* — for copies that
   leave the node (backups, a replica's log, flash's remapped blocks):
   a subject's values sealed under a key of their own, the keys outside
   the data and its backups, destroyed by `FORGET`.  Neither alone
   suffices: scrubbing cannot reach a backup or a remapped block;
   shredding leaves plaintext in indexes, the WAL and caches, which hold
   derived data in the clear.  So backups will carry rows, not index
   pages (indexes rebuilt on restore), and the live node is scrubbed.
4. **Erasure is a statement, `FORGET SUBJECT …`,** over every table
   with a column marked as a subject, following foreign keys (the
   lineage of §M14.22) — overriding history retention (§M14.14) — and
   it leaves a record saying what was erased, when and by whom, without
   the subject's data; the audit log keeps a hash of each record's
   detail in its chain, so a detail can be erased and the chain still
   checks.
5. **Citations are rows and versions, not keys.**  `recall` returns each
   memory's table, key and version (its commit's timestamp); a session
   can record what its statements retrieved under a request id into a
   log the application reads back.  The engine's read set stays what it
   is (validation); citations come from the retrieval itself.
6. **Speed by the editions already built**: in the process (§M14.23:
   10.5 µs a round trip from Python), many queries per call (one
   statement, an array of query vectors, a lateral top-k each), results
   as columnar batches — vectors as float buffers, no conversion.  The
   latency target is set from the first measurement (the HNSW's search
   among writes is 0.9 ms on SIFT-1M today), for a hot set in memory.

*Steps* (agreed 2026-11-13, in this order):

- **R1 — memory stores and remember / recall / forget**: the module;
  `CREATE MEMORY STORE`; the functions; the near-duplicate merge;
  valid time and replacement; expiry by the maintenance thread (a
  general row expiry, `SET EXPIRY (column)`, since none exists); driver
  helpers and MCP tools.  Measured: retrieval recall@k on LongMemEval's
  evidence sessions against its published retrievers (no model needed
  to score retrieval).
  **R1 ✅ (2026-11-13).**  As built (DOCS §8.28, §6.5):
  - **Not a module but the SQL layer's own** (`memory`, `expiry`):
    `CREATE MEMORY STORE`, `REMEMBER` and `recall` are grammar and
    binding, which a module cannot add yet (principle 12's registry
    takes functions, types, index methods and models, not statements);
    the store itself is made by SQL the layer writes, so it is an
    ordinary table — granted, policed, dumped, dropped as any.  Functions
    could not do it either: a function gets a transaction and the
    registry, not the session's privileges and policies, and a memory
    written past them would leak (study point 1).  So `REMEMBER` runs
    its steps as statements of the caller's (`execute_statement`), and
    `recall` is bound as a query — `Binder::expansions` replaces its
    `$n` by the call's arguments while binding it.
  - **The merge**: the same text of the subject first (exact, whether
    or not its vector is made yet), else the nearest current memory by
    the store's model at cosine ≥ `merge` (0.95 by default) — among the
    rows the caller sees: a test keeps a hidden memory with the same
    text, which the application's `REMEMBER` does not touch (it adds its
    own).  A memory whose vector the queue has not made yet is a
    candidate too, its text embedded here (the model's cache has it) —
    CI's embedded-edition run found the merge depending on the queue's
    pace.  `REPLACING id` closes the old memory's validity (only a
    current one of the same subject: `42704`).  `forget` is `DELETE`
    (the drivers' and the MCP tool); `FORGET SUBJECT` is R2's.
  - **Row expiry is general**: `ALTER TABLE t SET EXPIRY (column)` on
    any table, deleted by the maintenance thread every minute as
    ordinary deletes (foreign keys, indexes, derived rows follow);
    `recall` leaves out the expired rows not yet deleted.
    `nucleus_server::maintain_with` takes the edition's registry, so
    deletes reach a module's types and models.  Prior art: Cassandra's
    and ScyllaDB's TTL per cell, MongoDB's TTL indexes (a background
    thread every 60 s — the cadence taken), PostgreSQL's none
    (`pg_cron` jobs instead); a column, not a per-row TTL, so the
    expiry is visible and indexable.
  - **MCP**: `nucleus mcp --memory` offers `remember`, `recall`,
    `forget` without the general `execute` (a memory-keeping agent need
    not be given every write); a tool not offered is refused as unknown.
    Drivers: Python's and JavaScript's `remember`, `recall`, `forget`.
  - **Measured** (`bench/memory/longmemeval.py`, `docs/benchmarks.md`):
    LongMemEval_S, every user turn of 470 questions a memory of its
    question's subject (114,902), scored by the paper's code's metrics.
    `recall()` — BM25 and bge-small vectors fused — finds every
    evidence turn in its first 10 for **0.819** of the questions,
    against **0.592** for the paper's own BM25 baseline run here on the
    same data, 0.737 for nucleus's BM25 alone and 0.780 for the vectors
    alone (session R@5 0.897 / 0.685; multi-session questions 0.661 /
    0.372, the hardest).  Fusion is what wins at turn level; vectors
    alone are slightly better at session level (0.912).  The paper's
    published retrieval tables are on LongMemEval_M (500 sessions a
    question; Stella V5 1.5B, round level K = V: R@5 0.582, R@10 0.692)
    — a harder haystack, not compared.  HNSW within a subject equals
    exact search here (~244 rows a subject: the filtered search reads
    them all).
  - **Found and fixed on the way — a plan without statistics**: the
    first run took 398 ms a recall.  A table never analyzed — as a new
    store is — had its key range (`subject = $1`) replaced by the text
    index's condition (`method_filters` treats a range without an
    estimate as large), so BM25 scored the words' matches among all
    114,902 memories and filtered by subject after.  Now an equality on
    every column of an index's key stays without statistics (as
    `choose_access` already prefers it then); the plan reads the
    subject's rows and checks the words on them: 23.5 ms with the
    query's embedding, 9 ms for BM25 alone.  Analyzed, both indexes rank
    among the subject's rows: `recall(…, 10)` 4.4 ms, plus ~12 ms to
    embed the query with the local model.  Automatic `ANALYZE` (the
    maintenance thread's, as PostgreSQL's autovacuum) would give every
    store the analyzed plan — not built; R4 sets the latency target.
  - `REMEMBER` one at a time: 28 ms median (its text embedded to find a
    near-duplicate, then inserted; the column's vector made by the queue
    from the model's cache).  Batched remembering is R4's.
- **R2 — `FORGET` and scrubbing** ✅: subject columns; the cascade; the
  bytes removed from versions (vacuum of the ranges, history overridden),
  freed pages (zeroed — `secure_delete`'s choice; its cost measured, and
  whether every freed page or only a forget's), the WAL (a checkpoint,
  the dropped segments and spares zeroed), HNSW (the dead node's vector
  zeroed in place), the text index and column segments (rewritten),
  caches, statement statistics, the audit log (details by hash); the
  verification scan; the erasure record.  Measured: the bytes found by
  the scan before and after, on a planted corpus.
  *Where a deleted row's bytes stay* (surveyed 2026-11-13): its versions
  and their tombstones — the keys of its index entries hold its indexed
  values — until a commit's collection or a vacuum passes the watermark,
  which history (§M14.14) holds back; the B+tree's freed pages (copy on
  write: every rewritten leaf leaves its old page in the free list,
  bytes intact) and overflow chains — values of 1 KiB and more are
  LZ-compressed, so a plain byte search misses them; the WAL's segments
  up to the checkpoint — dropped ones deleted (the blocks keep them) or
  renamed to spares; the HNSW's vector pages (a dead node keeps its
  vector until a consolidation rewrites the page), sealed objects and
  the decoded object cache; the text index's segments (a deleted
  document's terms, positions and key stay in the dictionary and the
  postings until a merge drops it) and its tombstones (the old text's
  terms); column segments until a merge; `ANALYZE`'s kept sample rows
  and most common values (`00 04`); an embedding column's queue
  (`00 17`); the model's embedding cache; the session caches by text
  (plans, parses); the audit log's statement texts.  Leaves are encoded
  whole with their free space zeroed, so a live page keeps no removed
  cell.  Statement statistics keep normalized texts (constants as
  `$n`).  In three parts:
  - **R2a — `FORGET SUBJECT`**: subject columns (`ALTER TABLE t SET
    SUBJECT (c, …)`; a memory store's `subject` marked when made); the
    statement deletes the subject's rows from every such table as the
    caller (privileges and policies — and all of the subject or nothing:
    a row left that row security hid refuses it), their foreign keys'
    actions and derived rows following; the erasure record
    (`nc_catalog.erasures`: when, who, the tables and rows — no value of
    the subject); its own audit record and logs with its constants
    masked.
    **R2a ✅ (2026-11-13).**  As built (DOCS §8.29): subject columns of
    any type, the value cast into each (a column it cannot be a value of
    holds none of the subject's rows); the tables deleted from
    referencing ones first, so `RESTRICT` between subject tables does not
    stop it, while a key from an unmarked table acts as declared; all or
    nothing checked by counting as the system what is left after each
    delete; `FORGET` refused in a transaction block (the scrubbing after
    its commit is not transactional, as `VACUUM`).  Prior art: GDPR
    article 17 tooling sits outside databases — Snowflake's and
    BigQuery's tags on columns marking personal data drive policies, not
    deletes; MongoDB's and Cassandra's TTLs expire, they do not erase by
    subject; event-sourced systems (Verraes) erase by key.  A subject
    column is the column-tag model taken into the engine, where the
    delete, its cascade and the proof can be one statement.  The record
    keeps no hash of the subject: a hash of an e-mail address is guessed
    back by hashing candidates, so the record numbers the erasure and the
    requester keeps the number.  Not yet: an MCP tool (R7's).
  - **R2b — scrubbing**: the affected tables' versions purged past
    history, once no older snapshot runs; the index methods' structures
    rewritten without the deleted rows (HNSW vector pages, text
    segments, column segments) and their caches dropped; statistics'
    samples remade; the embedding cache's entries and the session caches
    dropped; audit details by hash (the chain hashes a detail's hash, so
    a detail can be blanked and the chain still checks); then the store:
    a checkpoint, the WAL moved to a new segment and checkpointed again,
    dropped segments and spares overwritten before they go, the free
    pages zeroed.
    **R2b ✅ (2026-11-13).**  As built (DOCS §8.29, §4.5, §4.6, §6.1):
    after the commit, before the row is sent, for every table the delete
    could reach (subject tables, those referencing them transitively,
    partitions): `IndexMethod::scrub` — an HNSW graph sealed, folded and
    consolidated, then each node cut from it whose row is not current
    zeroed (vector and key) and its vector pages rewritten; a text or
    sparse index's segments with deleted documents merged one by one —
    `column_store::scrub` (the delta taken in, segments with deleted rows
    rewritten), samples remade, the object and model caches emptied
    (keyed by a hash of contents, they cannot be cleared by row; a
    dropped column segment now always leaves its cache), then `Kv::scrub`
    (`Engine::purge_range` past history once older snapshots end, waited
    for up to 60 s; `Store::scrub`: checkpoint, `Wal::roll`, checkpoint,
    dropped segments and spares zeroed, `Pager::zero_free`).  The search
    (`Kv::find_bytes`, `nucleus_storage::scan`) runs at once — R2c's
    core, needed to prove R2b: `FORGET` returns `scrubbed` and `found`
    and the record keeps them.  The audit log's records became version
    2 (the hash over the detail's digest), so an erasure blanks the
    details holding the subject's literal or its rows' texts and the
    chain holds.  Tests: `tests/forget.rs` — a corpus through HNSW, a
    text index, merged memories (the replaced content is an old version
    only) and column segments: every value found before, none after,
    the kept subject's still found; each index scrub and the column
    scrub taken out in turn leave 18 and 50 places found; the audit
    chain intact with blanked details; `tests/store.rs` — a scrub leaves
    no deleted byte in any file in either in-place mode, and the crash
    test at every I/O point with a scrub in its workload.
    *Prior art.*  SQLite's `secure_delete` zeroes each freed page as it
    is freed (a cost on every delete); nucleus zeroes the free list at an
    erasure only — an erasure is rare, a delete is not.  PostgreSQL's
    `pg_switch_wal` and a checkpoint are how a log is made to drop a
    record; its `VACUUM` cannot reach a page a snapshot still needs, as
    `purge_range` waits.  Lucene and Tantivy drop deleted documents only
    in a merge, as the text scrub forces one per segment with deletes;
    FreshDiskANN's consolidation removes deleted nodes from a graph but
    a page keeps its bytes until rewritten — the scrub's zeroing pass.
    The audit chain's detail-by-digest is the Merkle trick of
    certificate transparency's redaction proposals: hash the hash, so a
    leaf can be withheld and the tree still checks.
    *Measured* (release, real files, M-series laptop): a table of
    100 000 rows — text, a text index, an HNSW index — 96 MiB on disk;
    `FORGET` of a subject's 100 rows 1.05 s against 6 ms for the same
    `DELETE`: index scrubs 170 ms, `Kv::scrub` 80–130 ms (≈3 900 free
    pages and 16 MiB of log zeroed; 117 MiB the first time, the load's
    log), the search 700 ms (205 needles over the files and the decoded
    leaves) — R2c's to make faster.  `found` 0 each time; the files
    shrank to 72 MiB.
    *Found and fixed on the way:* a needle of fewer than 8 distinct byte
    values (a sparse vector's window) was found by chance; a value the
    kept rows of the same table also hold (identical vectors from the
    hashing model) was "found" where it rightly is — both now left out
    of the search.  Statement statistics' samples (each statement's last
    text, constants and all, kept in memory for the index advisor; study
    point 8) holding the subject are blanked too (`statements::erase`).
    Not scrubbed: the server's logs (masked texts),
    files outside the directory (backups: R6 ✅, by their keys), session plan caches
    (a session's, gone with it), values under 8 bytes not searched for.
  - **R2c — the verification scan**: every file of the directory
    searched for the forgotten values (strings' bytes, vectors'),
    the data file's compressed values decompressed; its result in the
    erasure record; the planted-corpus measurement.
    **R2c ✅ (2026-11-13).**  The scan and its result in the record came
    with R2b (needed to prove it).  Made fast: a needle is anchored at
    the four-byte window the fewest other needles share, then the least
    like zeros and runs, and the places are filtered by a bitmap of the
    anchors' hashes (2^20 bits) before the map — a filter on a needle's
    first two bytes let a quarter of a database's bytes through (texts
    and hex share their pairs) and a vector needle's first bytes are a
    float's zero low bytes; the anchor shared by every text needle of a
    table ("message …") sent each row to all of them.  `FORGET` on the
    benchmark's 96 MiB went from 1.05 s to 0.41 s, the search from
    700 ms to ~150 ms (79 ms for 100 needles).  Prior art: Rabin–Karp
    and Aho–Corasick (one pass for many patterns); the anchor by rarity
    is what Hyperscan's literal matcher and ripgrep's "rarest byte"
    heuristic do, the hashed bitmap a one-hash Bloom filter.
    *The planted corpus* (`tests/erasure_bench.rs`, docs/benchmarks.md
    "Erasure"): a subject's 100 texts found in 100 places before a
    `DELETE`, 100 after it, 100 after the indexes' upkeep and a vacuum —
    a delete erases nothing a disk image would not show — and 0 after
    `FORGET SUBJECT`; a `FORGET` of a subject whose rows a `DELETE`
    already removed scrubs them too (its table is scrubbed whole).  Not
    built: a superuser function to search for given bytes (the scan is
    `Kv::find_bytes`; a use beyond erasure has not come up), a parallel
    scan (the search is now a fifth of a `FORGET`).
- **R3 — permissioned retrieval, checked** ✅: leak tests over every
  recall path — HNSW, text, hybrid, the merge in `remember` (never
  across subjects or past a policy) — and BM25's statistics per
  partition, so a tenant's scores do not depend on another's rows.
  **R3 ✅ (2026-11-13).**  As built (DOCS §9 "Retrieval under it"):
  `tests/permissioned_retrieval.rs` runs 12 random queries per tenant of
  three (uneven: one holds a fifth of the rows) through every path —
  HNSW k-NN, BM25 ranked by the index, `@@` counted, `rrf`, `convex` —
  as the role under a `current_setting('app.tenant')` policy, and as the
  administrator with the policy written as a filter: the same rows in
  the same order, on a table and on one partitioned by tenant; `recall()`
  never gives another subject's memory, `REMEMBER` of another subject's
  text inserts.  The engine already passed them (§M14.25's barrier, the
  index methods' `keep`).  The side channel was BM25's statistics: a
  partitioned table's summed every partition's, so another tenant's
  rows moved a tenant's scores (the test without the fix: acme's 20 best
  scores 1.302–0.834 against 1.365–0.872 alone, and a different 20th
  row).  Now the binder hands the text index the partitions the
  policies let the statement read, each name behind its
  `partition::guard` under the policies' condition, evaluated when the
  statement runs — the scores are those of the tenant's rows in a table
  of their own, unchanged by 500 rows inserted into another tenant.
  *Prior art.*  Elasticsearch scores by each shard's statistics by
  default (`query_then_fetch`; `dfs_query_then_fetch` gathers them
  globally) — per-shard scores are its default, and a shard per tenant
  isolates them; Lucene's `IndexSearcher` takes its statistics from the
  readers it searches.  Here the partition is the shard, and row
  security picks it.  *Not done:* on a table not
  partitioned by tenant the statistics count hidden rows (counting only
  visible ones would read every row's policy per statement): documented
  — partition by the tenant where scores must stay a tenant's.
- **R4 — fast retrieval** ✅: batched recall, prepared; the latency target
  set and met in the process; a benchmark for it in
  `docs/benchmarks.md`.
  **R4 ✅ (2026-11-13).**  *The target:* `recall(…, 10)` of one subject
  among 100 000 memories of 1 000, in the process, under 1 ms at the
  median with the query's embedding cached (the model's own time is the
  model's: ~12 ms for the local one, R1) — met: p50 0.81 ms, p95
  0.91 ms a question one statement at a time, 0.63 ms a question in
  batches of 16 (`tests/recall_bench.rs`, release, real files, the
  hashing model).  The way there, measured one change at a time:
  - *12 ms → 1.05 ms — the queue's tombstones.*  `SET
    embedding.pending = wait` (which `recall` users set) scans the
    embedding queue before each statement; 100 000 jobs done left
    100 000 tombstones there until the whole vacuum ran.  A round that
    embedded or cleared jobs now vacuums the queue's range after its
    commit (`Kv::vacuum_range`) — RocksDB's queue pattern
    (`DeleteRange`/compaction of a consumed prefix), as the HNSW delta
    already did.
  - *1.05 → 0.8 ms — a stable call once a statement.*  The ranking's
    projection called `embed(query, model)` a row (a model-cache lookup
    and a vector copy each: a third of the time — the text branch
    0.27 ms, the vector scan 0.19 ms, the embeds 0.32 ms).  A
    projection now evaluates, on its first row, every non-volatile call
    whose arguments are constants, parameters or the enclosing rows'
    values, and uses the value for every row (`exec::row_free_evaluated`)
    — PostgreSQL's planner folds stable functions of constants at
    execution start the same way (`estimate_expression_value`; a stable
    function is the same within a statement).
  - *Batched recall:* `SELECT … FROM unnest($2::text[]) WITH ORDINALITY
    AS b(q, n), LATERAL recall('notes', $1, q, 10) r` — one statement,
    one snapshot, one parse for many questions (a model's several
    retrieval queries, RETRO's and kNN-LM's batched lookups).  It needed
    BM25's statistics subquery to read the enclosing row: the query is
    lifted into it and the subquery correlated when it reads one
    (`text::score::bind`; before, `XX000 text_stats of texts`).  Each
    batch's answers equal each question's own (the benchmark asserts
    it).
  - *Prepared:* a statement parsed once and run with parameters
    (`Session::run`) — the drivers' path; binding and planning remain a
    statement's (~0.15 ms): a plan cache keyed by the statement and the
    catalog version would take it off — not built, the target met.
  *Found and fixed on the way — a deleted row back after a crash.*  The
  queue's vacuum after each round made the embedding crash test find a
  done job back after recovery.  A commit's leaf filter (`Collect`)
  drops, without logging, versions under a newer one at or below the
  watermark — here a job's value under its tombstone, the tombstone kept
  as its leaf's last entry.  The vacuum then found the tombstone alone
  and removed it, logged; replay — the filter's drops not logged —
  brought the value back under no tombstone.  The whole `vacuum()` had
  the same flaw, rarely hit (it runs seldom; a crash had to follow
  before a checkpoint).  Now a sweep logs, with its deletes, the ranges
  they cover — a key's versions from the first removed to its last —
  and replay deletes all that is left in them (`Store::write_covering`,
  WAL batch version 2), as a range tombstone does (RocksDB's
  `DeleteRange`, Pebble's range deletions).  `tests/mvcc.rs`'s queue
  crash test (puts, deletes, re-puts, the range vacuumed now and then, a
  whole vacuum every ninth commit) fails at once without it and passes
  100 seeds with it; `storage/tests/store.rs` checks the replay.
- **§M14.14 (time travel) before R5**: history kept by time and `AS
  OF` in SQL — "what the system knew then" needs it.
- **R5 — citations and read records**: versions in `recall`'s results,
  the retrieval log by request id, `AS OF` on memory stores; erasure
  reaching the history and the log.
  **R5 ✅ (2026-11-14).**  *Versions:* `recall` returns `as_of`, the
  commit its statement read (`snapshot_commit()`, the transaction's
  snapshot) — a citation is `(id, as_of)`, and `AS OF COMMIT as_of`
  (§M14.14) reads exactly what was cited however the memory changed
  since; a row's own last commit was not taken (the store keeps no
  per-row commit, and the read's commit names the whole answer, which
  a row's would not).  *The read log:* `SET request_id` records each
  query run under it as an audit record of kind `read` (apart, so a
  rolled-back block keeps it; in the hash chain): request, commit,
  tables, the ids returned, parameters, text; `nc_catalog.reads`.
  Prior art: PostgreSQL's pgAudit logs statements (and, asked,
  parameters) but not what came back; here what came back and the
  commit that reproduces it.  Its cost, measured (`recall_bench` with
  `REQUEST_ID`): a read record is a commit — p50 0.81 → 1.3–1.5 ms a
  `recall` statement, 0.63 → 0.67 ms a question in batches of 16; an
  asynchronous log (records batched off the reader's path) would take
  it off — not built.  *Erasure:* `FORGET`'s purge already ignored the
  history kept (`purge_range` sweeps to the oldest running snapshot),
  so a time within the retention reads no forgotten row; read records
  holding the subject's value are blanked as audit details are.
  `AS OF` on memory stores needed nothing new: a store is a table, and
  `recall` inside `BEGIN … AS OF` gives the answer then.
  `tests/citations.rs`.
- **R6 — per-subject keys (crypto-shredding)**, with §M14.18's backups:
  sealed subject values, the key store outside data and backups,
  destroyed by `FORGET`; backups without index pages.
  **The design (2026-11-14), before building.**  §M14.18's backups do
  not exist yet; R6 builds the slice it needs and leaves the rest
  (incremental chains, object storage, scheduled restore tests) to
  §M14.18.
  - *A backup is the store's entries at one snapshot, less what is
    derived* — `BACKUP TO 'dir'` (an administrator's; online: one
    read-only transaction, writes go on): every key the snapshot sees
    but secondary indexes' (B-tree, HNSW, text, sparse — the "index
    pages" that hold subject values in the clear and that sealing could
    not reach) and `ANALYZE`'s samples.  Restore (`nucleus restore`)
    writes the entries into a new database and rebuilds every index and
    the statistics from the rows.  Physical rather than a logical dump:
    nothing to keep in step with each object kind (roles, policies,
    models, secrets — sealed already —, chunk tables, memory stores),
    the catalog travels as it is.  Parts of 16 MiB, each's SHA-256 in a
    manifest written last (renamed into place): no manifest, no backup.
  - *Sealing*: each entry about a subject's row — the row, its values
    stored out of line, its embedding jobs and next-model vectors, the
    rows of tables that reference it `ON DELETE CASCADE` (what `FORGET`
    deletes with it), transitively — sealed by ChaCha20-Poly1305 under a
    key derived from the keys of all its subjects (SHA-256 of them in
    order: destroying any one makes it unreadable, as forgetting any one
    deletes the row).  An audit record whose detail holds a subject's
    literal keeps its fields in the clear and its detail sealed, so the
    chain still checks after a restore that cannot open it (version 2
    records hash the detail's digest).
  - *Subject keys* live outside the data and its backups (`SubjectKeys`;
    the server's file `<dir>.subject-keys`, mode 0600), named by
    HMAC-SHA256 of the subject's text under a pepper kept there too — a
    backup alone does not show whose rows it holds.  `FORGET SUBJECT`
    destroys the subject's keys after its commit (the file rewritten and
    renamed); a restore then finds those entries unreadable and leaves
    them out, the audit details blanked.
  - *Not covered*: a table stored as columns with subject columns (a
    segment mixes subjects) — refused by `BACKUP` (`0A000`) until its
    rows are backed up as rows; a key file's old blocks on flash (a key
    store in a KMS or HSM is a module's to add).
  **R6 ✅ (2026-11-14).**  Built as designed, with three changes the
  tests forced.  *A key has an id of its own* (16 random bytes) that a
  backup names it by: a subject forgotten and then written again gets a
  new key, and an old backup must still name — and fail to open — the
  old one; keyed by the subject's id alone, the old backup met the new
  key and failed as if tampered with.  *A destroyed key leaves a mark
  with the erasure's commit*: a backup whose snapshot is older (begun
  before the `FORGET`, still running after) gets no new key for the
  subject and leaves its entries out, one taken after gets a new key —
  without the mark the first would have sealed the forgotten rows under
  a live key.  *Audit details are matched as `FORGET` blanks them*: by
  the subject's literal and by the heads of its rows' texts (an `INSERT`
  of an attachment names no subject, only its text).  Rows referencing a
  subject's row follow it unless their key sets itself to NULL or a
  default (then the restore sets them so: `nulled`).  `FORGET` destroys
  the keys of the subject as text in each subject column's type — the
  canonical text a backup names it by.  *Found and fixed on the way:*
  `FORGET` of a table that had statistics failed after its commit
  ("now() outside a statement") — its scrub analyzes again; now under a
  clock.
  *Prior art.*  Crypto-shredding (Mathias Verraes's event-sourcing pattern;
  Akka Persistence's GDPR support; Axon's data protection module):
  per-subject keys, deletion by destroying the key — taken, with the key
  store kept apart and keys named by a keyed hash so a backup does not
  name its subjects.  NIST SP 800-88 counts cryptographic erase as
  sanitization when the key is destroyed and was never stored with the
  data — the reason for the separate file (and for the note that its
  old blocks may remain: a KMS is the stronger store).  Backups as an
  online snapshot without derived structures: CockroachDB's `BACKUP`
  (a timestamp, SSTs of the primary data; indexes are data there, so
  taken whole) and SQLite's online backup API (pages at a snapshot) —
  here entries at a snapshot, secondary indexes left out because their
  pages would hold subjects' values unsealed and are cheap to rebuild.
  PostgreSQL's base backups copy files (indexes included) and so could
  not be forgotten row by row.
  *Measured* (release, real files, M2 Pro; `docs/benchmarks.md`
  "Backups"): 60 MiB — 100 000 notes of 1 000 subjects with a B-tree
  and a text index, 100 000 rows of no one — backed up in 0.46 s to
  19 MiB (unsealed 0.16 s, 15 MiB: sealing ≈ 3 µs and 50 bytes a row),
  restored in 0.80 s with its 3 indexes rebuilt and analyzed (0.71 s
  unsealed); after `FORGET SUBJECT 'user7'` the same backup restored
  99 900 notes.
  `tests/backup.rs`; `subject_keys` and `subject_keyfile` unit tests.
- **R7 — examples and documentation**: a knowledge base with citations,
  per-user memory, erasure on request — in Python, JavaScript and as an
  MCP server an agent uses.
  **R7 ✅ (2026-11-14).**  `docs/memory.md`, the guide: one assistant's
  data end to end — a knowledge base it cites (chunks, vectors and a
  text index fused by `rrf`, `snapshot_commit()`), a citation read back
  `AS OF COMMIT`, a memory per user, who sees what, read records,
  erasure, backups — in SQL, Python, JavaScript and over MCP.
  `examples/python/memory.py` and `examples/js/memory.ts` run it with
  assertions, against the server and in the process
  (`scripts/driver-tests.sh`; the server there now keeps an hour of
  history).  The MCP server gained `cite` (memories by id at a
  citation's commit, with `--memory`) and `forget_subject` (with
  `--allow-erasure`, on its own: an agent erasing a person on request
  is a decision its operator makes apart from giving it memory) — the
  tool R2 left for here.  *Found and fixed by the examples:* a query
  reading a table `AS OF`, prepared as drivers prepare it, failed ("AS
  OF outside a statement": the extended protocol's Describe binds it,
  and binding names the snapshot, without a clock) — `Session::describe`
  runs under one; `tests/time_travel.rs` prepares one now.  An erasure
  assertion in the Python example failed once in about 60 runs and did
  not recur in 40 more; its assertions now say what they saw, so CI
  shows it if it comes back.

### §M14.13 — Database branches ⬜

The copy-on-write store makes a consistent snapshot a root pointer: a
branch is a writable database that starts as a snapshot of another and
shares every unchanged page with it.

- `nucleus branch create <name> [--from <branch> [--at <time>]]`, `list`,
  `drop`, `reset`; SQL-visible (`nc_catalog.branches`), and per connection
  (`ncp` Hello names a branch; drivers and `ncsql` take it).
- Pages are reference-counted across branches (the free list learns
  sharing); a branch costs only what it changes.
- Typical uses: a database per test, per pull request, per experiment;
  discarded in milliseconds.

**Done when:** creating a branch of a large database takes constant time
and space, writes on either side stay invisible to the other, and crash
tests cover branch creation and drop.

### §M14.14 — Time travel ✅

MVCC already keeps old versions for `history`; make them queryable:

- `SELECT … FROM t AS OF TIMESTAMP '…'` (or `AS OF <commit>`), a
  session-wide `SET read_as_of`, and `BEGIN … AS OF` for a read-only
  transaction in the past.
- A retention setting by time (not only by commit count), and
  `nc_catalog` showing how far back each table can go.
- Restoring a dropped or damaged table from the past (`CREATE TABLE … AS
  … AS OF`).

**Done when:** queries at past timestamps return exactly what was visible
then (checked against a log of the workload), within the retention.

**Study (2026-11-13).**  What exists: every commit has a timestamp — a
counter, not a time —, a transaction can read an older snapshot
(`TxnOptions::at`, read-only, `SnapshotTooOld` once its versions are
collected), and `history` keeps versions for that many commits; nothing
maps a time to a commit, and nothing in SQL reaches it.  A session's
catalog cache already serves only snapshots that read the catalog as it
was filled (`watched_as_at`), so a statement at a past snapshot binds
against the past catalog — a table dropped since included.

*Prior art.*  Oracle's flashback query (`AS OF TIMESTAMP` / `AS OF
SCN`, per table reference; the SCN↔time map `SMON_SCN_TIME` sampled
every few seconds, so a time is only as exact as its sample;
`UNDO_RETENTION`; `FLASHBACK TABLE … TO BEFORE DROP`).  SQL:2011
system-versioned tables (`FOR SYSTEM_TIME AS OF`, SQL Server, MariaDB):
history rows in a table of their own, per table, opted into.
CockroachDB's `AS OF SYSTEM TIME` — a whole statement or read-only
transaction, its timestamps (HLC) being times already; retention
`gc.ttlseconds`.  Snowflake's `AT (TIMESTAMP => …)` per table, `UNDROP`,
`DATA_RETENTION_TIME_IN_DAYS`.  Datomic's `as-of` database value.
PostgreSQL had time travel until 6.2 and keeps only
`track_commit_timestamp` (a commit time per transaction id).

*Choices.*  A commit's time is recorded exactly, with the commit (a
store entry `00 05 | ts → µs` in its batch — PostgreSQL's commit
timestamps, not Oracle's samples: "exactly what was visible then" needs
the exact boundary), times made monotonic (a clock stepping back takes
the last time), only while a retention by time is set (no cost
otherwise).  A time `t` reads the snapshot of the last commit at or
before `t`.  The retention is a time (`EngineOptions::history_time`;
the server's `--history`, a database setting later): the watermark is
the last commit at or before now − retention, found from the recorded
times (a few seeks, cached a second — older is safe: it keeps more).
SQL: a whole statement or a read-only transaction at a time (CockroachDB's
model — one snapshot, the past catalog) and a table reference at a time
(Oracle's and Snowflake's — a second snapshot for that scan, the table's
past definition), the latter what restores need (`CREATE TABLE … AS
SELECT … FROM t AS OF …`, a dropped table included).

*Steps.*
- **TT1 — the engine** ✅: commit times recorded, retention by time, a time
  to a snapshot (`Engine::snapshot_at`), how far back (`Engine::reach`);
  the KV API; crash and model tests.
  **TT1 ✅ (2026-11-13).**  `EngineOptions::history_time` /
  `nucleus_kv::Options::history_time`; the records `00 05 ‖ ts → µs` in
  each commit's batch (monotonic: the last time if the clock stepped
  back); `snapshot_at` a binary search by seeks; the floor found again
  at most once a second, outside every lock (an older floor keeps more);
  a whole vacuum trims the records below the watermark.
  `tests/time.rs`: 300 random times a seed against a logged workload of
  120 commits (several within one microsecond: a time reads the last of
  them), the retention (a minute) kept and the rest collected, the
  records surviving a crash at every I/O point, none written without a
  retention.
- **TT2 — SQL at a time, whole** ✅: `BEGIN READ ONLY AS OF TIMESTAMP …`,
  `SET TRANSACTION AS OF …`, `SET read_as_of`; errors past the retention
  (`SnapshotTooOld` → its SQLSTATE) and in the future.
  **TT2 ✅ (2026-11-13).**  `TxnMode::as_of` (`AS OF TIMESTAMP | SYSTEM
  TIME | COMMIT expr`), the session's `read_as_of`; the expression
  evaluated as a one-row query in a transaction of its own when the
  block's transaction begins; `22023` for the future or NULL, `72000`
  past the retention.  Nothing else was needed: the catalog cache
  already refuses a snapshot that reads the catalog otherwise, so a
  past statement binds against the past catalog — a column added since
  absent, a dropped table there (`tests/time_travel.rs`); the text index
  and HNSW answered for their past in the small case — TT5 checks them
  at scale.
- **TT3 — a table at a time**: `FROM t AS OF TIMESTAMP x` (and `AS OF
  COMMIT n`): its own snapshot, its past definition; writes in the same
  statement read the present — restores by `CREATE TABLE … AS` and
  `INSERT … SELECT`.
  **TT3 ✅ (2026-11-13).**  `FROM t AS OF TIMESTAMP | SYSTEM TIME |
  COMMIT expr` and SQL:2011's `FOR SYSTEM_TIME AS OF expr` (SQL Server,
  MariaDB, CockroachDB's `AS OF SYSTEM TIME` per statement; Oracle's
  `AS OF TIMESTAMP` per table — taken: per table, so a statement
  compares and restores).  The binder binds the table against the past
  snapshot's catalog in a sub-binder; `Plan::Past { at, plan }` runs in
  a read-only transaction of its own at `at` and is materialized like a
  CTE — one past read per table, the present's plan untouched.  Needed
  for the restore: `CREATE TABLE … AS query [WITH [NO] DATA]` (it was
  missing), and `WITH` reserved, as PostgreSQL has it, since an alias
  had swallowed `WITH NO DATA`.  Privileges are the present's (a table
  that still exists needs `SELECT` now).  `tests/time_travel.rs`
  (a table's past beside its present, a dropped table restored),
  `slt/64_create_table_as.slt`.
- **TT4 — retention and reach in SQL**: the server's `--history`,
  `nc_catalog.history` (the oldest time and commit reachable),
  `nucleus_kv` `Options::history_time`.
  **TT4 ✅ (2026-11-13).**  `nucleus server --history 7d` (durations as
  `statement_timeout` takes them); `nc_catalog.history` — `retention`,
  `oldest_time`, `oldest_commit`, `newest_commit` — from `Kv::reach`,
  `Kv::history_time`, `Kv::newest`.  The oldest time read back through
  `AS OF TIMESTAMP (SELECT oldest_time …)` and `AS OF COMMIT`, a second
  before it `72000` (`tests/time_travel.rs`).  Prior art: CockroachDB's
  `gc.ttlseconds` per zone and `crdb_internal` for what is reachable,
  Oracle's `UNDO_RETENTION` and `V$UNDOSTAT`; one retention for the
  database here — a table's own is for when a tenant asks.
- **TT5 — the check**: a workload logged with its commit times, queries
  at random past times against the log's model, through every index
  kind (HNSW's and the text index's caches at a past snapshot); the
  benchmark of a commit's added cost.
  **TT5 ✅ (2026-11-13).**  `tests/time_travel.rs`
  `a_logged_workload_reads_back_at_random_times_through_every_index`:
  80 rounds of inserts, updates and deletes (one transaction each, the
  clock moving 1 µs – 30 s between), a B-tree index made halfway (the
  past before it has none), HNSW and text upkeep every 10 rounds, a
  vacuum every 15, a reopen at 55; then 150 queries at random times,
  alternately a whole block `AS OF` and a table `AS OF`, through the
  B-tree index, the text index, HNSW (only rows of then, as many as
  asked, recall ≥ 95% of the exact nearest) and a column
  table, against the log's model; 40 seeds clean.  A mutation (the
  snapshot read 20 s late) fails it at the first query.
  The cost of recording times, measured (`crates/kv/examples/bench.rs`
  with `NUCLEUS_HISTORY`, M2 Pro, five alternating runs): one-row
  commits from 8 threads 12.1k/s with a time record per commit against
  16.6k/s without (−28%), at the SQL level 10.9k against 12.5k (−13%) —
  the record rewrote a leaf of its own in the copy-on-write tree each
  commit (`insert_sorted_at`'s samples doubled).  Taken: the commit
  times as *deferred puts* (`Store::write_deferring`, WAL batch version
  3): logged with the commit, kept in memory, into the tree 4096 at a
  time or at a checkpoint's freeze, replayed into memory — a memtable
  for one key range (RocksDB's and LevelDB's memtable; LSM trees'
  premise that a log makes a write durable and the sorted structure
  can follow in bulk).  After: 15.2k/s against 15.9k/s (within the
  spread), the SQL level 11.7k against 11.3k.  Not taken: sampling
  the times (Oracle's `SMON_SCN_TIME`, every 3 s) — a time would no
  longer name its commit exactly; CockroachDB's HLC timestamps as the
  versions themselves — the versions here are counters, and the map
  costs nothing now.  `snapshot_at`'s search bounded by the visible
  commit, not the timestamp space: 20 µs → 5.5 µs.

### §M14.15 — JSON ◐ (study written, J1–J3, J4a–b ✅, 2026-11-14)

- A `JSON` type (validated, stored in a compact binary form), operators
  `->`, `->>`, `#>`, `@>`, `?`, functions to build, extract, test and
  aggregate (`json_build_object`, `json_agg`, `jsonb_path`-style paths).
- Indexes on JSON paths (expression indexes), and a containment index.
- The bridge to the document model (§M15): documents are JSON values.

**Done when:** an slt script checked against PostgreSQL's `jsonb` covers
the operators and functions.

**The study (2026-11-14).**  Why it comes first among what is left: what
an AI application stores is JSON more than anything else — a model's
structured output, tool calls and their results, an API's answers,
metadata beside a chunk or a memory — and today nucleus has nowhere to
put it but `text`, which it can neither query nor index.

*What exists to build on* (surveyed in the code):

- **Registered types** (`ext::Type`): a value is its type and its
  canonical bytes (`Value::Ext`, equal values with equal bytes), with
  equality, hashing, a total order and an order-preserving key encoding
  each optional — how `uuid` and `vector` are built.  A JSON value fits:
  bytes in, nothing new in rows, pages, the log, backups (§M14.24 R6) or
  time travel.
- **Operators outside the grammar**: the lexer already reads any run of
  operator characters (`<->`, `@>`, `?|`) as a user operator, resolved
  by the operands' types — so `@@` can mean a text match on `text` and a
  path predicate on `json`, as in PostgreSQL.  The drivers number
  parameters `$1`, so `?` is free (JDBC's clash does not arise).
- **Table functions** (`generate_series`, `unnest`, `WITH ORDINALITY`):
  `json_each`, `json_array_elements` and `JSON_TABLE` plug in there.
- **Indexes**: B-trees over columns, a stored generated column
  (`GENERATED ALWAYS AS (doc->>'kind') STORED`, §M14.22 E4a) indexable
  like any other; no expression indexes yet (J4a added them).  The text index (§M14.21)
  is an inverted index — terms to block-compressed postings, kept in the
  writer's transaction, crash-safe — the machinery a containment index
  needs.
- **No JSON in the core**: the modules that talk to services (MCP,
  embeddings, OIDC) use `serde_json`; the core is dependency-free
  (principle), so the parser is nucleus's own.  The nesting limit
  (`MAX_NESTING`, §M14.29 S2) bounds the parser's depth too.

*What others do:*

- **PostgreSQL** has two types: `json` keeps the text as written
  (whitespace, key order, duplicate keys) and re-parses it on every
  access; `jsonb` (9.4, 2014) is binary — a container header, a 4-byte
  `JEntry` per element (type and length, every 32nd an offset, so
  TOAST's compression still finds repeats), an object's keys sorted by
  length then bytes and found by binary search, duplicates resolved
  last-wins, numbers as `numeric` (exact; `1.50` prints `1.50`).  Its
  GIN index has two operator classes: `jsonb_ops` (keys and values as
  separate terms: `@>`, `?`, `?|`, `?&`) and `jsonb_path_ops` (one hash
  of path and value per leaf — smaller and faster, `@>` and path
  predicates only).  SQL/JSON paths in 12; `IS JSON` and the standard
  constructors in 16; `JSON_TABLE`, `JSON_VALUE`, `JSON_QUERY`,
  `JSON_EXISTS` in 17.
- **SQLite** 3.45 (2024) added JSONB: a BLOB of nodes, each a header
  byte (type and a size width) and a payload — no offset table, so a key
  is found by a linear walk, but nothing is parsed; the text functions
  accept either form.
- **MySQL** (5.7): binary JSON with sorted keys and offset tables for
  binary search, small and large variants (16- or 32-bit offsets); 8.0
  updates a value in place where it can (partial updates in the log).
- **MongoDB's BSON**: length-prefixed fields in insertion order, typed
  (int32, int64, double, decimal128, dates), found by a linear walk;
  its indexes reach into arrays (multikey).
- **The Variant encoding** (Spark 4 / Delta / Iceberg / Parquet, 2024):
  a metadata dictionary of a column's keys, values that refer to keys
  by id, objects with sorted field ids and offsets whose width is chosen
  per value (1–4 bytes); and *shredding* — frequent paths stored as
  typed columns beside the rest.  Snowflake's `VARIANT` shreds
  automatically; ClickHouse's `JSON` type (24.8) keeps each path as its
  own subcolumn up to a limit.  DuckDB stores JSON as text, parsed by
  yyjson, and turns it into typed `STRUCT`s when it reads.
- **Parsing**: simdjson (Langdale and Lemire, 2019) parses gigabytes a
  second by finding structural characters with SIMD first; yyjson
  without SIMD is close.  The cost that matters in a database is less
  the parse than not parsing again: reading a field from a binary form
  costs nothing near a parse.
- **The standards**: RFC 8259 (JSON); RFC 7493, I-JSON (no duplicate
  names, numbers within IEEE doubles); RFC 8785, JCS (one canonical
  text: sorted keys, numbers as ECMAScript prints them); the SQL/JSON
  path language (ISO/IEC 9075-2:2016, `lax` and `strict`) and RFC 9535
  JSONPath (2024), which differ; JSON Schema 2020-12, which the model
  providers' structured outputs take (a subset).

*What nucleus takes, and why:*

1. **One type, `json`** (`jsonb` accepted as its name): the binary form
   only.  A text kept as written is a `text` column; PostgreSQL's
   two types are a history, not a need.  Keys sorted, duplicates
   last-wins (PostgreSQL's choice — the one that never refuses what
   another system wrote).
2. **Canonical bytes** — equal documents, equal bytes — so equality,
   hashing, `DISTINCT`, `GROUP BY` and unique indexes cost a byte
   comparison and `Value::Ext`'s law holds.  The one visible
   difference from PostgreSQL, recorded: a number is stored as its
   value, so `1.50` reads back `1.5` and `1e2` `100` (as JCS and every
   language's JSON library, but unlike `jsonb`, which keeps the scale).
   Numbers stay exact (a decimal of any size, like `numeric`): an id of
   20 digits is not rounded.
3. **The form** (Variant's and MySQL's ideas, measured before fixed): a
   header byte per node (type, width); an object's keys sorted, with a
   table of offsets whose width (1, 2 or 4 bytes) is chosen per object —
   a key found by binary search, a value reached without reading its
   siblings; an array's offsets give `a[i]` in constant time; strings
   raw UTF-8 with a length, so `->>` hands out a slice with nothing to
   unescape.  No column-wide key dictionary (Variant's): a row's value
   must stand alone (a row is read, logged, backed up and erased by
   itself).
4. **A parser of its own**: strict RFC 8259 (no comments, no trailing
   commas, no `NaN`), UTF-8 checked, surrogate pairs joined and lone
   ones refused, `\u0000` refused (`text` holds none), depth within
   `MAX_NESTING`; scanning a string's bytes a word at a time for `"`,
   `\` and controls (simdjson's idea, without its two passes), measured
   against `serde_json` and PostgreSQL.
5. **PostgreSQL's operators and functions** (`->`, `->>`, `#>`, `#>>`,
   `@>`, `<@`, `?`, `?|`, `?&`, `||`, `-`, `#-`, `@?`, `@@`; the
   `jsonb_*` functions and aggregates under `json_*` names, the `jsonb_`
   names accepted) — what users and models know — **and the
   standard's** (`JSON_VALUE`, `JSON_QUERY`, `JSON_EXISTS`,
   `JSON_TABLE`, `IS JSON`, `JSON_OBJECT`, `JSON_ARRAY`, `JSON_ARRAYAGG`,
   `JSON_OBJECTAGG`), both checked against PostgreSQL 18.
6. **Paths are the SQL/JSON path language** (what PostgreSQL, Oracle and
   the standard share), not RFC 9535; compiled once per statement.
7. **Indexes**: expression indexes (`CREATE INDEX ON t ((doc->>'kind'))`
   — for every type, not JSON only; a stored generated column is the
   path until then), and a **containment index** `USING json` on the
   text index's postings, transactional and crash-safe like it: a term
   per leaf (a hash of path and value, `jsonb_path_ops`' choice) and
   one per key path (so `?` is served too, which `jsonb_path_ops`
   cannot), for `@>`, `?`, `?|`, `?&`, `@?` and `JSON_EXISTS` with
   equalities.
8. **For models**: `json_matches_schema(doc, schema)` — the JSON Schema
   subset the providers' structured outputs use (`type`, `properties`,
   `required`, `additionalProperties`, `enum`, `const`, `items`,
   `anyOf`, `$defs` / `$ref`, bounds, `pattern` by the core's regex) —
   so a `CHECK` refuses an output that does not fit; JSON values as
   JSON in MCP results (not strings inside strings); driver values as
   the language's own (Python `dict` / `list`, JavaScript objects, Rust
   and C the text).
9. **Not taken now**: shredding in columnar tables (Variant / ClickHouse
   — later, measured on a columnar workload); the text-preserving
   `json` type; in-place partial updates (MySQL's: rows are rewritten
   whole here anyway).

*Steps (agreed 2026-11-14):*

- **J1 — the type**: the parser, the binary form, output (compact, and
  `json_pretty`), casts to and from `text` and the scalar types, equality,
  hashing, PostgreSQL's order (object > array > boolean > number >
  string > null) and a key encoding, so it can be a key; the `json`
  wire type and the drivers' native values; a fuzz target (text → form
  → text, a round trip as the law).  Measured: parse and output
  throughput against `serde_json` and PostgreSQL, size against `jsonb`.

  **J1 ✅ (2026-11-14).**  Built as studied (`crates/sql/src/json/`:
  `form`, `parse`, `text`, `key`); the form, byte by byte, in DOCS.md
  §8.31.  *Taken:* a header byte per node and offset tables whose width
  is chosen per container (Variant's and MySQL's) — end offsets, so an
  element's span is two reads; integers in the fewest bytes, other
  numbers as packed decimal digits and an exponent, made canonical
  (JCS's idea, exact like `numeric`, `numeric`'s bounds); strings
  scanned eight bytes at a time for `"`, `\` and controls (simdjson's
  idea in a word, without SIMD or its two passes), in the parser and
  the output; PostgreSQL's order for the key encoding (null < string <
  number < boolean < array < object, containers by count first).  *Not
  taken:* SIMD (the core has none yet; the word scan was enough to pass
  `serde_json`), a key dictionary (Variant's: a value stands alone).
  The wire carries the text (a `wire_output` / `wire_input` hook on
  registered types: drivers parse JSON natively), pg-compat `jsonb`
  (oid 3802, binary version 1) and `json` (114).
  *Measured* (M2 Pro; `bench/json`, the median of 15): parse, output
  and one field of the top object against `serde_json`'s `Value` —
  `canada.json` (numbers) 484–568 MB/s against 491–499 across runs,
  `citm_catalog.json` 1011 against 660, `twitter.json` 716 against
  480; output 448 / 1606 / 1178 MB/s against serde's 1004 / 4235 / 2092
  (serde prints compact text from values it holds as doubles; nucleus
  `jsonb`'s layout, with spaces, from exact decimals); a field 0.024 µs
  in place against a parse's 1.3–4.6 ms.  Two changes made the numbers:
  output as bytes with strings as byte slices and digits unpacked by
  pairs (2–3.5× serde's time before, about 2× now), and numbers packed
  straight from the text's digits (canada's parse 334 → ~520 MB/s; an
  attempt through a 64-bit mantissa was slower).  Against PostgreSQL 18
  (`bench/json/pg.py`, its `jsonb` in Docker on the same machine, its
  defaults; nucleus a server; both timed as one statement): parsing
  50 000 tweets (246 MB of text) 0.43 s against 1.63, 50 000
  `citm_catalog` performances (103 MB) 0.09 against 1.00, canada's
  features ×4 (8.8 MB) 0.026 against 0.072; output 0.145 / 0.048 /
  0.029 s against 1.12 / 0.44 / 0.044; a whole file stored 1667 / 407 /
  431 KB against `jsonb`'s 2599 / 739 / 497 (text 2198 / 1687 / 617).
  *Differences from PostgreSQL, recorded:* `1.50` reads back `1.5`
  (the value, not the scale); keys sorted by bytes, not length first
  (J2 changed this: below); the top-level `[] < null` quirk of `jsonb`
  is not kept.  *Fuzzing*
  (`fuzz/fuzz_targets/json.rs`: text laws, and malformed bytes read
  safely) found two before the merge: a corrupt decimal with a huge
  exponent printing billions of zeros (now bounded at read) and an
  exponent overflow in the writer (checked).  *Found and fixed on the
  way:* a table dropped after a bulk `CREATE TABLE AS` came back empty
  — a commit's leaf filter dropped a tombstone over a large commit's
  unmade intent (53a0c36, the Changelog's entry).
- **J2 — operators and functions**: the access and containment
  operators, building (`json_build_object`, `to_json`, `row_to_json`,
  `json_agg`, `json_object_agg`), changing (`json_set`, `json_insert`,
  `||`, `-`, `#-`, `json_strip_nulls`), reading (`json_typeof`,
  `json_array_length`, `json_each`, `json_array_elements`,
  `json_object_keys`); an slt checked against PostgreSQL 18.

  **J2 ✅ (2026-11-14).**  `json::ops` (on the binary form: a node's
  bytes are a value's, so a change re-encodes only the containers it
  passes through) and `json::funcs`.  *Operators:* `->`, `->>`, `#>`,
  `#>>`, `@>`, `<@`, `?`, `?|`, `?&` as registered operators; `||` and
  `-` — the grammar's own — through the type's arithmetic, which may
  now take a right side of another type (`Arithmetic::other_operand`:
  `json - text`, `- int`, `- text[]`); `#-`.  An untyped right operand
  (`doc -> 'k'`, `doc -> $1`) takes `text` where the operator has a
  `text` form, else its only form's type (`Registry::operand_for` —
  PostgreSQL's preference of `unknown` for the string category, for
  any registered operator).  *Functions* under `json_` and `jsonb_`
  names: `json_typeof`, `json_array_length`, `json_pretty`,
  `json_strip_nulls`, `to_json`, `json_build_object`,
  `json_build_array`, `json_object`, `json_set`, `json_insert`,
  `json_extract_path[_text]`; in `FROM` `json_each[_text]`,
  `json_array_elements[_text]` (a table function may now name its
  single column: `value`, as PostgreSQL's out parameter), and
  `json_object_keys`; aggregates `json_agg`, `json_object_agg`.
  *Checked against PostgreSQL 18:* `slt/65_json.slt`, 108 records,
  with a new tool, `scripts/slt-pg.py` (`check` runs a script's records
  against a real PostgreSQL in Docker and reports what differs; `fill`
  writes its results in; records after `# nucleus-only:` are left out)
  — the claim "checked against PostgreSQL" the test kit's scripts make
  is now a command.  Its first runs showed every object's text in
  another key order: `jsonb` sorts keys shorter first, then by bytes.
  *Changed:* the form now sorts keys so (`form::key_order`, and the key
  encoding a key's length first), so text, `ORDER BY` and the scripts
  agree with PostgreSQL's; and `json_pretty` prints an empty container
  on two lines, as `jsonb_pretty`.  A value stored by J1 (the same
  day) still reads, but is not canonical in the new order.  *Kept
  different, recorded* (`# nucleus-only:` records): the scale (`2.50`
  is `2.5`), and `jsonb`'s raw-scalar quirk (`'1'::jsonb -> 0` is `1`
  there, `NULL` here).  *Not there:* arrays of `json` (`json[]`: a
  registered type has no array type yet), `row_to_json` and `to_json`
  of a row (nucleus has no row values yet: `json_build_object` names
  the columns), set-returning functions in the select list (in `FROM`,
  as everywhere in nucleus), SQL/JSON paths (J3).  *Fuzzing:* the `json` target checks the operators' laws too —
  every result canonical, a value contains itself and itself less a
  key, a key deleted is gone and set is there (1.4 M runs clean).
  *Measured* against PostgreSQL 18 (`bench/json/pg.py`, the field sums
  asserted equal): a nested field over 50 000 tweets, `doc -> 'user'
  ->> 'screen_name'`, 0.082 s against 0.361; a performance's `->>
  'start'` 0.010 against 0.038; a feature's name 0.004 against 0.012 —
  read in place, where `jsonb` finds a key by binary search too but
  detoasts the value first.
- **J3 — SQL/JSON**: the path language (`lax` / `strict`, filters,
  methods), `json_path_query` and kin, `@?` / `@@`, `JSON_VALUE`,
  `JSON_QUERY`, `JSON_EXISTS`, `JSON_TABLE`, `IS JSON`, the standard
  constructors; checked against PostgreSQL 18.  In three steps: **J3a**
  the path language, its type and functions; **J3b** `JSON_VALUE`,
  `JSON_QUERY`, `JSON_EXISTS`, `IS JSON` and the constructors; **J3c**
  `JSON_TABLE`.

  **J3a ✅ (2026-11-14).**  `json::path`: `parse` (PostgreSQL's
  `jsonpath` grammar — `jsonpath_gram.y`, `jsonpath_scan.l` — by
  recursive descent with one token of look-ahead; what PostgreSQL
  refuses while reading a path is refused here with its SQLSTATE),
  `print` (its normalized text, from `printJsonPathItem`'s rules:
  operator priorities, quoted keys, `numeric`'s numbers), `eval`
  (`jsonpath_exec.c` step for step: lax unwrapping and wrapping,
  ignored structural errors and their being ignored under `.**`, the
  early stop of `exists` and of `@?` in lax mode, three-valued
  predicates, strict mode's collecting every item to see that none
  fails; soft errors — PostgreSQL's `RETURN_ERROR`, which `silent` and
  predicates swallow — apart from hard ones), `datetime` (the ISO forms
  `.datetime()` tries, `to_timestamp`'s standard-mode template reader
  for the common patterns, the casts and comparisons across date, time,
  timetz, timestamp, timestamptz and the ones that need a zone).  Items
  are ranges of the document (or a variable, or a value the path made,
  shared by `Rc`) read in place — no item is copied until a function
  returns it.  A `jsonpath` type (its normalized text; PostgreSQL's oid
  4072 in pg-compat, binary with its version byte); `json_path_exists`,
  `_match`, `_query_array`, `_query_first`, `json_path_query` in
  `FROM`, each with a `jsonb_` name and a `_tz` form; `@?`, `@@`.  A
  path value is parsed once per thread while among the last 16 used.
  *Checked against PostgreSQL 18:* `slt/66_json_path.slt`, 221 records
  filled from PostgreSQL by `scripts/slt-pg.py fill` and passing on
  nucleus with two differences, recorded: numbers keep no scale (`2 *
  3.5` is `7`, PostgreSQL's `7.0`, as J1 decided), and `.keyvalue()`'s
  `id` below the outermost value is a byte offset in nucleus's form, not
  in PostgreSQL's.  One deliberate departure: PostgreSQL prints
  `(@.a + 1).abs()` inside a filter without its parentheses (`@."a" + 1.abs()`,
  which reads back as another path); nucleus keeps them.  *Not there:*
  PostgreSQL's less common template patterns (ordinal suffixes, `FM`,
  `FX`, ISO weeks, Roman months) and zone names beyond `Z`, `UTC`,
  `GMT`, `UT`; numbers past 38 digits in arithmetic and methods
  (`22003`, `numeric`'s bound in nucleus).  *Fuzzing:* the `json` target
  reads paths too — the normalized text reads back as itself, items are
  canonical, nothing panics (2.0 M runs clean).  *Measured* against
  PostgreSQL 18 (`bench/json/pg.py`, a `path` column; the counts
  asserted equal): `doc @? '$.entities.hashtags[*] ? (@.text starts
  with "a")'` over 50 000 tweets 0.062 s against 0.356 (5.7×); a
  performance's seat areas above an id 0.018 against 0.051 (2.8×); a
  feature's points west of 70°W 0.005 against 0.012 — the items read
  in place, the path parsed once, `@?`'s lax evaluation stopping at the
  first match as PostgreSQL's does.

  **J3b ✅ (2026-11-14).**  `json::sqljson` and the parser's
  `sql_json`: `JSON_EXISTS`, `JSON_VALUE`, `JSON_QUERY` (`PASSING`,
  `RETURNING`, wrappers, quotes, `ON EMPTY` / `ON ERROR`), `IS JSON`,
  `JSON_OBJECT`, `JSON_ARRAY` (of values or of a query),
  `JSON_OBJECTAGG`, `JSON_ARRAYAGG`, `JSON`, `JSON_SCALAR`,
  `JSON_SERIALIZE`.  *How* (PostgreSQL's way, `gram.y`'s
  `func_expr_common_subexpr` and `parse_expr.c`'s `transformJson*`, read
  for the semantics): the keywords are the parser's alone — each form is
  desugared into a call of an ordinary function whose options are
  constant arguments, so the binder, the planner and the executor need
  nothing new.  `RETURNING t` needs the type at bind time, and a
  function's result type is its resolver's: the form carries a typed
  `NULL` (the *witness*, `CAST(NULL AS t)`) the resolver reads `t` off,
  and `ScalarFunction::rewrite`, which sees the bound arguments, reads
  the modifier off the bound witness (`varchar(n)` → `substr`,
  `char(n)` → `bpchar`, `numeric(p, s)` → `numeric`, a registered
  type's `FitFn`) and gives the call specialized to it — PostgreSQL's
  `JsonExpr` keeps a coercion expression the executor evaluates in an
  error-safe way; here the specialized function converts by `t`'s input
  itself (lengths checked, not cut), so a conversion's error is the `ON
  ERROR` clause's.  `JSON_ARRAY(query)` is `(SELECT JSON_ARRAYAGG(x)
  FROM (query) q(x))`, as PostgreSQL reads it.  `IS JSON … WITH UNIQUE
  KEYS` checks keys while parsing (`json::parse::parse_checked`), after
  escapes are read.  *Checked against PostgreSQL 18:*
  `slt/67_json_sql.slt`, 207 records filled by `scripts/slt-pg.py`,
  plus 12 nucleus-only ones for the differences: `json` is one canonical
  type, so a constructor's text is `jsonb`'s (sorted keys, the last of
  equal keys, numbers without their scale) where PostgreSQL's `json`
  keeps its own layout; `JSON_SERIALIZE` of `jsonb` in PostgreSQL 18
  gives the binary form read as text (a bug nucleus does not copy); a
  `PASSING` value travels as `to_json` makes it, so a date is a string
  (PostgreSQL passes a datetime item).  *Found on the way:* J3a's
  registration leaked its function names (`String::leak`), which every
  fuzz target that builds a registry reported (LeakSanitizer, CI of
  ed92a23); now static.  And the `json` fuzz target found a path of 900
  signs overflowing the parser's stack, and a chain of `&&` or `+` —
  read in a loop but nesting the tree to the left — could overflow the
  printer, the evaluator and the drop: a path's tree nests at most 128
  levels now (`54001`; PostgreSQL's limit is its stack's), the parser
  allowed twice as deep so that every normalized text reads back
  (`-(-$)` for `- -$`); checked on a 2 MiB thread in a debug build.  *Not there:* `FORMAT JSON ENCODING` other than
  UTF-8 (read and ignored), `RETURNING` a type whose cast is a taught
  function (`0A000`),
  a `DEFAULT` that is a subquery (PostgreSQL refuses it too),
  `JSON_OBJECTAGG` / `JSON_ARRAYAGG` with `FILTER` or `OVER`.
  *Measured* against PostgreSQL 18 (`bench/json/pg.py`, a `value`
  column, the sums asserted equal): `sum(JSON_VALUE(doc,
  '$.user.followers_count' RETURNING bigint))` over 50 000 tweets 0.077 s
  against 0.362 (4.7×); a performance's start 0.012 against 0.040
  (3.3×); a feature's number of rings 0.004 against 0.014 (3.5×) — the
  scalar read in place and converted from its text once, the path
  parsed once per thread.

  **J3c ✅ (2026-11-14).**  `JSON_TABLE` in `FROM`: PASSING, `FOR
  ORDINALITY`, a typed column (as `JSON_VALUE` reads it, or `JSON_QUERY`
  when `FORMAT JSON`, wrapped, quoted or `json`-typed), `EXISTS`
  columns, each column's `ON EMPTY` / `ON ERROR`, the table's `ERROR |
  EMPTY ON ERROR`, `NESTED PATH` to any depth.  *How:* as J3b, the parser
  alone — `JSON_TABLE` is desugared into a lateral subquery: the row
  path's items from a table function `__json_table_rows` (`item`, `ord`),
  each column a call of J3b's functions on the item, a `NESTED PATH` a
  lateral `LEFT JOIN` of its own level, sibling nests `UNION ALL`ed with
  the others' columns `NULL` — PostgreSQL's default plan (`parent OUTER
  child`, `sibling UNION sibling`; its executor walks a plan tree of
  `JsonTablePlan` nodes in `nodeTableFuncscan.c`, where here the join and
  union operators the engine already has do it).  No new operator, and
  the optimizer sees an ordinary lateral join.  *Checked against
  PostgreSQL 18:* `slt/68_json_table.slt`, 67 records filled by
  `scripts/slt-pg.py` (the rows' order too, where a query has no `ORDER
  BY`), and one difference recorded: a `DEFAULT` may read the row's
  columns (PostgreSQL refuses column references there).  *Not there:*
  the standard's `PLAN` clause (PostgreSQL 17 and 18 have none either).
  *Measurement pending:* `bench/json/pg.py` has the `table` column
  (count and sum asserted equal to PostgreSQL 18's); the run waits for
  the Docker VM, which stalled when the disk filled.
- **J4 — indexes**: expression indexes for every type, the containment
  index, the planner choosing them (with statistics: a path's
  selectivity); measured against PostgreSQL's GIN (`jsonb_path_ops`)
  on the same documents.  In three steps:
  - **J4a — expression indexes** ✅ (2026-11-14): `CREATE [UNIQUE]
    INDEX … ((expr), lower(c), …)`, any type keys encode.  *Prior art:*
    PostgreSQL keeps an index's expressions in its record (`indexprs`)
    and matches a condition's side to them as planned trees; MySQL 8
    makes a functional key part a hidden virtual generated column.
    Taken: PostgreSQL's — the expression's SQL text, type and columns
    in the index record (`catalog::IndexExpr`, table record version
    19), bound once per writer (`dml::IndexKeys`, as a taught
    ordering's mapping is) and, for the optimizer, once a statement
    with the built-in functions (`Stats::key_parts`), matched
    structurally (`optimizer::key_bound`; a function's `RETURNING`
    witness is an argument, so `JSON_VALUE … RETURNING int` and `…
    RETURNING text` are different expressions).  Not taken: MySQL's
    hidden column — every reader of a table's columns (`SELECT *`,
    `COPY`, the drivers' descriptions, the catalogs) would have to skip
    it.  Immutability as PostgreSQL's (`42P17`), and a conversion of
    or to `timestamptz` refused as it depends on the time zone.  An
    expression part is no uniqueness of its columns: the statistics',
    the optimizer's and foreign keys' inferences of uniqueness read
    `Index::key_columns`, `None` for it.  The selectivity of a
    condition on it comes from the statistics' sample, which evaluates
    it.  `slt/69_expression_indexes.slt`.  Measured with J4b in J4c.
  - **J4b — the containment index** ✅ (2026-11-14): `USING json`, as
    the study planned (point 7), for `@>`, `?`, `?|`, `?&`.  *Prior
    art:* PostgreSQL's GIN has two operator classes — `jsonb_ops`, a
    key or a value an entry (serves `?`, larger, a common key's
    postings long), and `jsonb_path_ops`, a 32-bit hash of a leaf's
    path and value an entry (smaller, faster `@>`, no `?`), both lossy
    and rechecked.  Taken: `jsonb_path_ops`' leaf terms, with 64-bit
    hashes (fewer collisions to recheck), and a key path's term from
    `jsonb_ops`' idea so `?` is served — but a query looks a key path
    up only above an empty object or array, since a leaf below a key
    implies it: measured on 200,000 events, `@> '{"user": {"name":
    "u42"}}'` (40 rows) 1.96 ms with the key paths ANDed, 0.12 ms
    without.  The postings are the text index's storage (§M14.21 T4),
    so writes are transactional and crash-safe with nothing new;
    documents weighted, each term 1, so no positions are kept.  The
    recheck: `IndexMethod::exact`, false, keeps the condition in the
    scan's filter.  *Measured* (nucleus alone — PostgreSQL waits for
    the Docker VM —, `bench/json/index.py --no-pg`, 200,000 rows, the
    median of five, a full scan against the index): `doc->>'id' =`
    (B-tree expression) 8 ms → 0.07 ms; `(doc->>'score')::int BETWEEN`
    (600 rows) 16 → 0.90 ms; `@>` a user (40 rows) 33 → 0.12 ms; `@>`
    a kind and a tag (2,000 rows) 32 → 2.3 ms; `? 'rare'` (200 rows) 6
    → 0.28 ms.  Builds: 0.11 s, 0.09 s, the containment index 0.84 s.
    `slt/70_json_index.slt`.
  - **J4c — measured** against PostgreSQL's GIN (`jsonb_path_ops`) and
    B-tree expression indexes on the same documents.
- **J5 — for models**: `json_matches_schema`, MCP's JSON results, an
  example (a model's tool calls and structured outputs, stored, checked
  and queried), the guide, DOCS and the benchmark page.

### §M14.16 — Migrations and code generation ⬜

- `nucleus migrate`: versioned migration files (`up` / `down`), applied
  transactionally (DDL is transactional), recorded in `nc_catalog`;
  `status`, `new`, `diff` (the schema change between a database and the
  files), and a dry run on a branch (§M14.13).
- `nucleus codegen ts|rust|python|java`: types and typed query helpers
  generated from the schema (`nc_catalog`) and from checked SQL files.

**Done when:** a sample project migrates up and down, and the generated
code compiles and passes its tests in each language.

### §M14.17 — Easy start ⬜

- Packages: Homebrew, winget, apt/rpm, a Docker image, an install script;
  `nucleus init` for a ready local setup.
- A web console in the server (`--console`): browse tables, run SQL, see
  plans drawn, `EXPLAIN ANALYZE`, the analyzer's findings.
- A browser playground: nucleus compiled to WebAssembly (in-memory
  platform), the whole database in a web page — for docs and teaching.
- A guided tutorial in DOCS and the examples.

**Done when:** a new user goes from nothing to a running query in one
command per platform, and the playground runs the tutorial.

### §M14.18 — Operations ⬜

- **Online schema changes on large tables** (moved from §M14.11, first
  planned in §M14.19): `ALTER COLUMN … TYPE` converting a very large
  column in the background in chunks while the table stays writable — it
  runs as one transaction today, bounded in memory by spilling.
- Online backups (a checkpoint's pages are immutable: a consistent copy
  without stopping writes), and point-in-time restore from backups plus
  the WAL.  *Begun by §M14.24 R6:* `BACKUP TO 'dir'` / `nucleus restore`
  — entries at one snapshot, secondary indexes and samples left out and
  rebuilt, subjects' rows sealed under their keys.  Pages copied would
  be faster but hold index pages and unsealed values: an incremental
  form must keep R6's sealing (entries changed since a commit, not
  pages).
- **Incremental backups**: only the pages changed since the previous
  backup (the store tracks which pages a checkpoint rewrote), chained to
  a full backup; chains merged into a new full backup in the background,
  so a restore never replays a long chain.
- **Backup targets are modules** (principle 12): a local directory and
  S3-compatible object storage built in, others (GCS, Azure Blob)
  added the same way.  Backups are compressed, checksummed and encrypted;
  the WAL is archived to the same target continuously, so point-in-time
  restore reaches the last archived commit; retention policies (keep N
  full, every daily for a week, …) prune old backups and WAL.
- **Restore tests, automatically**: on a schedule, the latest backup is
  restored into a scratch directory and checked — every page checksum,
  the store opens, the catalog and indexes agree with the tables, row
  counts match the backup's manifest.  An untested backup is reported as
  one; each result is a metric and an audit entry.
- Metrics (Prometheus endpoint) and tracing (OpenTelemetry): commits,
  conflicts, cache, WAL, statement latencies, backups and restore tests,
  logins (§M14.25).
- **Health checks**: liveness and readiness endpoints for orchestrators
  (`/health/live`, `/health/ready` — ready only once recovery is done and
  the store is open), and `nucleus status` for people.
- **Alerting, ready to use**: a set of recommended alert rules
  (Prometheus rules) and a dashboard (Grafana) shipped with each release —
  disk space and its rate of use, WAL growth, a failed or overdue backup
  or restore test, long-running transactions, the rate of serialization
  conflicts, cache hit ratio, failed logins, and from §M17.5 replication
  lag — each with a short runbook entry in DOCS.
- TLS, users, roles, privileges and row-level security moved to §M14.25.
- ✅ An exclusive lock on the data directory, so a second process cannot
  open it (done first, 2026-10-17: DOCS §4.6).

**Done when:** a restore — from a full backup, from an incremental chain,
from local and from object storage — reproduces the database at a chosen
moment, under crash tests during backup and restore; the scheduled
restore test catches a deliberately corrupted backup; metrics and traces
reach a Prometheus and an OpenTelemetry collector in tests; readiness
waits for recovery; and every shipped alert rule fires in a test that
provokes it.

### §M14.29 — Security and assurance from outside ◐

nucleus's own tests (the simulator, crash tests, the laws, the fixtures
of other systems' answers) show what its authors thought to check; a
database that holds other people's data is also checked by people and
machines that did not write it.  Not one block at the end: each step
comes when it costs least and protects most (decided 2026-11-12) —
reporting from the day the repository is public, fuzzing while formats
and parsers are still cheap to change, an audit before anyone is asked
to run nucleus in production; the distributed layer's analysis is
§M23.5.

- **S1 — `SECURITY.md`** ✅ (2026-11-12): how to report a vulnerability
  privately (GitHub's private vulnerability reporting), what is answered
  and how fast, which versions get fixes, and how a fix is disclosed (a
  GitHub security advisory, a CVE where one applies, the release notes).
- **S2 — fuzzing in the repository, before §M14.23** (an embedded
  library opens files and takes queries from the application it runs
  in): fuzz targets (`cargo fuzz`, run in CI for a few minutes a change)
  for every input that crosses a trust boundary — the SQL lexer and
  parser, `ncp` and PostgreSQL's wire messages, the authentication
  exchanges (SCRAM, tokens, OIDC's JWTs), the text-search and `tsquery`
  parsers, `COPY`'s formats — and for what the engine reads back from
  disk: pages, WAL records, catalog and index records (a damaged file
  must fail with an error, never a panic or wrong data).
  ✅ (2026-11-13).  `fuzz/` (DOCS §2, `fuzz/README.md`): a crate of its
  own workspace, built by `cargo fuzz` (libFuzzer, AddressSanitizer, a
  nightly toolchain), ten targets — `sql_parse` (the lexer and parser;
  what parsed parses again the same), `values` (every type's text
  input, and back out), `text_search` (`@@` queries, `tsquery` and
  `tsvector`, highlighting, the chunkers), `copy` (text and CSV with
  options, into keys, checks, a foreign key and an index), `ncp` and
  `pg_wire` (a client's bytes from the first: login with no password or
  by SCRAM / OAUTHBEARER, queries, parameters, `COPY`, columnar loads),
  `scram` (both sides; a real client accepted exactly when its password
  is right), `jwt` (tokens before they are trusted, the claims of signed
  ones, key sets), `disk` (a real database's pages and WAL patched,
  then opened, checked and read) and `model_files` (`tokenizer.json`,
  `config.json` with `model.safetensors`, the encoder built and run).
  What makes them reach deep: the front ends serve any byte stream now
  (`handle_native_stream`, `nucleus_pg_compat::handle_stream`), so a
  connection is an input, no socket; the database is the simulation's
  file system under the machine's clock, `statement_timeout` 250 ms;
  `disk` seals each patched page and WAL record with its checksum again
  (by default) — a checksum stops random damage at the first read, so
  what is fuzzed is the damage it cannot see (a bug's wrong page, a
  misdirected or stale write; SQLite's `dbsqlfuzz` fuzzes a database
  file and the SQL run on it together, without checksums to pass);
  `jwt` signs the fuzzer's
  claims with the issuer's key.  Seeds kept in `fuzz/seeds/`
  (`fuzz/seeds.py`: the slt files' 1,960 statements, recorded sessions
  of nucleus's Python driver and psycopg, a SCRAM exchange with its
  proof computed, tiny model files cut from real ones).  CI's `fuzz`
  job runs each target three minutes a change, its corpus kept between
  runs; the lint job checks the crate.
  Found and fixed, each with a regression test:
  - `websearch_to_tsquery` sliced two bytes for `or` inside a character
    of more (a panic on `東京…`).
  - PostgreSQL's Bind took a negative count of formats or parameters as
    a vast one (a panic from any logged-in client); now `08P01`.
  - `ncp`'s columnar load made as many rows as a batch's count said
    before reading one (4 billion from four bytes); now the count must
    fit the batch.
  - The buffer pool's flush went through every frame and *made* each
    (`frame()` evaluated before the slot was checked): the first
    checkpoint allocated the whole cache, 128 MB, in every database
    opened.  Found profiling the harness (a new database 17 ms → 1.5 ms
    under ASan); a small or embedded database (§M14.23) now holds only
    the pages it uses.
  - A B-tree node whose keys are out of order (behind a valid checksum)
    panicked when written again (its key prefix taken from the first and
    last keys); now `Node::decode` refuses keys out of order, and a
    merge of siblings checks the seam — a `Corrupt` error.  The check
    is a comparison of neighbours on the write path's decode, which
    copies every key anyway.
  - `COPY`'s text format checked a record's fields as UTF-8 together,
    not each: `\xcc` and `\xb1` split by a tab are a character together
    (U+0331), so the field boundary fell inside it — a panic slicing the
    string (CI's first full run of `copy`).  Now each field must start
    and end on a character, `22P02` otherwise.
  - A time zone of four bytes was split as `HHMM` at its second byte,
    inside a character of two (`'2007-1ɚ0'::time`): a panic, the
    session lost (`values`).  Now only an ASCII zone is split.
  - **A statement nested deep overflowed its session's stack, and a
    stack overflow aborts the process: any client that could log in
    could stop the server** — a thousand parentheses, or twenty
    thousand `OR`s (`sql_parse`; the `OR` chain found reproducing it).
    The parser, and every stage after it (binding, planning, running,
    even dropping the tree), walk the statement by recursion.  Prior
    art: PostgreSQL bounds the stack itself (`max_stack_depth`, 54001
    "stack depth limit exceeded", the stack pointer checked in each
    recursive routine); SQLite (`SQLITE_MAX_EXPR_DEPTH`) and DuckDB
    (`max_expression_depth`) bound the expression tree's height at
    1000 as it is built.  Taken: the height bound, as it bounds every
    later stage too (and dropping a tree, which nothing could check),
    and needs no unsafe stack probing.  `parser::MAX_NESTING` = 1000:
    each frame (an expression, a query, a FROM item) counts its height
    — the nodes it chained (operators, casts and subscripts, joins, set
    operations) over the highest frame inside it — and its depth as it
    is entered, so the parser stops before its own stack grows; past it
    `54001`.  Measured (release, the stack a whole statement takes per
    level): an operator 3.7 KB, a parenthesis 4.4 KB, a set operation
    11 KB, a subquery 27 KB — so a subquery counts 8 and a set
    operation 3, and every shape at the limit takes under 6 MB (under
    16 MB in a debug build).  Threads that run statements — sessions
    (server, embedded), parallel workers, `COPY`'s parser — now get
    `nucleus_platform::SESSION_STACK`, 16 MB (8 MB on 32-bit), only what
    is used committed; Rust's default was 2 MB.  WebAssembly: the
    module's stack is 8 MB, and the host's own stack bounds it too — a
    browser's (V8's) is about 1 MB, which 850 levels of `CASE`
    exhausted once the engine's code was optimized (`RangeError`, the
    instance lost) — so there the limit is 500 (measured: every shape
    at it runs after warm-up).  `nesting.rs` checks every shape fails
    past the limit and runs just under it; the drivers' suite checks a
    deep statement fails and the session goes on.
  - A SCRAM client did as many PBKDF2 iterations as the server's first
    message asked — `i=4094096…` held the fuzzer past its timeout, and
    any server (or one in the middle, before TLS) could hold a client
    in the key derivation for minutes (`scram`, CI's later run).  Now
    a client refuses more than `SCRAM_MAX_ITERATIONS`, 1 000 000 (about
    a second; the server's own default is 4 096), as malformed — the
    Rust crate and the Python, JavaScript, Java and C drivers alike.
    Prior art: MongoDB's drivers refuse fewer than 4 096 (the floor);
    libpq takes any count — the ceiling is ours.
  Not a bug, recorded: HMAC pads a key shorter than its block with
  zeros, so a password and itself with NULs after are one in SCRAM (as
  everywhere; SASLprep forbids NUL).  The two §M14.22 review items
  meant for here are done too: a model's file at most a quarter of the
  machine's memory, and its download within the statement's time;
  remote calls bounded by the statement's time (`nucleus-http`'s
  `total`), so a server dripping a byte a read no longer holds one.
  Throughput under ASan on an M2 Pro: `sql_parse` ~5,800 inputs a
  second, `values` ~9,000, `copy` and `text_search` ~4,500, `ncp` and
  `pg_wire` ~300, `disk` ~18 (a database opened, checked and queried
  each).
- **S3 — OSS-Fuzz**: the same targets submitted to Google's OSS-Fuzz,
  which runs them continuously and files what it finds privately.
- **S4 — an independent security audit through OSTIF** (the Open Source
  Technology Improvement Fund, which arranges and funds audits of open
  source projects), before the first release meant for production: the
  network front ends, authentication, privileges and row-level
  security, secrets (§M14.22), `nucleus-crypto`'s own primitives, and
  the modules that reach out (OIDC, LDAP, embedding providers); its
  report and the fixes published.
- **S5 — Elle on one node**: the transaction guarantees as documented
  (serializable, snapshot isolation, read committed; DOCS §5.2) checked
  by Jepsen's history checker, Elle, over nucleus's own workloads in CI —
  cheap now, and the ground §M23.5 stands on.

**Done when:** `SECURITY.md` is in the repository and GitHub's private
reporting is on; every listed input has a fuzz target that runs in CI
and nucleus is in OSS-Fuzz; an audit's report and its fixes are
published; and Elle finds no anomaly the documented isolation level
forbids in nucleus's own histories.

---

## §F2 — NoSQL layer

On the same KV contract and transaction machinery as SQL.  The query layer
(planner + executor) is **shared**; only the frontend and the data-model
mapping differ.

### §M15 — Document model ⬜

- Binary document encoding (JSON-compatible, fast field access).
- Collections in the catalog; document id → key.
- Path indexes (`a.b.c`), array indexes.

### §M16 — Document queries + document server ⬜

- Filter language (Mongo-style) **and/or** SQL/JSON functions — decided at
  the end of M15.
- Compiles to the same logical plan as SQL.
- A transaction can span SQL tables and document collections.
- **Document server:** documents travel over `ncp` (D8): new messages for
  collections, and document APIs in every nucleus driver and in `ncsql`,
  in the same sessions and transactions as SQL.
- **MongoDB compatibility** (optional, like `--pg-compat`): a subset of
  the MongoDB wire protocol (`--mongo-compat`), so `mongosh` and MongoDB
  drivers can connect.

**Done when:** every nucleus driver runs CRUD and queries on collections,
in transactions that also touch tables; and with `--mongo-compat`,
`mongosh` and a standard MongoDB driver run CRUD and queries.

### §M16.5 — Streams: a transactional broker ⬜

Not a Kafka replacement — what the ordered KV and transactions give almost
for free, and what developers otherwise assemble from several systems:

- Streams: append-only, ordered topics stored as KV ranges, with
  retention; consumer groups with committed offsets.
- Publishing and consuming inside SQL transactions: a table write and its
  event commit together (a built-in outbox), and a consumer's offset moves
  only if its processing commits.
- Changefeeds (CDC, moved here from §M25): a table's committed changes as
  a stream, from the MVCC versions.
- Clients: `ncp` messages for subscribing, and the drivers' consumer API.
- **Task queues**: a stream with competing consumers, leases, retries,
  delays, priorities and a dead-letter stream — the queue an AI agent or
  a pipeline needs, transactional with the data it changes.
- **Patterns for AI agents**, built from these and shipped as documented
  schemas and runnable examples (`examples/agents/`, in Python and
  TypeScript first, then every driver):
  - *conversation memory* — messages and tool calls per session, with
    summaries;
  - *semantic memory* — facts with embeddings (§M14.22) and text
    (§M14.21), recalled by hybrid search, scoped by row-level security
    (§M14.25);
  - *tool and run state* — an agent's steps, checkpoints and results, so
    a run resumes after a crash;
  - *a task queue for agents* — work handed between agents, exactly once;
  - *experiments on a branch* (§M14.13) — an agent changes a copy, a
    person or a check merges or drops it.

**Done when:** exactly-once processing (consume, write, commit) survives
crash tests, changefeeds deliver every committed change in order, the
task queue survives crash tests with no lost or doubled task, and every
agent example runs in CI against `nucleus server`.

### §M16.6 — Live queries ⬜

A client subscribes to a query and receives its changes after every
commit that affects it — built on the changefeeds (§M16.5).

- `ncp` subscription messages; driver APIs (`subscribe(sql, params)` →
  initial rows, then inserts / updates / deletes).
- Incremental maintenance where the plan allows (filters, projections,
  joins on keys), re-evaluation otherwise; a limit on subscriptions.

**Done when:** subscribers see exactly the query's result after each
commit, under concurrent writes, in every driver.

### §M16.7 — Sync: devices and a server ⬜

Embedded databases (§M14.23) on phones, browsers and laptops, kept in sync
with a server — offline first: the application reads and writes its local
database at local speed, and changes flow both ways when a connection
exists.

- **Shapes**: a device subscribes to a subset of the server's data (rows
  a query selects, scoped by row-level security, §M14.25) and holds only
  that; the server streams changes into it from changefeeds (§M16.5).
- **Local writes** are committed locally, kept in an outbox and sent in
  order; the server applies them in a transaction, with its constraints
  and policies, and answers accepted or rejected.
- **Conflicts** resolved by a rule chosen per table: server wins, last
  writer wins (by hybrid logical clocks), or a merge function (a module);
  rejected writes are reported to the application, never dropped silently.
- **Resumable**: a device that has been offline for a week catches up from
  its last position, or re-syncs its shape if the server's retention has
  passed it.
- Clients in every embedded edition (Python, JavaScript, browser, Swift,
  Kotlin), over `ncp`.

**Done when:** devices that write offline and reconnect in any order
converge to the server's state under the chosen rules, with no lost and no
doubled write, under network-partition and crash tests; and a device sees
only rows its policies allow.

### §M17 — Further models ⏸

Decided later: wide-column, time series, graph (vectors are §M14.12).  All follow the same
pattern: a mapping onto the ordered KV + operators in the shared executor.

### §M17.5 — Read replicas, failover, management ⬜

Several servers on one database before the distributed layer: one writes,
the others serve reads.  Most deployments need exactly this, and it is far
simpler than per-range Raft (§M19), which later replaces it.

- **Replication:** the primary streams its logical WAL (committed key
  batches) to replicas over `ncp`; a replica applies it to its own copy and
  serves read-only queries at its applied snapshot.  Asynchronous, or
  synchronous for a chosen number of replicas (a commit waits for them).
  A new replica starts from an online backup (§M14.18) and catches up.
- **Failover:** promoting a replica (`nucleus promote`), with a fencing
  epoch so an old primary that comes back cannot accept writes; optional
  automatic promotion by a witness.
- **Clients:** drivers take several hosts, route reads to replicas and
  writes to the primary (or as told), reconnect after a failover, and can
  ask for read-your-writes (wait until a replica reached a commit).
- **Management:** `nucleus cluster status` and an admin endpoint — roles,
  lag in commits and time, health — plus the metrics of §M14.18.
- **Option, later — shared-storage readers:** committed pages are never
  overwritten (copy-on-write), so extra read-only servers could read
  snapshots straight from shared storage, if the writer leases freed pages
  until no reader needs them (Aurora-style compute/storage separation).

**Done when:** replicas follow the primary under load, a promotion loses no
committed write in synchronous mode (crash and partition tests), and
drivers fail over without application changes.

---

## §F3 — Distributed layer

A distributed implementation of the KV contract.  F1/F2 code is unchanged.

### §M18 — Range partitioning (single node) ⬜

- Split the keyspace into contiguous ranges; range descriptors live in the
  KV (meta range).
- Range split by size/load, range merge.
- Still a single process — the goal is to finish the routing logic before
  the network comes in.
- SQL table partitions (§M14.20) map onto ranges, so a partition and its
  storage mode can be placed as a unit later.

### §M19 — Raft replication ⬜

- Raft per range (leader election, log replication, snapshots, membership change).
- The Raft log runs **inside the deterministic simulator**, with injected
  network partitions, message loss and reordering.

**Done when:** the simulator runs a linearizability check (Porcupine/Knossos
style) across a thousand seeds without failure.

### §M20 — Cluster membership + placement ⬜

- Node join/leave, gossip or a central placement driver.
- Replica placement rules.

### §M21 — Distributed transactions ⬜

- HLC timestamps (swapping in the principle-3 type).
- Write intents + 2PC / parallel commits; transaction record.
- Handling the read uncertainty interval.

### §M22 — Distributed query execution ⬜

- Push filters/projections down to ranges; distributed joins and aggregation.

### §M23 — Fault tolerance + testing ⬜

- Rebalancing, node failure handling, slow nodes.
- Jepsen-style tests on a real network.

### §M23.5 — Jepsen's analysis ⬜

- The distributed guarantees — replication (§M17.5), distributed
  transactions (§M21), rebalancing and failures (§M23) — analysed by
  Jepsen itself on a real network with partitions, clock skew and
  crashes, from the one-node checks of §M14.29 S5; the report and the
  fixes published.

**Done when:** Jepsen's report is published and every anomaly it finds
is fixed or documented as the guarantee it is.

---

## §F4 — Above

### §M24 — Federation / connectors ⬜

- External data sources under the query layer (FDW-style): other SQL
  databases, CSV/Parquet, another nucleus cluster.
- Predicate push-down to the sources.

### §M25 — Multi-region ⏸

- Multi-region placement, follower reads.  (Changefeeds moved to §M16.5.)

---

## Changelog


- **2026-10-16** — first plan: vision, layers, 8 principles, 7 open
  decisions, F0–F4 / M0–M25.
- **2026-10-16** — D1 settled: Rust.  D7 settled: MIT.  Repo:
  `github.com/davidsarosi92/nucleus`.
- **2026-10-16** — plan translated to English (all repo content is English).
- **2026-10-16** — deployment modes (embedded / SQL server / document server
  / cluster); principles 9 (consumer-agnostic core) and 10 (portable and
  frugal); §M6.5 d-os settings store as the first consumer; D8 document
  client protocol; M16 gains the document server.
- **2026-10-16** — build targets: Linux, macOS, Windows, d-os; per-OS
  durability notes; artifacts are not stored in the repo (D9).
- **2026-10-16** — packaging and install per platform, including the d-os
  app format (`app install`, `.dos-app` manifest); first packages in M6.
- **2026-10-17** — §M0 shipped (DOCS §1–§3); CI green on all nine jobs.
- **2026-10-17** — D2–D6 settled as proposed; page size 8 KiB (M1).
- **2026-10-17** — §M1 shipped (DOCS §4); CI green on all nine jobs.
- **2026-10-17** — §F1 (SQL) complete: §M7–§M14 shipped (DOCS §8–§9); D12
  settled.
- **2026-10-17** — §M6.5 nucleus side done (settings service, protocol,
  packaging); the d-os side remains.
- **2026-10-17** — §M6 shipped: contract, CLI, packaging, release workflow;
  the d-os archive is built and checked in CI, not yet run on d-os.
- **2026-10-17** — D10–D12 recorded; §M1 revised (copy-on-write rule, free
  list in dedicated pages, no leaks); §M2–§M5 shipped.
- **2026-10-17** — §M14.5 shipped: set operations, FULL JOIN, CTEs,
  numeric / date / time / interval / array types, regular expressions,
  foreign keys (validated reads added to the KV contract), `nc_catalog`
  and the PostgreSQL compatibility catalog; D13 settled.
- **2026-10-17** — §M14.6 shipped: `ncsql` and `nucleus-client`; the
  server's `--log-queries`.
- **2026-10-17** — D14 settled: nucleus's own protocol and drivers; the
  PostgreSQL protocol becomes optional.  §M14.7 shipped; default
  credentials `nucleus` / `nucleus`.
- **2026-10-17** — §M14.8 shipped: drivers for C, C++, Python, JS/TS and
  Java beside Rust, with `examples/`; D15 (versioning) opened.
- **2026-10-17** — Developer appeal planned: §M14.9 SQL analyzer, §M14.10
  AI access (MCP), §M14.11 large data foundations, §M14.12 vectors; §M16.5
  streams and a transactional broker (changefeeds moved there from §M25).
- **2026-10-17** — Developer platform extended: §M14.13 branches,
  §M14.14 time travel, §M14.15 JSON, §M14.16 migrations and code
  generation, §M14.17 easy start, §M14.18 operations (with the data
  directory lock, urgent); §M16.6 live queries.
- **2026-10-17** — The data directory lock shipped (one process per
  database).  §M17.5 read replicas, failover and cluster management planned
  (shared-storage readers as a later option).
- **2026-10-17** — D8 settled: documents over `ncp`, MongoDB compatibility
  optional; principle 11 (one system, many services; compatible, not
  borrowed).  §M16's "done when" follows.
- **2026-10-17** — Principle 12: modular, any part replaceable or
  extensible (as in d-os); §M14.19 turns today's hard-coded lists into
  registries with an extension API.
- **2026-10-17** — Fidelity to the principles comes first (stated under
  the principles).  §M14.19 is next: a principle audit, registries, types
  that can be added, modified and updated (versioned encodings, online
  column type changes), an extension API.
- **2026-10-17** — §M14.19: type capabilities (equality, hashing, order,
  key encoding, operators, distance, casts, aggregation, partial update,
  statistics, modifiers, encoding versions, conversions), consulted by
  every layer, listed in `nc_catalog`; laws checked for every type.
- **2026-10-17** — §M14.19: teaching a type new abilities in SQL
  (`ALTER TYPE … ADD ORDERING / OPERATOR / DISTANCE / CAST`), preferably
  by a mapping into a type that already has them; several named orderings
  per type (`USING`), collations as text orderings; operators extend the
  vocabulary, not the grammar; dependencies tracked in the catalog.
- **2026-10-17** — §M14.12: `vector` is a built-in type with every
  capability (equality, hashing, an element-wise order and key encoding,
  four distances, operators, aggregates, statistics, driver mappings);
  §M14.19's partial-capability examples use a `geopoint` sample type.
- **2026-10-17** — Large data and analytics: §M14.11 measures against
  PostgreSQL first (TPC-H, ClickBench, pgbench-like, ANN recall) and adds
  columnar `ncp` results that drivers hand over zero-copy (numpy, Arrow,
  pandas, polars, typed arrays); an embedded Python mode is not planned
  for now.  New §M14.20: vectorized execution, SIMD batch kernels and
  columnar encodings as type capabilities, parallel execution, columnar
  storage beside rows (HTAP).
- **2026-10-17** — §M14.20: storage mode (`row`, `column`, both) chosen
  per table, per column and per partition, all required in the milestone;
  declarative partitioning (range, list, hash) with pruning and an age
  policy; table storage as a module on the KV contract; online conversion.
  §M14.11's benchmarks are no longer first.
- **2026-10-17** — For AI applications: §M14.12 filtered vector search
  (filters inside the graph search, selectivity-based exact search,
  per-tenant index partitions); new §M14.21 full-text and hybrid search
  (BM25, analyzers as modules, rank fusion with filters); new §M14.22
  embeddings in the database (`embed()`, embedding columns kept current
  through a durable job queue, model versions, chunking); §M14.18
  row-level security everywhere rows are read; §M16.5 task queues and
  agent patterns (`examples/agents/`).
- **2026-10-17** — §M14.19: modules live in one repository but are built
  to leave it (`crates/`, `modules/`, `editions/`, `drivers/`,
  `bindings/`; public-API-only dependencies; a boundary check both ways
  and a detachability test in CI), `pg-compat` first.  New §M14.23
  embedded editions (in memory or on a file inside Python and JavaScript,
  the browser, iOS and Android) and §M16.7 sync between devices and a
  server; build targets and D15 updated.
- **2026-10-17** — New §M14.24, memory for AI models: knowledge kept in
  nucleus and read at generation time — updatable (`remember` / `recall`
  / `forget`), provably erasable (`FORGET` through indexes, derived rows,
  history, WAL and backups by crypto-shredding), permissioned by
  row-level security, auditable (sources, recorded read sets, `AS OF`),
  with an in-process retrieval API for inference servers.
- **2026-10-17** — New §M14.25, identity and access, right after §M14.19:
  users and roles in SQL, privileges on every object, authentication
  modules (SCRAM with a new `ncp` handshake, scoped tokens for agents,
  client certificates, OIDC, LDAP), access rules by source, front end and
  method, privileges by connection context (ceilings and conditional role
  membership), row-level security (moved from §M14.18), protection and a
  hash-chained audit log.
- **2026-10-17** — §M14.18 grows: incremental backups, backup targets as
  modules (local, S3-compatible), encrypted archives with continuous WAL
  archiving and retention, scheduled restore tests, health endpoints,
  shipped alert rules and dashboard.  §M14.19: `UUID` (v4 and v7
  generators) is the first new type built through the type registry.
- **2026-10-17** — New §M14.26, core types, right after §M14.19 and
  before §M14.25: integers by width, `real`, `bytea` (with out-of-line
  large values), `time`, `char(n)`, `inet` / `cidr`, enums whose labels
  can be added anywhere without a rewrite.
- **2026-10-17** — §M14.19 started: the principle audit (table in
  §M14.19).  Fixed: principle 3 (`Timestamp` opaque), 4 (`now()` and the
  retry back-off through the platform), 5 (a SQL crash test and
  consistency check), 6 (the SQL layer's format record); 10 and the
  network part of 4 scheduled (§M14.11, §M19); 11 and 12 are the rest of
  this milestone.
- **2026-10-17** — §M14.19: the extension API (`nucleus_sql::ext`) with
  registries for scalar functions, schemas and catalog views; the
  built-ins register through it; `pg-compat` is a module, on only in the
  PostgreSQL front end's sessions.
- **2026-10-17** — §M14.19: aggregates and table functions registered too.
- **2026-10-17** — §M14.19: front ends and authentication registered in
  the server; `nucleus server --front-end <name>`.
- **2026-10-17** — §M14.19: `modules/pg-compat` (the first detachable
  module), `editions/nucleus`, `crates/testkit`; the boundary check and
  the detachability test in CI.
- **2026-10-17** — §M14.19 T1–T2: types as modules — `ext::Type` and its
  abilities, the built-ins registered, registered types stored and
  compared (`geopoint` sample), refused where an ability is missing; the
  principle audit's rows 11 and 12 closed.
- **2026-10-17** — §M14.19 T3: `UUID` through SQL, `ncp` (version 2), all
  six drivers and pg-compat; its slt script checked against PostgreSQL 18.
- **2026-10-17** — §M14.19 T4: the type law kit; registration refuses a
  type whose examples break a law.  It found arrays' key encoding putting
  a `NULL` element before the others: key encoding version 2, with an
  upgrade of version-1 databases.
- **2026-10-17** — §M14.19 T5: orderings taught in SQL (`ALTER TYPE …
  ADD ORDERING … BY`), `ORDER BY … USING`, indexes in an ordering with
  their dependencies, `COLLATE` as a text ordering; function volatility.
  Operators, distances and casts taught in SQL are T5b.
- **2026-10-17** — §M14.19 T5b: operators outside the grammar's own
  (PostgreSQL's lexing and precedence), a type's distance as its operator,
  modules' operators, operators and casts taught in SQL.
- **2026-10-18** — §M14.19 T6: encoding versions of registered types
  (upgraded as read, `ALTER TYPE … UPGRADE`, crash-tested) and `ALTER
  TABLE … ALTER COLUMN … TYPE … [USING]`, checked against PostgreSQL 18.
- **2026-10-18** — §M14.19 shipped (DOCS §1, §8.11–§8.12, §9): the
  sample extension `examples/extension` (T7) closes it; what was left for
  later is listed in §M14.19.  Next: §M14.26.
- **2026-10-18** — §M14.26: integers by width (`smallint`, `integer`,
  `bigint`), checked against PostgreSQL 18; row encoding version 2 (2-,
  4- or 8-byte integers); `ncp` given its own value encoder, so storage
  encodings no longer leak into the protocol.
- **2026-11-01** — §M14.20 V13: what other engines do, the first three —
  key filters from hash joins (the scans below a join's probe side
  narrowed by its keys: Q5 1.19× → 0.81× PostgreSQL's time on column
  tables); eager aggregation (Q13 1.09× → 0.29×); the morsels' groups
  merged on the workers, those in key order finished there (Q18 0.55× →
  0.23×); the aggregation table with salts and a direct index (Q1 0.54×
  → 0.47×); batch probes, pair keys and build sides gathered in columns
  (Q9 1.81× → 0.70×).  ClickBench on column tables 0.31× → 0.26×.
- **2026-11-01** — §M14.20: TPC-H Q13 and Q16 — a hash join's table
  made by the workers holds its rows and keys a column at a time (Q13's
  `orders` side fits work_mem now, where it spilled); `LIKE` with `%`
  inside a pattern a kernel; `NOT LIKE` estimated as the rest of `LIKE`;
  uncorrelated `IN (subquery)` a list kernel, and uncorrelated
  subqueries run once before workers look up or scan with them.  TPC-H
  SF 1: 0.67× PostgreSQL's time on column tables (from 0.75×), 1.27× on
  row tables (from 1.40×); Q13 3.8× → 1.09×, Q16 3.4× → 1.00×;
  ClickBench 0.31× and 2.31× (`docs/benchmarks.md`).
- **2026-11-01** — §M14.20: TPC-H Q19 and Q22 — `char(n)` columns a
  vector of their own (row and column storage), `IN` lists of constants
  a kernel, scalar functions' batch kernels (`ScalarFunction::
  call_batch`, `substr` the first), uncorrelated subqueries evaluated
  once in a batch; index nested loops on worker threads; the buffer
  pool's hits under a read lock of their shard, the column store's cache
  of decoded columns in shards the same way (one bound for all).  TPC-H
  SF 1: 0.75× PostgreSQL's time on column tables (from 1.03×; the
  target, 0.8×, met), 1.40× on row tables (from 2.34×; within 2×); Q22
  7.2× → 1.0×, Q19 7.9× → 2.95× on column tables; ClickBench 0.32× and
  2.32×; `bench/run.py` within its targets (`docs/benchmarks.md`).
- **2026-11-01** — §M14.20: TPC-H and ClickBench measured again at
  `e2d1afa`: TPC-H 1.03× PostgreSQL's time on column tables (1.10× on
  10-30), 2.34× on row tables (2.24×; a second run 2.26×, the row
  tables' times varying by whole queries between runs — not yet
  explained); ClickBench 0.34× and 2.47× (0.34× and 2.51×); loads
  98 s / 111 s against 26 s (118 s / 125 s) and 18.7 s / 20.9 s against
  7.2 s (21.7 s / 21.4 s).
- **2026-11-01** — §M14.20: the self-join — a hash join's right side
  collected on worker threads when it is a pipeline, its table without an
  allocation per key: 1.7× PostgreSQL's time (4.2× before); every
  `bench/run.py` target met.
- **2026-11-01** — §M14.20: the load — one look at the tree for a run
  of new keys (not one per row), a spill's keys checked as a commit's,
  no intent delete for keys written after the spill outside its span:
  the 1M-row `INSERT … SELECT` 1.2–1.4× PostgreSQL's time (2.0–2.2×
  before); the `bench/run.py` load target met.
- **2026-11-01** — §M14.20: a commit's leaves prepared on workers while
  the walk installs earlier ones (swapped into the pool, not copied): a
  10% delete ~136 ms, 1.3–1.9× PostgreSQL's (its own time varied 75–107
  ms in the run).
- **2026-11-01** — §M14.20: a delete's rows stashed once (not again
  with a partition number when there is one table): the 10% delete
  1.96× PostgreSQL's time — the `bench/run.py` write targets met, the
  delete and the load at the edge.
- **2026-11-01** — §M14.20: the write set a sorted vector while writes
  come in order, a large commit's keys one sorted run among the recent
  commits': against PostgreSQL in the same run, a 10% delete 2.2×, a 25%
  update 1.0×, the load 2.0× (the update and load targets met).
- **2026-11-01** — §M14.20: garbage collected as leaves are rewritten —
  a commit's batch drops the versions no snapshot sees from the leaves it
  writes anyway (a compaction filter, not logged), instead of walking its
  keys' versions; its conflict check from the recent commits' keys: a 10%
  delete ~2.1×, a 25% update ~1.1× PostgreSQL's 10-31 time; repeated
  updates faster and the table smaller.  Against a fresh PostgreSQL run
  (faster writes than on 10-31): the delete 3.1×, the update 1.3×.
- **2026-11-01** — §M14.20: a commit's leaves merged on the CPUs (the
  tree the same page for page): a 10% delete ~2.5×, a 25% update ~1.2×
  PostgreSQL's 10-31 time.
- **2026-11-01** — §M14.20: less per written row (stashes consumed,
  keys encoded once, commit batches sorted by the engine): a 10% delete
  ~2.9×, a 25% update ~1.3×, the load ~2.6× PostgreSQL's 10-31 time.
- **2026-11-01** — §M14.20: the write path — `UPDATE` / `DELETE` find
  their rows by the parallel vectorized scan; early conflict checks from
  the recent commits' keys; a commit's leaves merged without decoding;
  a 10% delete 7.5× → 3.2× PostgreSQL's time, a 25% update 2.2× → 1.4×
  (the update target met).  Fixed: recent keys pruned past the visible
  snapshot.
- **2026-10-31** — §M14.20: ClickBench against PostgreSQL measured (a
  column table 0.34× of its time) and the targets recorded
  (`docs/benchmarks.md`).
- **2026-10-30** — §M14.20 V12: statistics across columns — column
  groups' distinct combinations and functional dependencies (`CREATE
  STATISTICS`, and every multi-column key and index), used by the
  estimates of filters, joins and groupings.
- **2026-10-30** — §M14.20 V11: write intents — large transactions'
  spilled writes as versions stamped with their id, nothing moved after
  the commit; `VACUUM`, run by the server when there are intents.
- **2026-10-30** — §M14.20 V10 (second part): TPC-H against
  PostgreSQL; subqueries unnested into semi and anti joins; correlated
  subquery and `COPY` fixes.
- **2026-10-30** — §M14.20 V10 (first part): results columnar end to
  end — batches into `ncp` columnar results without a value per cell.
- **2026-10-30** — §M14.20 V9 (second part): attach, detach and drop
  partitions; storage policies by age, applied by the server.
- **2026-10-30** — §M14.20 V9 (first part): partitioning — range, list,
  hash, default; pruning at plan and run time; routed inserts, moving
  updates; indexes and columns on every partition.
- **2026-10-30** — §M14.20 V8: storage per table and per column —
  `row, column`, `ALTER TABLE … SET STORAGE` online, markers by row.
- **2026-10-29** — §M14.20 V7: columnar storage — `WITH (storage =
  column)`, segments, delta store, markers, merges, skipping by least /
  greatest values.  A million-row `sum` 22 → 2 ms.
- **2026-10-29** — §M14.20 V6: table storage as a module — the
  `TableStorage` trait and registry, `row` behind it, `WITH (storage =
  …)`.
- **2026-10-29** — §M14.20 V5: compression — key prefixes stored once
  per leaf, LZ-compressed values; format version 2.  A million-row
  table's file 93 → 61 MB.
- **2026-10-29** — §M14.20 V4: parallel execution — morsels at the KV's
  split points on worker threads, partial aggregate states combined in key
  order, filtering pipelines in rounds; `parallel_workers`; exact double
  sums.  A million-row `sum` 77 → 27 ms.
- **2026-10-29** — §M14.20 V3: batch kernels per CPU level
  (`nucleus-kernels`), tested against scalar references; text and date /
  time vectors, `LIKE` and string comparisons in kernels.  A million-row
  `sum` 90 → 77 ms.
- **2026-10-29** — §M14.20 V2: the vectorized executor — pipelines,
  kernels, vectorized aggregates (`Aggregate::grouped`), `batch_size`;
  integer sums exact in 128 bits (order-independent).  A million-row
  `sum` 265 → 90 ms.
- **2026-10-29** — §M14.20 started; steps V1–V10 recorded.  V1: batch
  reads through the stack — the KV contract's `scan_chunks`, the B+tree
  and the engine reading through reused buffers (`count(*)` of a million
  rows 360 → 200 ms).
- **2026-10-27** — §M14.11 shipped (DOCS §5.2, §8.5, §8.7, §10,
  `docs/benchmarks.md`).  The "done when", as checked: bounded memory —
  peak RSS 917 MB at 2 million rows, 646 MB at 10 million, the same work;
  100 million not run locally (disk); numpy 10 million values in 5.3 s
  with the server's scan 4.5 s of it; the suite in CI against
  `postgres:18`.  Parallel scans and page compression moved to §M14.20,
  background column conversion to §M14.18.  Next: §M14.20.
- **2026-10-27** — §M14.11 L8: the benchmark suite (`bench/run.py`) and
  its CI job against `postgres:18`; the first results and the targets set
  from them are in `docs/benchmarks.md` (1 million rows: loads and updates
  within 3–5×, reads by key faster, scans 15–30× slower — §M14.20's
  work).
- **2026-10-27** — §M14.11 L7: scans decode only the columns the plan
  reads (`prune`, by plan address in the execution context — conservative
  where a subquery could read the row, its mutation caught by the slt
  suite); an engine scan without side areas fills its buffer directly; a
  grand total hashes no key.  A count over 2 million rows 1.0 s → 0.9 s:
  what remains is per-row copying between the B+tree, the engine and the
  executor, and pages past the cache — the work of §M14.20 (zero-copy
  batches, vectorized execution) rather than of more patches here.
- **2026-10-27** — §M14.11 L7: CRC-32C by slicing-by-8 (every page read
  and written checks one); buffer frames allocated when first used, so
  the default page cache is 128 MiB (was 8 MiB) at no cost to a small
  database.  2 million rows (`id int pk, v bigint, s text`), release
  build, against PostgreSQL 18 on the same machine: load 11.3 s / 4.1 s,
  `count(*)` 1.13 s / 0.067 s, a filtered count 1.29 s / 0.035 s, UPDATE
  of 500 000 rows 6.1 s / 2.0 s, DELETE of 200 000 4.9 s / 0.2 s, a sort
  3.2 s / 0.65 s, a self-join 5.2 s / 0.28 s — scans are the largest gap
  (per-row work in the executor, then no parallel workers).
- **2026-10-27** — §M14.11 L7 (first part): measured first — a
  2-million-row INSERT took 197 s, nearly all of it decoding whole B+tree
  pages per lookup and applying commit batches key by key.  `get` and
  cursors now read pages in place (`node::View`); `Store` applies a batch
  of distinct keys in one sorted pass (`BTree::apply_sorted`: one decode
  and encode per node touched, leaves packed to 7/8, batched removals
  rebalanced); the load takes 13 s, a second 200 000-row load into a
  non-empty database 8.7 s → 0.8 s.  `ALTER COLUMN … TYPE` stashes its
  rows.
- **2026-10-27** — §M14.11 L6c (L6 done): joins past `work_mem` — the
  right input stashed; grace hash joins, block nested loops, a full join
  without its output in memory; checked with 64 kB against 64 MB, its
  mutations caught.
- **2026-10-27** — §M14.11 L6b: hash aggregation, `DISTINCT`, `UNION`,
  `INTERSECT` / `EXCEPT` past `work_mem` — hybrid: what fits stays in
  memory, the rest is hash-partitioned and taken in turn (accumulators
  never serialised).  Also: the L6a commit's formatting (CI's lint).
- **2026-10-27** — §M14.11 L6a: `work_mem` (a session and a database
  setting); the KV contract's `scratch()` (files in `scratch/`, in memory
  by default); external merge sorts; CTEs, recursion and writes' rows
  stashed; streaming table functions; ANALYZE by reservoir sample.
  Checked by running the same statements with 64 kB and 64 MB.
- **2026-10-27** — §M14.11 L5b: large transactions — writes past the
  budget spill into a reserved side area of the store; commit atomic by a
  status record; resolution into versions with readers merging the side
  area meanwhile (a concurrent reader never sees part of a commit);
  rollback, drop and recovery drop or resolve side areas.
- **2026-10-27** — §M14.11 L5 done: `copy` in every driver; `load` —
  Python (pandas, pyarrow, numpy columns without a value per cell),
  JavaScript (typed arrays as they are), Java (primitive arrays).
- **2026-10-27** — §M14.11 L5 (first part): `COPY … FROM STDIN` — text
  and CSV parsed as they arrive, typed rows from `ncp` batches, each row
  checked as an INSERT's, the whole load one transaction; pg-compat's
  COPY (checked with a real PostgreSQL driver), `ncsql \copy`.  Found:
  a transaction holds its writes in memory until it commits, so a load
  is bounded by memory — L5b added.
- **2026-10-27** — §M14.11 L4: columnar results in every driver — Python
  into numpy, Arrow, pandas and polars (CI installs them), JavaScript
  typed arrays, Java buffers, C and C++ pointers into the batch.
- **2026-10-27** — §M14.11 L3: columnar results — `ncp` version 7's
  batches (a buffer per column, Arrow's layout for variable widths, a
  bool a byte, numeric as mantissa and scale), the Rust driver's
  `Client::batches`; checked cell by cell against the rows for every type.
- **2026-10-27** — §M14.11 L1–L2: results stream — the executor's rows
  go to a sink as produced (`Session::run_into`), `ncp` version 6 adds a
  fetch size and `S` / `F` / `c`, every driver reads through a cursor
  (closed early, it stops the statement), pg-compat streams; Execute
  with a row limit still keeps the result (a portal suspended across
  messages would need the executor to outlive the call).
- **2026-10-23** — §M14.10 shipped (B3 `nucleus mcp`, B4 `docs/llm.md`).
  Every "done when" holds: an MCP client lists, describes and queries a
  database end to end (`editions/nucleus/tests/cli.rs`), and the common
  error codes have hints (`crates/sql/tests/hints.rs`, over `ncp` and pg
  too).  Next: §M14.11.
- **2026-10-23** — §M14.10 B1–B2: comments and hints in errors.
- **2026-10-23** — §M14.9 shipped (A4: the index advisor).  Every "done
  when" holds: each lint rule and the advisor have slt cases (firing and
  quiet), and `EXPLAIN ANALYZE`'s actual rows are checked against the
  queries' results.  Next: §M14.10.
- **2026-10-23** — §M14.9 A3: the linter.
- **2026-10-23** — §M14.9 A1–A2: `EXPLAIN ANALYZE` (actual rows checked
  against the results in slt) and statement statistics.
- **2026-10-23** — §M14.25 shipped (T7: the audit log, with a broken
  chain found).  Every "done when" holds: privileges and row-level
  security in slt with leak tests; SCRAM, tokens, certificates, OIDC
  (against a test issuer) and LDAP through every driver; access rules
  over real sockets from several addresses; the insecure start refused;
  failed logins throttled; every login and change of access audited.
  Next: §M14.9.
- **2026-10-23** — §M14.25 T6: protection — failed logins locked, limits
  on connections and statements, password rules, no insecure start.
- **2026-10-23** — §M14.25 T5: access rules and privileges by connection
  context — the same user with full rights from one address and read only
  from another, a role held only under its condition, over real sockets
  (proxied, and 127.0.0.1 against ::1).  Next: T6.
- **2026-10-23** — §M14.25 T4: LDAP as a module, checked against OpenLDAP
  2.6; with it every authentication method of T4 is in.  Next: T5.
- **2026-10-23** — §M14.25 T4: OpenID Connect as a module; tokens of a
  test issuer log in through every driver.
- **2026-10-23** — §M14.25 T4: SCRAM-SHA-256-PLUS (channel binding) on
  pg-compat, checked with `psql channel_binding=require`, and on `ncp` in
  all six drivers (checked by a server that required it).
- **2026-10-23** — §M14.25 T4: client certificates (mutual TLS) on both
  front ends and in all six drivers.
- **2026-10-23** — §M14.25 T4: TLS as a module (`modules/tls`, rustls),
  so the core stays without a TLS library; both front ends and all six
  drivers; checked with `psql` (`verify-full`).
- **2026-10-23** — §M14.25 T4: tokens — a login name and a secret, so
  every driver and PostgreSQL client takes them as they are; a ceiling
  (read only, tables) that narrows the role and holds through `SET ROLE`.
- **2026-10-23** — §M14.25 T4: SCRAM-SHA-256 on the wire — `ncp` version
  4 proves the password and never sends it; all six drivers speak it,
  with the C driver's primitives written in the driver; pg-compat uses
  PostgreSQL's SASL (checked with `psql`).
- **2026-10-23** — §M14.25 T3: row-level security, with leak tests that
  agree with PostgreSQL 18 run in one session.
- **2026-10-23** — §M14.25 T2: privileges on objects, checked at bind
  time and before DDL, checked against PostgreSQL 18 in one session.
- **2026-10-23** — §M14.25 T1: users and roles in the catalog, password
  verifiers, owners, logins from the catalog (`nucleus init` claims a
  database); a new core crate, `nucleus-crypto`.
- **2026-10-22** — §M14.26 shipped (DOCS §8.1, §8.2, §10, §11): what was
  left for later is listed in §M14.26.  Next: §M14.25.
- **2026-10-22** — §M14.26: `ncp` version 3 with native tags for the new
  types; all six drivers speak it, with the types both ways in their
  conformance tests.
- **2026-10-22** — §M14.26: enums, checked against PostgreSQL 18; adding
  and renaming labels rewrites no row (tested on the stored bytes).
- **2026-10-22** — §M14.26: `inet` and `cidr`, checked against
  PostgreSQL 18; containment scans an index range through a new hook of
  the extension API, `ScalarFunction::implied_range`.
- **2026-10-22** — §M14.26: `char(n)`, checked against PostgreSQL 18.
- **2026-10-22** — §M14.26: `time`, checked against PostgreSQL 18.
- **2026-10-22** — §M14.26: large values out of line, for every type:
  chunks keyed by their row, written and removed in its transaction, kept
  by updates that leave them, checked and crash-tested.
- **2026-10-22** — §M14.26: `bytea` — the type, its text forms and
  functions, digests without a dependency; out-of-line storage of large
  values and the drivers' native types are its next steps.
- **2026-10-18** — §M14.26: `real`, and PostgreSQL's float output for
  `double precision` too; `INSERT … VALUES` bound against its target
  columns, as PostgreSQL does.
- **2026-11-02** — §M14.20 V14: the TPC-H load profiled phase by phase
  and the whole engine read for speed and for work done twice; the plan
  W0–W8 and the rule "one mechanism per job"; D16 (bulk loads without the
  WAL, switchable), D17 (`sync = ordered` by default), D18 (format
  changes now); `CLAUDE.md` (prior art as a source of ideas, not copies).
- **2026-11-02** — §M14.20 W0: concurrent writes of a column table
  conflict as a row table's do (a correctness bug: two overlapping writers
  both committed and the second's rows were lost); DOCS §8.15.
- **2026-11-02** — §M14.20 W1: `ANALYZE` by Algorithm L, decoding to the
  last wanted column, 1 GiB checkpoints, D17's sync policy, 64 KiB wire
  buffers; the TPC-H SF 1 load 79.4 → 69.8 s.
- **2026-11-02** — §M14.20 W2: one sorted index builder for `CREATE
  INDEX`, `ALTER … TYPE` and key rewrites (parallel scan, abbreviated keys,
  one run per spill, uniqueness by neighbours), internal nodes packed;
  fixed a failed column-table statement's rows outliving it; the load
  69.8 → 45.6 s.  DOCS §3, §8.7, §8.15.
- **2026-11-03** — §M14.27 added: statistics and numerical analysis on
  large data for AI models and AI developers — a study first (who needs
  what, the function families and their mergeable methods, prior art, the
  fit with the principles), then the plan.
- **2026-11-03** — §M14.20 W3: one row builder for `COPY` and `INSERT`,
  text parsed straight to column types, `COPY` parsed and built in
  parallel, one statement context, a column table's existence checks
  through the KV's remembered empty ranges (`Transaction::first`); the
  row load 45.6 → 37.9 s, a column `lineitem` `COPY` 36.5 → 12.8 s; the
  batch work moved to W3b after W4.  DOCS §6.1, §8.7, §8.15.
- **2026-11-03** — §M14.20 W4, the checkpoint: in two halves (the pager's
  `freeze` / `commit_frozen`), its slow half off the writer lock; one
  trigger and a checkpointer thread; WAL segments deleted with one
  directory sync; ordering-only syncs (`File::sync_order`); the row load
  37.9 → 35.3 s.  DOCS §3.1, §4.2, §4.3, §4.5–§4.7, §5.
- **2026-11-03** — §M14.20 W4, D16: values written into a fresh range
  skip the WAL, stored as final versions; the range marked in a logged
  record, checkpointed before the commit record, deleted on a rollback or
  by recovery; `--wal minimal|full`; `CREATE INDEX` on SF 1 17.0 → 13.5 s.
  DOCS §4.6, §5.1–§5.3, §6, §8, §9.
- **2026-11-03** — §M14.20 W3b: one batch insert path for `INSERT`,
  `COPY` and partition moves (`dml::insert_batch`,
  `TableStorage::insert_batch`), segments built from typed vectors on
  threads, `COPY`'s parse overlapped with its stores, `ANALYZE` through
  the storage's sampling method (`TableStorage::sample`); fixed enum
  labels read from column segments; the row load 34.0 → 29.0 s, the
  column load 34 → 26–28 s.  DOCS §8.7, §8.8, §8.15.
- **2026-11-03** — §M14.20 W5, page reads: the pager's page checks
  without its lock, coalesced reads ahead of a cursor (a window ramping
  1 → 16 pages), CRC-32C in three interleaved streams; the 22 TPC-H
  queries on row tables 10.0 → 8.9 s.  DOCS §4.1–§4.3.
- **2026-11-03** — §M14.20 W5, reads past intents: a spilled
  transaction's intents' keys kept as up to eight ranges, so only the
  reads they reach merge intents; the merging pass decodes in place;
  Q6 over a just loaded `lineitem` 385 → 330 ms.  DOCS §5.2.
- **2026-11-03** — §M14.20 W5: `Value` 64 → 32 bytes (decimals and
  addresses aligned to 8, enum values behind an `Arc`); the 22 TPC-H
  queries 8.90 → 8.37 s on row tables, 4.58 → 4.31 s on column tables.
  DOCS §8.1.
- **2026-11-03** — §M14.20 W5: decimal vectors' mantissas 64-bit while
  they fit (a cached decimal column half the size); column tables 4.33 →
  4.25 s.  DOCS §8.5.
- **2026-11-03** — §M14.20 W5 done: TinyLFU admission for the segment
  cache tried and not taken (TPC-H 4.20 → 6.72 s), the write path's
  copies measured (1.7 % of a `COPY`) and left; `bench/phases.py`, a
  step's load and reads measured against another build; next W6.
- **2026-11-03** — §M14.20 W6: groupings past `work_mem` on the workers —
  morsels write their groups' partial states to scratch by merge part,
  the parts read them back, one bound for the whole aggregate; ClickBench
  Q32 2.5 → 0.62 s, Q18 0.95 → 0.24 s, Q39 1.10 → 0.33 s (the 43 queries
  9.06 → 5.13 s), TPC-H Q13 147 → 179 ms (its groups recur in every
  morsel); `bench/phases.py clickbench`.  DOCS §8.5.
- **2026-11-03** — §M14.20 W6: top-N — a sort below a `LIMIT` keeps only
  its rows in a heap (gathered to 2k first, as PostgreSQL), a parallel
  sort's morsels building a row only when it enters; ClickBench 5.1 →
  4.7 s (Q32 610 → 525 ms, Q24 11.2 → 5.4).  DOCS §8.5.
- **2026-11-03** — §M14.20 W6: a decimal division in one 128-bit
  division when the scaled dividend fits (the long division for the
  rest); ClickBench 4.86 → 4.73 s (Q32 540 → 508 ms).
- **2026-11-03** — §M14.20 W6: typed aggregate states — a value a group
  for `min`/`max`/`bool_*`, an exact sum and a row count for a `sum` or
  `avg` of integers or one scale's decimals, `Acc`s for the rest; a
  grouping counts each accumulator's own bytes a group against
  `work_mem`; ClickBench Q32 531 → 445 ms, Q6 14 → 6; TPC-H column Q18
  280 → 248.  DOCS §8.5, §8.12.
- **2026-11-03** — §M14.20 W6: `DISTINCT` inside aggregates planned as
  two groupings (by group and value, the other aggregates' partial
  states; then by group) — the vectorized, parallel grouping, where it
  was a set a group a row at a time; `Aggregate::order_free`; ClickBench
  4.9 → 2.4 s (Q4 547 → 14 ms, Q9 728 → 74).  DOCS §8.6, §8.12.
- **2026-11-03** — §M14.20 W6: shared reads without a queue — intent
  statuses under a reader-writer lock, `KvRead::first` without
  read-ahead, unlogged writes flushed only with a fresh range;
  `EXPLAIN ANALYZE` reads only the columns needed (Q19 18.6 s → 145 ms
  under it).  TPC-H Q19 on column tables 65 → 45 ms, Q21 543 → 462.
- **2026-11-03** — §M14.20 W6: correlated subqueries on the workers —
  per row, as on one thread (`expr::ready_subqueries`), in pipelines,
  lookups, and filters and projections over other rows
  (`exec::OnWorkers`, the lookups' block iterator made one for both);
  buffer pool frames and shards on cache lines of their own.  TPC-H Q20
  on column tables 577 → 173 ms, Q2 49 → 25, Q17 88 → 31; on row tables
  Q20 509 → 175.  DOCS §8.5.
- **2026-11-04** — §M14.20 W6: a small frame for `Expr::eval` (its
  less common nodes in `eval_other`): 1,680 → 480 bytes a node; TPC-H
  on column tables 3.92 → 3.75 s, row tables 8.4 → 8.0, ClickBench 2.47
  → 2.39.  DOCS §8.5.
- **2026-11-04** — §M14.20 W6: a column table's point reads — a
  segment's directory entry decoded once (cached by id), its row keys
  searched through marks first.  TPC-H Q19 40 → 37 ms, Q20 167 → 156.
  DOCS §8.15.
- **2026-11-04** — §M14.20 W6: one grouping table a worker across its
  morsels (groups ranked by morsel and place, finished in rank order;
  handed over per morsel when groups do not recur).  TPC-H Q13 on column
  tables 107 → 95 ms, on row tables 180 → 165.  DOCS §8.5.
- **2026-11-04** — §M14.20 W6: joins inside pipelines — a hash join
  probed as a step of the pipeline below it, its table made first, so
  joins over joins and the grouping above them run on the workers.
  TPC-H on column tables 3.8 → 2.3 s (Q9 882 → 243 ms, Q5 308 → 80), on
  row tables 8.0 → 7.2.  DOCS §8.5.
- **2026-11-04** — §M14.20 W6: `CASE` by batches (each `WHEN` over the
  rows left, each branch over the rows it takes); expressions still run a
  row at a time read only their columns.  ClickBench Q39 270 → 230 ms.
  DOCS §8.5.
- **2026-11-04** — §M14.20 W6: one key table for groupings and joins (a
  join's keys in `keytable`'s `IntKeys` / `KeyTable`, no key built a
  probe).  Speed unchanged.  DOCS §8.5.
- **2026-11-04** — §M14.20 W6: sorts on normalized keys (a row's keys
  one byte string, the index encoding's, compared by `memcmp`, when its
  key columns allow).  A whole sort of a million rows 430 → 370 ms;
  benchmarks unchanged.  W6 closed.  DOCS §8.5.
- **2026-11-05** — §M14.28 added: B+tree or LSM tree chosen per table,
  orthogonal to the row / column layout — a study first (workloads and
  their amplifications, the shape behind the KV contract, compaction,
  prior art), then the plan.
- **2026-11-05** — §M14.20 W7: row format version 3 — an offset every 16
  columns, text, `char(n)`, bytea and enums under 256 bytes with a
  one-byte length; versions 1 and 2 still read.  ClickBench on a row
  table 7.2 → 6.25 s (geometric mean 155 → 130 ms), its file 984 → 902
  MB; TPC-H on row tables 6.6 → 6.45 s.  DOCS §8.1.
- **2026-11-05** — §M14.20 W7: page reads without zeroing (each thread's
  read buffer kept, a page copied out once), and no row headers parsed
  for a scan reading no column.  ClickBench on a row table 6.5 → 6.1 s.
  DOCS §4.2.
- **2026-11-05** — §M14.20 W7: the data file grown ahead of its pages,
  by zeros an eighth of its length at a time (1–64 MiB).  The TPC-H load
  on row tables 56 → 27 s, on column tables 34 → 22; a logged `COPY` of
  `lineitem` 22 → 9 s.  W7's extents and shared WAL buffer measured and
  not taken (1–1.5 % and 1 % of the time).  DOCS §4.2.
- **2026-11-05** — §M14.20 W7: LZ blocks decoded by wild copies (LZ4's:
  fixed 16-byte pieces into room past the output's end), 2–2.5× the
  decoder before.  ClickBench on a column table 19.1 → 18.15 ms
  geometric mean, its `SELECT *` 133 → 116.  DOCS §4.4.
- **2026-11-05** — §M14.20 W7 closed: ClickBench on a row table 7.2 →
  5.9 s, the TPC-H load on row tables 56 → 27 s; extents and the shared
  WAL buffer measured and not taken.  W8 next.
- **2026-11-05** — §M14.20 W8 closed: a session's catalog, plans,
  parsed statements and statistics' keys kept by text; pipelining and a
  statement for many rows in every driver; one wire format crate; the
  next result batch made while the client reads one.  `SELECT 1` 28.3 →
  20.4 µs, a parameterized point read 52.1 → 25.8, 5.8 pipelined.
- **2026-11-05** — §M14.27, the study: who needs which statistics and
  how they get them today; variance methods measured (the textbook
  formula meaningless on offset data, PostgreSQL's three digits short,
  Welford a different last digit for every split among workers, exact
  power sums the same bits always, 8.9 ns a value by a superaccumulator);
  the families chosen with their methods, bounds and prior art; steps
  S1–S8, to be agreed before they are built.
- **2026-11-06** — §M14.28, the study: write amplification measured
  against PostgreSQL 18 (`bench/wamp/wamp.py`): bulk loads cheaper in
  nucleus (4–6 against 5–11); a commit 16 KiB against 8 (the WAL's
  growing segments — step L0); random keys into a table twenty times the
  cache about 29 in both; the design behind the KV contract (one WAL,
  engines per key range), lazy leveling chosen, prior art; steps L0–L5,
  to be agreed before they are built.
- **2026-11-06** — §M14.12 V1: `vector(n)` as a registered type with
  every capability (equality, hashing, order, key encoding, distances
  `<-> <=> <#> <+>`, arithmetic, a modifier, casts, exact `sum` / `avg`);
  distance kernels with a fixed summation order, the same bits at every
  SIMD level; exact nearest-neighbour search; a native mapping in all six
  drivers; the law kit passes on it.
- **2026-11-06** — §M14.28 L0: the commit's bytes — `fdatasync` on Linux,
  WAL segments grown ahead by zeros and recycled where the file system
  writes in place (not APFS, measured: an overwrite costs it more than an
  append); on ext4 a one-row commit 16.6 → 4.4 KiB and ~4× faster; a
  checkpoint's segment drop made safe against a crash before the new
  segment's entry is durable.  Steps S, L and V agreed to be built.
- **2026-11-06** — §M14.12 V2: the exact search bound by bandwidth —
  values out of line read beside a scan's rows (`large::Chunks`), type
  lookups kept a thread, the server's `--cache`; 100k × 768 nearest
  neighbours 233 → 48 ms (numpy 57, pgvector exact 167).
- **2026-11-06** — §M14.12 V2b: `x[i]`, `x[lo:hi]`, `SET x[i] = v` for
  arrays and vectors through one capability (`Elements`); vectors as
  `(n, d)` numpy and Arrow `fixed_size_list<float32>` in Python;
  pg-compat's `vector` (oid 13201, `to_regtype`, pgvector's binary),
  pgvector's psycopg client working.
- **2026-11-06** — §M14.20 ✅: the full runs against PostgreSQL meet
  every target — TPC-H 0.25× / 0.77×, ClickBench 0.11× / 0.89×, loads
  0.55–1.1×, `bench/run.py` within its margins.
- **2026-11-06** — §M14.27 S1: one exact accumulator — Neal's
  superaccumulator in a window of chunks, rounded from the integer,
  exact subtraction and products; the split law tested at 1–32 workers.
- **2026-11-06** — §M14.27 S2: moments — `variance`, `stddev` (and
  their `_samp` / `_pop` names), `skewness`, `kurtosis`; PostgreSQL's
  digits over integers and decimals, the exact value rounded once over
  doubles; `ExactSum`'s state smaller (`avg` by a million groups 3.2 →
  2.0 s).
- **2026-11-06** — §M14.27 S3: two variables — `corr`, `covar_*`,
  `regr_*` as PostgreSQL's, from exact sums rounded once; exact products
  in one step, Knuth's long division.
- **2026-11-06** — §M14.27 S4: window functions — `OVER`, `WINDOW`,
  `ROWS` / `RANGE` / `GROUPS` frames with `EXCLUDE`, every aggregate
  over its frame (a running state finished in place, small frames fed
  directly, a segment tree of partial states), the ranking and value
  functions, PostgreSQL's semantics and errors; a `Sort` per window,
  the final `ORDER BY` left out when the last window gives it; `avg`
  of doubles rounded once.
- **2026-11-06** — §M14.27 S6: sketches — `approx_count_distinct`
  (HyperLogLog++, Ertl's estimator), `approx_percentile` (DDSketch),
  `approx_top_k` (Misra–Gries), the `hll` and `ddsketch` types; a
  stable hash (xxHash64 of the key encoding); a sketch the same bytes
  in every run and executor.
- **2026-11-06** — §M14.27 S5: ordered-set aggregates — exact
  `percentile_cont` / `percentile_disc` / `mode` `WITHIN GROUP`,
  `median`, by selection over order-preserving keys, combinable, a
  radix-partitioned spill past `work_mem` (3–4× PostgreSQL's);
  `width_bucket`, `histogram`; constant expressions once per batch.
- **2026-11-06** — §M14.27 S4·sort: one sorter for every full sort —
  normalized keys with an abbreviated prefix, chunks in memory or in
  indexed scratch runs, a merge split into key ranges on the workers
  (`ORDER BY` 2–4× faster at 10M rows, past PostgreSQL's); a `Sort` a
  primary key's scan gives left out; unfiltered chains on the workers,
  a `LIMIT`'s bound down to the scan.
- **2026-11-06** — §M14.27 S7: sampling and splits — `TABLESAMPLE`
  (`BERNOULLI`, `SYSTEM`, `REPEATABLE` by the primary key's hash) on the
  workers, `hash_unit`, `random()`, `reservoir_sample` (Algorithm L, one
  reservoir with `ANALYZE`'s, merged hypergeometrically).
- **2026-11-06** — §M14.27 S8: drift tests — `ks_test`, `auc`, `psi`
  over the values in order, `js_divergence`, `chisq_test` over their
  counts, scipy's and scikit-learn's answers; `SUMMARIZE` and the MCP
  tool `summarize_table`.
- **2026-11-06** — §M14.12 V3: conditions the statistics cannot estimate
  (a distance bound, `LIKE`, functions of columns) counted on up to
  1,000 rows `ANALYZE` keeps; join orders without cross products while
  conditions connect the chain.
- **2026-11-06** — §M14.12: adaptive joins — an inner hash join whose
  left side an index could look up chooses the lookups as it runs when
  its right side has fewer rows than where the two cost the same
  (Oracle's adaptive plans, SQL Server's adaptive joins).
- **2026-11-07** — §M14.12 V4: the HNSW index's storage decided by
  measurement — a shared graph (pgvector's), segments (Lucene's) and the
  combined model as prototypes on the real KV (`bench/ann/`, SIFT-1M and
  GloVe, 10 and 1 threads): the combined one (one graph in content-hashed
  pages, a delta in epochs folded in batches sized by measured costs, an
  economic order quantity) searches 3–5× the others and pgvector's at
  the same recall, inserts 3× pgvector's; deletes and updates checked
  against the table and cut out by FreshDiskANN's consolidation.
- **2026-11-14** — The order after §M14.24, agreed: §M14.15 JSON, then
  §M14.13 branches, §M14.17 easy start, §M14.16 migrations, §M14.18
  operations with §M14.29 S5 (Elle) beside it and S3's files (OSS-Fuzz)
  made ready; §M14.28's LSM when a write-heavy load asks for it.  S3's
  submission and S4's audit are the owner's to request.
- **2026-11-14** — Found while measuring §M14.15 J1, fixed: a table
  dropped after a bulk `CREATE TABLE AS` came back, empty, once later
  commits rewrote its catalog leaf.  A commit's leaf filter (`Collect`)
  dropped a key's newest tombstone at or below the watermark as hiding
  nothing; but a large transaction's committed intent of the key, not
  yet made a version by vacuum, sorts before the versions and had
  committed before the tombstone — dropped, the intent showed again.
  DOCS.md's claim that a commit's collection need not mind intents was
  wrong.  Now, while committed intents are pending, a run's tombstone
  stays when the run has an intent or begins the leaf (its intent may
  be in the leaf before) — vacuum's rule for keys with intents.
