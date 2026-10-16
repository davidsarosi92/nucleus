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
10. **Portable and frugal.**  Every target in the table below builds from
    this repo (see "Build targets").  32-bit is included: no assumption of a
    64-bit `usize` or of mapping large files.  The real platform layer uses
    only plain file operations (on POSIX: `open`, `pread`, `pwrite`, `fsync`,
    `rename`, `ftruncate`; no `io_uring`, no `O_DIRECT`, `mmap` optional),
    with one implementation per OS family behind the platform interface.
    Memory use (buffer pool, caches) is bounded and configurable.

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
| D4 | SQL dialect | ✅ **PostgreSQL-like** subset | together with the Postgres wire protocol this gives free clients (psql, drivers) |
| D5 | Isolation | ✅ **Snapshot Isolation** first, **Serializable (SSI)** later | — |
| D6 | File layout | ✅ **One data file + one WAL directory** per database | single file (SQLite-style) |
| D7 | License | ✅ **MIT** (came with the GitHub repo) | — |
| D8 | Document client protocol (needed by M16) | **MongoDB wire protocol** subset — existing drivers and tools work, as the Postgres wire protocol does for SQL | own protocol (binary or HTTP/JSON); decide at the end of M15 |
| D9 | Where release builds are published | **GitHub Releases** attached to tags, built by CI | a separate artifact store; not in this repo either way |
| D10 | Storage engine shape | ✅ **Copy-on-write B+tree + logical WAL** (2026-10-17): checkpoints are atomic pager commits of a COW tree; the WAL holds batches of key writes; recovery = replay after the checkpoint; no undo | ARIES-style physiological logging with in-place updates — more write-efficient, far more complex; revisit only if checkpoint write amplification hurts |
| D11 | Tree concurrency | ✅ **One writer or many readers** per tree (`RwLock`); MVCC scans re-seek between batches (2026-10-17) | latch crabbing / B-link trees, when benchmarks show writer contention |
| D12 | Serializable isolation (§M14) | **Read-set validation** (FoundationDB style): a commit fails if a key or range it read was written after its snapshot | Cahill SSI (fewer aborts, much more state); FDB's approach also distributes in F3 |

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
- **2026-10-16** — build targets: Linux, macOS, Windows, d-os; per-OS
  durability notes; artifacts are not stored in the repo (D9).
- **2026-10-16** — packaging and install per platform, including the d-os
  app format (`app install`, `.dos-app` manifest); first packages in M6.
- **2026-10-17** — §M0 shipped (DOCS §1–§3); CI green on all nine jobs.
- **2026-10-17** — D2–D6 settled as proposed; page size 8 KiB (M1).
- **2026-10-17** — §M1 shipped (DOCS §4); CI green on all nine jobs.
- **2026-10-17** — §M6.5 nucleus side done (settings service, protocol,
  packaging); the d-os side remains.
- **2026-10-17** — §M6 shipped: contract, CLI, packaging, release workflow;
  the d-os archive is built and checked in CI, not yet run on d-os.
- **2026-10-17** — D10–D12 recorded; §M1 revised (copy-on-write rule, free
  list in dedicated pages, no leaks); §M2–§M5 shipped.
