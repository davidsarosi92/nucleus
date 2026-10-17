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
| §F0 – §F4 | Phases and milestones in detail (the developer platform, §M14.9–§M14.19, sits between F1 and F2) |
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
§M14.19 comes next); and every milestone's design is checked against them
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
| D15 | Versioning (open) | Proposed: one semantic version for the server, CLI and Rust crates (`Cargo.toml`); each **format** versioned on its own and checked on open — the on-disk store (pager header), the catalog records (table definitions, already versioned), the row encoding, the WAL, and the client protocol (`ncp`'s Hello version, negotiated); drivers versioned per language but tied to the protocol version they speak; upgrades read older formats and refuse newer ones, and a release note lists format changes | to decide before the first tagged release and before publishing drivers |

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
| | M14.19 | **Next:** modularity — registries, extensible and evolvable types, an extension API (principle 12) | ⬜ |
| | M14.9 | SQL analyzer: `EXPLAIN ANALYZE`, statement statistics, linter, index advisor | ⬜ |
| | M14.10 | AI access: MCP server, schema comments, hints in errors | ⬜ |
| | M14.11 | Large data foundations: streaming results, bulk load, spilling, parallel scans, benchmarks | ⬜ |
| | M14.12 | Vectors: `VECTOR(n)`, similarity search, then an ANN index | ⬜ |
| | M14.13 | Database branches (copy-on-write) | ⬜ |
| | M14.14 | Time travel: `AS OF` queries | ⬜ |
| | M14.15 | JSON type and functions | ⬜ |
| | M14.16 | Migrations and code generation | ⬜ |
| | M14.17 | Easy start: packages, Docker, web console, browser playground | ⬜ |
| | M14.18 | Operations: backups and point-in-time restore, metrics and tracing, TLS, roles and privileges | ⬜ |
| **F2 — NoSQL** | M15 | Document model | ⬜ |
| | M16 | Document queries (shared executor) + document server | ⬜ |
| | M16.5 | Streams: a transactional broker, changefeeds (CDC) | ⬜ |
| | M16.6 | Live queries: subscriptions to query results | ⬜ |
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

### Developer platform (§M14.9–§M14.19)

What makes nucleus worth choosing, before more data models: understanding
queries, being usable by AI models, holding large data, vectors — and the
things nucleus's design gives almost for free (branches and time travel
from copy-on-write storage and MVCC), the everyday tools (JSON,
migrations, code generation), an easy start, and what production needs.

### §M14.19 — Modularity: registries and an extension API ⬜ (next)

**Order:** this comes first in the developer platform — principles before
features — then §M14.9 onward.  It starts with a **principle audit**: the
code checked against all twelve principles, each gap listed here with its
fix.

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
- The front ends share one session layer, so a new protocol is a module
  over `Session` (as `ncp` and `--pg-compat` already are); the server
  starts the modules a configuration names.
- An extension API for out-of-tree modules (Rust crates linked into a
  custom build first; loadable modules later), with DOCS for extension
  authors and a sample extension in `examples/`.
- A boundary check in CI: core crates do not depend on modules.

**Done when:** the principle audit's gaps are closed or scheduled; the
built-in functions, types, catalog views, front ends and authentication
are all registered through the public extension API; the sample extension
adds a function, a type and a catalog view without touching the core; and
a type's encoding is upgraded on a populated database (old and new values
read side by side, crash-tested) and a column's type changed online.

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
- Bulk load: `COPY`-style loading (text/CSV and `ncp` batches) and batched
  inserts in the drivers, bypassing per-row statement overhead.
- A memory-bounded executor: sorts, hash joins and aggregates spill to
  temporary files past a budget.
- Parallel scans over key ranges; page compression.
- Published benchmarks (load, point reads, scans, joins, mixed) with
  targets, run in CI to catch regressions.

Columnar storage for analytics and range partitioning (§M18) build on this.

**Done when:** a 100-million-row table loads, scans, sorts and joins within
a fixed memory budget, and the benchmark targets are met.

### §M14.12 — Vectors ⬜

Embeddings beside the data they describe, in the same transactions:

- A `VECTOR(n)` type (float32 components), literals and casts, wire and
  driver support.
- Distance operators and functions — cosine, L2, inner product — and exact
  nearest-neighbour search with `ORDER BY distance LIMIT k`.
- An approximate nearest-neighbour index (HNSW) the optimizer uses for
  `ORDER BY … LIMIT k`, kept transactionally consistent with the table.

**Done when:** exact search matches a brute-force reference, and the ANN
index reaches a set recall at a set latency on a public dataset.

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
- Metrics (Prometheus endpoint) and tracing (OpenTelemetry): commits,
  conflicts, cache, WAL, statement latencies.
- TLS for `ncp` (and the PostgreSQL protocol), password hashing (SCRAM),
  users and roles with `GRANT` / `REVOKE` on tables and columns.
- ✅ An exclusive lock on the data directory, so a second process cannot
  open it (done first, 2026-10-17: DOCS §4.6).

**Done when:** a restore reproduces the database at a chosen moment,
privileges are enforced in slt tests, and TLS works with every driver.

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

**Done when:** exactly-once processing (consume, write, commit) survives
crash tests, and changefeeds deliver every committed change in order.

### §M16.6 — Live queries ⬜

A client subscribes to a query and receives its changes after every
commit that affects it — built on the changefeeds (§M16.5).

- `ncp` subscription messages; driver APIs (`subscribe(sql, params)` →
  initial rows, then inserts / updates / deletes).
- Incremental maintenance where the plan allows (filters, projections,
  joins on keys), re-evaluation otherwise; a limit on subscriptions.

**Done when:** subscribers see exactly the query's result after each
commit, under concurrent writes, in every driver.

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
