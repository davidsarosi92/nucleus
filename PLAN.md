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
| Browser (from §M14.23) | `wasm32-unknown-unknown` | the embedded edition only: no threads or blocking file API by default — storage through OPFS (or in memory), its own platform implementation |
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
| D2 | First storage engine | ✅ **B+tree** (copy-on-write, or in-place + WAL) | LSM tree: better for writes, but compaction adds complexity; can come later as a second engine behind the KV contract |
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
| D15 | Versioning (open) | Proposed: one semantic version for the server, CLI and Rust crates (`Cargo.toml`); each **format** versioned on its own and checked on open — the on-disk store (pager header), the catalog records (table definitions, already versioned), the row encoding, the WAL, and the client protocol (`ncp`'s Hello version, negotiated); drivers versioned per language but tied to the protocol version they speak; upgrades read older formats and refuse newer ones, and a release note lists format changes | to decide before the first tagged release, before publishing drivers, before a module leaves this repo (§M14.19) and before an embedded edition ships a file format to devices (§M14.23) |

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
| | M14.26 | **In progress:** core types: integers by width (✅), `real`, `bytea`, `time`, `char(n)`, `inet` / `cidr`, enums | ⬜ |
| | M14.25 | Identity and access: users and roles, privileges on every object, authentication modules (SCRAM, tokens, certificates, OIDC), access rules by connection context, row-level security, audit | ⬜ |
| | M14.9 | SQL analyzer: `EXPLAIN ANALYZE`, statement statistics, linter, index advisor | ⬜ |
| | M14.10 | AI access: MCP server, schema comments, hints in errors | ⬜ |
| | M14.11 | Large data foundations: streaming and columnar results (zero-copy into numpy / Arrow), bulk load, spilling, parallel scans, benchmarks against PostgreSQL | ⬜ |
| | M14.20 | Analytics: vectorized execution, SIMD kernels as a type capability, table partitioning, row / column / both storage chosen per table, column and partition (HTAP) | ⬜ |
| | M14.12 | Vectors: `VECTOR(n)` with every capability, similarity search, an ANN index, filtered vector search | ⬜ |
| | M14.21 | Full-text and hybrid search: BM25, analyzers as modules, text + vector + filters in one query | ⬜ |
| | M14.22 | Embeddings in the database: `embed()`, embedding columns kept current in the background, chunking | ⬜ |
| | M14.23 | Embedded editions: in memory or on a file inside Python and JavaScript, the browser (WASM), iOS and Android | ⬜ |
| | M14.24 | Memory for AI models: knowledge that is updatable, erasable, permissioned and auditable, read at generation time | ⬜ |
| | M14.13 | Database branches (copy-on-write) | ⬜ |
| | M14.14 | Time travel: `AS OF` queries | ⬜ |
| | M14.15 | JSON type and functions | ⬜ |
| | M14.16 | Migrations and code generation | ⬜ |
| | M14.17 | Easy start: packages, Docker, web console, browser playground | ⬜ |
| | M14.18 | Operations: full and incremental backups to local or object storage, point-in-time restore, automatic restore tests, metrics and tracing, health checks, alert rules | ⬜ |
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
(Publishing them to crates.io, PyPI, npm and Maven Central waits for D15.)

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
| 10 | memory is bounded in the core (buffer pool), not in the executor: sorts, hash joins, hash aggregation and materialised CTEs hold their whole input | scheduled: spilling in §M14.11 | — |
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

### §M14.26 — Core types ⬜ (in progress)

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
- **`real`** (32-bit float) beside `double precision`, with PostgreSQL's
  output rules for each.
- **`bytea`**: binary strings — hex and escape text forms
  (`'\xdeadbeef'`), `||`, `substring`, `length`, `position`, `encode` /
  `decode` (hex, base64), digests (`sha256`, `md5`); byte-wise order and
  a key encoding; native types in every driver (`Vec<u8>`, `bytes`,
  `Uint8Array`, `byte[]`, pointer and length in C).  Values larger than a
  page are stored out of line in chunks, so a column can hold files and
  model artefacts.
- **`time`** (time of day, microseconds), with `date + time`, intervals,
  `extract` and `date_trunc`.
- **`char(n)`** with the standard's blank padding and comparison rules.
- **`inet` and `cidr`**: IPv4 and IPv6 addresses and networks;
  containment operators (`<<`, `<<=`, `>>`, `>>=`, `&&`), `host`,
  `network`, `masklen`, `family`, `broadcast`; an order and key encoding
  in which a network's addresses are one contiguous range, so containment
  is an index range scan.  The access rules (§M14.25) and the audit log
  use them.
- **Enums**: `CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')`, ordered
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

### §M14.25 — Identity and access ⬜

**Order:** right after §M14.19 and §M14.26 (whose `inet` / `cidr` the
access rules use), before the rest of the developer platform:
much of it builds on roles and row-level security — the MCP server's
write access (§M14.10), filtered vector search (§M14.12), hybrid search
(§M14.21), memory for models (§M14.24), sync (§M16.7).

Today there is one user and one password, given on the server's command
line; `ncp` sends the password in the clear (there is no TLS yet), and
`--no-auth` admits anyone.  Fine on `127.0.0.1` for development, not
beyond it.

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

### §M14.9 — SQL analyzer ⬜

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

### §M14.10 — AI access ⬜

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

### §M14.11 — Large data foundations ⬜

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

### §M14.20 — Analytics: vectorized execution and columnar storage ⬜

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
  across cores (morsel-driven), building on §M14.11's parallel scans.
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

### §M14.12 — Vectors ⬜

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

**Done when:** the law kit passes on `vector`; every capability above
works through SQL, `ncp` and all six drivers; exact search matches a
brute-force reference; the ANN index reaches a set recall at a set
latency on a public dataset; and filtered searches keep that recall at
filter selectivities from 50 % down to 0.01 %, always returning `k` rows
when `k` matching rows exist.

### §M14.21 — Full-text and hybrid search ⬜

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

### §M14.22 — Embeddings in the database ⬜

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

### §M14.23 — Embedded editions ⬜

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

### §M14.24 — Memory for AI models ⬜

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

### §M14.14 — Time travel ⬜

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

### §M14.15 — JSON ⬜

- A `JSON` type (validated, stored in a compact binary form), operators
  `->`, `->>`, `#>`, `@>`, `?`, functions to build, extract, test and
  aggregate (`json_build_object`, `json_agg`, `jsonb_path`-style paths).
- Indexes on JSON paths (expression indexes), and a containment index.
- The bridge to the document model (§M15): documents are JSON values.

**Done when:** an slt script checked against PostgreSQL's `jsonb` covers
the operators and functions.

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

- Online backups (a checkpoint's pages are immutable: a consistent copy
  without stopping writes), and point-in-time restore from backups plus
  the WAL.
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
