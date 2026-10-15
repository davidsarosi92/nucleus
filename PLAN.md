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
| Decisions | What has to be settled before the first line of code |
| Status snapshot | Phases + milestones table |
| §F0 – §F4 | Phases and milestones in detail |
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
  (from F0).
- **Standalone SQL server** — used like a plain PostgreSQL: start it, connect
  with `psql` or any Postgres driver (from M12).
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
│  Interfaces: embedded API · CLI/REPL · Postgres wire protocol    │  F1+
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
10. **Portable and frugal.**  The core builds and runs as a static binary on
    `x86_64`, `aarch64` and `i686` `unknown-linux-musl` (32-bit included: no
    assumption of a 64-bit `usize` or of mapping large files).  The real
    platform layer uses only plain syscalls (`open`, `pread`, `pwrite`,
    `fsync`, `rename`, `ftruncate`; no `io_uring`, no `O_DIRECT`, `mmap`
    optional).  Memory use (buffer pool, caches) is bounded and configurable.

## Decisions

These have to be settled before §F0/M0.  Each has a proposal; settled ones
are marked ✅.

| # | Question | Proposal | Alternatives / notes |
|---|----------|----------|----------------------|
| D1 | Language | ✅ **Rust** (settled 2026-10-16) — memory safety + concurrency (buffer pool, MVCC, Raft) | rejected: C, Zig, Go |
| D2 | First storage engine | **B+tree** (copy-on-write, or in-place + WAL) | LSM tree: better for writes, but compaction adds complexity; can come later as a second engine behind the KV contract |
| D3 | Concurrency model | **Threads + locks** in the engine, async only in the network layer | thread-per-core (shared-nothing) — revisit in phase 3 |
| D4 | SQL dialect | **PostgreSQL-like** subset | together with the Postgres wire protocol this gives free clients (psql, drivers) |
| D5 | Isolation | **Snapshot Isolation** first, **Serializable (SSI)** later | — |
| D6 | File layout | **One data file + one WAL directory** per database | single file (SQLite-style) |
| D7 | License | ✅ **MIT** (came with the GitHub repo) | — |
| D8 | Document client protocol (needed by M16) | **MongoDB wire protocol** subset — existing drivers and tools work, as the Postgres wire protocol does for SQL | own protocol (binary or HTTP/JSON); decide at the end of M15 |

---

## Status snapshot

| Phase | Milestone | Goal | Status |
|-------|-----------|------|:------:|
| **F0 — Core** | M0 | Project skeleton, build, test harness, simulated platform | ⬜ |
| | M1 | Pager + on-disk format | ⬜ |
| | M2 | Buffer pool | ⬜ |
| | M3 | B+tree | ⬜ |
| | M4 | WAL + recovery | ⬜ |
| | M5 | Transactions + MVCC | ⬜ |
| | M6 | Stabilise the KV contract + embedded API + CLI | ⬜ |
| | M6.5 | First embedded consumer: d-os settings store | ⬜ |
| **F1 — SQL** | M7 | Key encoding + catalog | ⬜ |
| | M8 | SQL parser + AST | ⬜ |
| | M9 | Binder, planner, Volcano executor | ⬜ |
| | M10 | Joins, aggregation, secondary indexes | ⬜ |
| | M11 | SQL transactions + constraints | ⬜ |
| | M12 | Server + Postgres wire protocol | ⬜ |
| | M13 | Cost-based optimizer | ⬜ |
| | M14 | Serializable (SSI) | ⬜ |
| **F2 — NoSQL** | M15 | Document model | ⬜ |
| | M16 | Document queries (shared executor) + document server | ⬜ |
| | M17 | Further models (decide later) | ⏸ |
| **F3 — Distributed** | M18 | Range partitioning (still single-node) | ⬜ |
| | M19 | Per-range Raft replication | ⬜ |
| | M20 | Cluster membership + placement | ⬜ |
| | M21 | Distributed transactions (HLC) | ⬜ |
| | M22 | Distributed query execution | ⬜ |
| | M23 | Rebalancing, fault tolerance, Jepsen-style tests | ⬜ |
| **F4 — Above** | M24 | Federation / connectors | ⬜ |
| | M25 | CDC / changefeeds, multi-region | ⏸ |

---

## §F0 — The core (transactional ordered KV)

The goal is an embeddable library that stores ordered key-value pairs
durably, crash-safely and transactionally.  This is the most important part
of the system — every later phase uses it.

### §M0 — Project skeleton ⬜

- Cargo workspace, one crate per layer; `rustfmt` + `clippy`, GitHub Actions CI.
- CI also cross-builds for the three musl targets of principle 10, so a
  portability regression fails the build the day it is introduced.
- Module boundaries following the architecture diagram (`platform`,
  `storage`, `txn`, `kv`, later `sql`, `doc`, `server`).
- **Platform interface** (file, time, randomness) with two implementations:
  real OS and simulated (in-memory, fault-injecting, deterministic from a seed).
- Property-based testing infrastructure.

**Done when:** `cargo build` + `cargo test` are green in CI; a sample test
runs on the simulated platform and is reproducible from a seed; a DOCS.md
skeleton exists.

### §M1 — Pager + on-disk format ⬜

- Fixed-size pages (4 KiB or 8 KiB — decided in M1), page id = offset.
- File header: magic, format version, page size, root page pointers.
- Every page carries a checksum + page type + LSN field (prepared for the WAL).
- Free-page management (free list).

**Done when:** pages can be written and read; a bad checksum yields a
detected error; a torn write is recognised on the simulated platform.

### §M2 — Buffer pool ⬜

- Fixed-capacity page cache, pin/unpin, dirty tracking.
- Eviction: clock or LRU-K.
- Thread-safe; page-level latches (reader/writer).
- Prepare the WAL rule: a dirty page may only be written out once its WAL
  records are durable (enforced from M4).

**Done when:** a concurrent stress test runs without data loss or deadlock;
a pinned page is never evicted.

### §M3 — B+tree ⬜

- Variable-length keys and values; overflow pages for large values.
- Insert, delete, point lookup, **range-scan iterator** (forward/backward).
- Page split and merge.
- Concurrent access: latch crabbing (or a B-link tree).

**Done when:** a property test against a reference `BTreeMap` agrees over a
million random operations; an invariant checker (ordering, fill factor,
parent/child keys) runs at the end of every test.

### §M4 — WAL + recovery ⬜

- Write-ahead log: LSNs, per-record checksum, group commit.
- Physiological logging; ARIES-style redo (+ undo if M5 requires it).
- Checkpoints, WAL truncation.
- Recovery on startup.

**Done when:** the simulation test injects a crash at **every** I/O point,
restarts, and checks that every committed transaction is present, nothing
uncommitted is visible, and the tree invariants hold.

### §M5 — Transactions + MVCC ⬜

- Versioned values: key + commit timestamp (the timestamp is an abstracted
  type — see principle 3).
- Snapshot Isolation: reads from a snapshot, write-write conflict detection
  (first-committer-wins).
- Garbage collection of old versions (GC / vacuum) based on the oldest
  active snapshot.
- Transaction API: `begin` → `get/put/delete/scan` → `commit` / `abort`.

**Done when:** anomaly tests (dirty read, lost update, non-repeatable read)
never occur under SI; write skew *can* occur and is documented (M14 fixes
it); GC never removes a version that is still visible.

### §M6 — Stabilise the KV contract ⬜

- Finalise the KV API — **this is the interface the F3 distributed
  implementation will also implement**, so it gets thought through here:
  range operations, batches, snapshot reads at a given timestamp, error
  types (e.g. retryable conflict vs. permanent failure).
- Public embedded library API + documentation.
- Small CLI/REPL for KV commands (`put`, `get`, `scan`, `begin`, `commit`).
- First benchmark baseline.

**Done when:** the KV API sits behind a trait that F1 code uses without
knowing any concrete type; DOCS.md describes it.

### §M6.5 — First embedded consumer: d-os settings store ⬜

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
  supervisor) built on the nucleus **embedded API only** (principle 9).  Its
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

**The d-os switch-over.**  d-os moves its settings to nucleus once the "done
when" list above is green; the d-os PLAN tracks the switch on its side.
Until then `d-os.conf` remains the d-os default.

**Beyond settings.**  The same deployment modes as anywhere else: once M12
ships, programs on d-os can use nucleus as a Postgres-compatible system
database; once M16 ships, as a document store.  No d-os-specific server code
— d-os gets the same binaries as any other platform.

---

## §F1 — SQL layer

### §M7 — Key encoding + catalog ⬜

- Memcomparable encoding for every type (integer, float, string, bool, NULL,
  composite keys); DESC ordering support.
- Keyspace layout: `/<table_id>/<index_id>/<key columns>` → row.
- Catalog (tables, columns, types, indexes) **in the KV space**, under a
  system prefix.
- Schema versioning (online schema changes come later, but the version field
  exists now).

**Done when:** encoding round-trip and order preservation are property-tested.

### §M8 — SQL parser + AST ⬜

- Hand-written recursive-descent (or Pratt) parser with good error messages.
- Initial subset: `CREATE TABLE`, `DROP TABLE`, `INSERT`, `SELECT ... WHERE`,
  `UPDATE`, `DELETE`.

### §M9 — Binder, planner, executor ⬜

- Binder: name resolution, type checking.
- Logical plan → physical plan (rule-based for now).
- Volcano (iterator) executor: scan, filter, project, sort, limit.
- `EXPLAIN`.

**Done when:** a basic SQL logic test suite (sqllogictest format) is green.

### §M10 — Joins, aggregation, indexes ⬜

- Nested loop, hash join, (merge join).
- `GROUP BY`, aggregates, `HAVING`, `ORDER BY`, `DISTINCT`.
- Secondary indexes, index-scan selection.

### §M11 — SQL transactions + constraints ⬜

- `BEGIN` / `COMMIT` / `ROLLBACK`, autocommit.
- `PRIMARY KEY`, `UNIQUE`, `NOT NULL`, `CHECK`; `FOREIGN KEY` later.

### §M12 — Server + Postgres wire protocol ⬜

- Network server, connection handling, session state.
- PostgreSQL frontend/backend protocol (simple + extended query).

**Done when:** `psql` connects and the M9–M11 test suite runs through it.

### §M13 — Cost-based optimizer ⬜

- Statistics (row count, histograms, NDV), `ANALYZE`.
- Join ordering (DP or Cascades-style search).

### §M14 — Serializable (SSI) ⬜

- Serializable Snapshot Isolation (tracking read-write dependencies).
- The write-skew test that "failed as documented" in M5 is green here.

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
- **Document server:** the M12 server also speaks the document client
  protocol (decision D8), so nucleus can be used like a NoSQL database.

**Done when:** an existing client for the D8 protocol (e.g. `mongosh` and a
standard driver, if D8 lands on the MongoDB wire protocol) runs CRUD and
queries against nucleus.

### §M17 — Further models ⏸

Decided later: wide-column, time series, graph, vector.  All follow the same
pattern: a mapping onto the ordered KV + operators in the shared executor.

---

## §F3 — Distributed layer

A distributed implementation of the KV contract.  F1/F2 code is unchanged.

### §M18 — Range partitioning (single node) ⬜

- Split the keyspace into contiguous ranges; range descriptors live in the
  KV (meta range).
- Range split by size/load, range merge.
- Still a single process — the goal is to finish the routing logic before
  the network comes in.

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

### §M25 — CDC, multi-region ⏸

- Changefeeds (from the WAL / MVCC versions), multi-region placement,
  follower reads.

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
