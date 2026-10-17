# nucleus — documentation

> **What this file is:** the *current state* — every component that exists,
> its API and its quirks.  The roadmap is [PLAN.md](PLAN.md); when a
> milestone ships, its design notes move here.

## Contents

| § | Section |
|---|---------|
| 1 | Repository layout |
| 2 | Building and testing |
| 3 | Platform layer (`nucleus-platform`) |
| 4 | Storage layer (`nucleus-storage`) |
| 5 | Transaction layer (`nucleus-txn`) |
| 6 | KV contract (`nucleus-kv`) and the CLI |
| 7 | d-os settings store (`integrations/d-os`) |
| 8 | SQL layer (`nucleus-sql`) |
| 9 | Server (`nucleus-server`) |
| 10 | Change log |

---

## 1. Repository layout

| Path | Crate | Contents | Milestone |
|------|-------|----------|-----------|
| `crates/platform` | `nucleus-platform` | file system, clock and randomness traits; real and simulated implementations; simulation harness | §M0 |
| `crates/storage` | `nucleus-storage` | page format, pager, buffer pool, B+tree, WAL, store | §M1–§M4 ✅ |
| `crates/txn` | `nucleus-txn` | MVCC, snapshot isolation, GC | §M5 ✅ |
| `crates/kv` | `nucleus-kv` | the KV contract and the local `Database` | §M6 ✅ |
| `crates/sql` | `nucleus-sql` | types, encodings, catalog, parser, binder, optimizer, executor, sessions | §M7–§M14 ✅ |
| `crates/server` | `nucleus-server` | the PostgreSQL wire protocol | §M12 ✅ |
| `crates/cli` | `nucleus-cli` (binary `nucleus`) | `nucleus kv`, `nucleus sql`, `nucleus server` | §M6, §M12 ✅ |
| `integrations/d-os` | `nucleus-dos-settings` (binary `nucleus-settings`) | the d-os settings service and app manifest | §M6, §M6.5 ◐ |
| `scripts` | — | `package.sh` | §M6 |

Workspace-wide: Rust edition 2024, toolchain pinned in `rust-toolchain.toml`
(rustup installs it on first use), `unsafe_code` forbidden, `missing_docs`
warned, clippy warnings are errors in CI.

## 2. Building and testing

```sh
cargo test --workspace                           # everything
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo build --target i686-unknown-linux-musl     # any target in PLAN.md "Build targets"
```

CI (`.github/workflows/ci.yml`) runs fmt and clippy, then builds every build
target and runs the tests on all of them except `x86_64-apple-darwin` (built
only).  The `i686` musl tests run as 32-bit binaries on a 64-bit Linux
kernel, which is what catches 64-bit `usize` assumptions.

**Simulation tests** take their seeds from `harness::run` (§3.4).  The test
profile builds with `opt-level = 1` (debug assertions on), because the crash
sweeps replay workloads thousands of times:

```sh
NUCLEUS_SEED=0xf4a13e02e1757317 cargo test crash_keeps   # rerun one failing seed
NUCLEUS_SIM_CASES=10000 cargo test -p nucleus-platform   # search more seeds
```

## 3. Platform layer (`nucleus-platform`)

The only way any nucleus code reaches files, time or randomness (PLAN.md
principle 4).

### 3.1 Interface

- `Platform` — `fs()`, `clock()`, `rng()`.
- `FileSystem` — `open(path, OpenMode)`, `remove_file`, `rename` (atomic
  replace), `create_dir_all`, `list_dir` (sorted names), `exists`,
  `sync_dir`.
- `File` — positional only, no cursor, all `&self` (shareable between
  threads): `read_at`, `write_at`, `read_exact_at`, `write_all_at`, `sync`,
  `size`, `set_len`.
- `OpenMode` — `ReadOnly`, `ReadWrite`, `Create`, `CreateNew`; none
  truncates.
- `Clock` — `now()` monotonic since platform creation, `wall()`.
- `Rng` — `next_u64`, `below`, `chance_ppm`, `fill_bytes`.  **Not
  cryptographic.**  `SplitMix64` is the implementation everywhere.

**Durability rule** (enforced by the simulation): a write is visible at once
but durable only after `File::sync`; a directory entry change (create,
remove, rename) is durable only after `sync_dir` of its parent.  **A failed
`sync` means the unsynced writes are lost**, even if a later `sync`
succeeds — callers must treat it as fatal for that data.

### 3.2 Real platform (`real::RealPlatform`)

`std` only.  `sync` = `File::sync_all`, which std implements as `fsync` on
Linux/d-os, `fcntl(F_FULLFSYNC)` on macOS (verified in std's
`sys/fs/unix.rs`) and `FlushFileBuffers` on Windows.  Positional I/O is
`pread`/`pwrite`, on Windows `seek_read`/`seek_write` (std maps
`ERROR_HANDLE_EOF` to `Ok(0)`).  `sync_dir` opens and `fsync`s the directory
on POSIX and only checks that it exists on Windows (directories cannot be
opened as files there; NTFS journals metadata).  The RNG seed comes from
`getrandom`.

### 3.3 Simulated platform (`sim::SimPlatform`)

In memory, deterministic from `SimPlatform::new(seed)` while one thread
drives it.

- **Two images per file**, current (reads) and durable (survives a crash);
  `sync` applies the queued operations to the durable one.  The namespace
  has the same two layers; `sync_dir` makes one directory's entries durable.
- **Crashes**: `sim_fs().crash()` now, or `crash_after_ops(n)` — the n-th
  operation from now crashes instead of running and fails with
  `SimFault::Crash`; `crash()` cancels a scheduled crash.  After a crash every old handle fails with
  `SimFault::StaleHandle`; the file system itself carries on, as after a
  restart.  An entry whose parent directory did not survive is gone too.
- **`CrashMode`**: `DropUnsynced` (default) loses everything unsynced.
  `Torn` keeps a random subset: each unsynced write survives, is lost, or
  survives in some of its 512-byte sectors (`SECTOR_SIZE`), never part of
  one; each length and directory change survives or not.
- **`Faults`**: per-million probabilities of read, write and sync errors
  (`SimFault::Injected`).  An injected write error changes nothing; an
  injected sync error forgets the pending writes ("fsyncgate").
- `sim_fault(&io::Error)` tells simulated failures apart from real errors.
- Paths are component lists (`/a/b` = `a/b`); `..` is refused; directories
  cannot be renamed.
- `sim_clock().advance(d)` is the only thing that moves time.  Wall time
  starts at 2026-01-01T00:00:00Z.

### 3.4 Harness (`harness`)

`harness::run(name, default_cases, |seed| …)` runs a case per seed.  Seeds
are fixed per test name, so `cargo test` is reproducible.  On failure it
prints the seed and the command to rerun it; `NUCLEUS_SEED` runs one seed,
`NUCLEUS_SIM_CASES` changes the count.

### 3.5 Tests

- `tests/conformance.rs` — the same assertions against the real OS (temp
  dir) and the simulation: I/O semantics, open modes, rename, directories,
  clock, RNG.  Runs on every CI target.
- `tests/sim.rs` — durability, directory sync, rename atomicity, fsyncgate,
  injected faults, scheduled crashes, determinism, torn sectors, and a
  proptest property: after a torn crash every byte is either its last-synced
  value or one a later write put there.  Checked against mutations of the
  crash logic (keeping unsynced writes whole, or in part): each fails a test.

## 4. Storage layer (`nucleus-storage`)

Bottom to top: page format → pager → buffer pool → B+tree → WAL → store.
Every layer reaches files only through `nucleus-platform`.

### 4.1 Page format (`page`)

A database file is an array of 8 KiB pages (`PAGE_SIZE`); page `n` is at
offset `n × 8192`.  Every page starts with a 24-byte header (little-endian):

| Offset | Size | Field |
|-------:|-----:|-------|
| 0  | 4 | CRC-32C of bytes `4..8192` |
| 4  | 1 | page type |
| 5  | 3 | reserved, zero |
| 8  | 8 | LSN (reserved; unused by the logical WAL) |
| 16 | 8 | the page's own id |

Payload: 8168 bytes (`PAYLOAD_SIZE`).  Types below 16 belong to the pager
(0 unused, 1 header, 2 free list); the B+tree uses 16 leaf, 17 internal,
18 overflow.  Verification names the failure: `checksum mismatch`, `never
written (all zeros)`, `page id mismatch (misdirected write)`.  `crc32c` is
a portable table-driven CRC-32C checked against the RFC 3720 vectors.

### 4.2 Pager (`pager::Pager`)

**The rule: the durable state is immutable.**  `write_page` accepts only
pages allocated since the last `sync` (else `Error::Immutable`).  Changing a
committed page means copying it; a freed committed page goes to *pending*
and becomes free after the next commit.  A page allocated and freed within
one epoch is free at once.  So a crash at any moment leaves the last
committed state untouched, and `sync` moves to a new one with one header
write.

**API.**  `create`, `open`; `read_page`, `write_page`, `is_writable`,
`allocate` (lowest free page, else grows the file), `free` (refuses double
frees); `meta`/`set_meta` (16 `u64` slots committed atomically with the
pages); `sync`, `close`; `check` (on-disk free list vs. memory, no page in
two places), `accounting` (free / pending / free-list pages / allocated).

**Header** — pages 0 and 1; copy `seq % 2` holds sequence number `seq`; a
commit writes the older copy, `open` takes the valid copy with the highest
`seq`.  Payload: magic `NUCLEUS\0`, format version 1, page size, `seq`, page
count, first free-list page, free count, 16 meta slots.

**Free list** — a chain of free-list pages (`next u64, count u32`, then up
to 1019 page numbers each), rewritten by every commit into pages the
previous state does not use; the previous free-list pages are free in the
new state.  Nothing leaks: after a crash every page is live, free, or
beyond the page count.

**`sync` order:** write the new free list; extend the file to the page
count (unwritten allocated pages become zeros); `fsync`; write the header
copy; `fsync`.  Any failure poisons the pager.  `create` builds the file as
`<path>.creating`, syncs it, renames it into place and syncs the directory.

### 4.3 Buffer pool (`buffer::BufferPool`)

`fetch(id) -> PageRef` pins a page (dropping the `PageRef` unpins);
`PageRef::read()` / `write()` take the frame's reader/writer latch, and
`write()` marks it dirty — after checking the pager's rule, so a missing
copy-on-write fails at once.  `create(type)` allocates a page into a frame;
`free(id)` drops it (must be unpinned).  Eviction is the clock algorithm
over unpinned frames; dirty victims are written back first.  `flush`
writes every dirty frame, `sync` = flush + pager commit.  Page-table
changes and misses take one pool lock.  Errors: `PoolExhausted` (every
frame pinned), `Pinned`.  `stats()`: hits, misses, evictions, write-backs.

### 4.4 B+tree (`btree::BTree`, `node`)

Byte-string keys (≤ `MAX_KEY` = 1024) compared bytewise, values of any
size.  **Node format:** kind, count, leftmost child, an offset table, cells
packed from the end.  Leaf cell `klen u16, flags u8, vlen u32, key, value`;
a cell over 2048 bytes moves its value to an overflow chain (`next u64,
len u32, data`) and stores `first page, length`.  Internal cell `klen u16,
child u64, key`; child `i` holds keys in `[keys[i-1], keys[i])`.

**Copy-on-write:** a node is rewritten in place only if its page is
writable; otherwise copied to a new page (old one freed) and the parent
updated, up to a new root.  **Balance:** split by bytes; a non-root node
under `MIN_FILL` (¼ page) after an insert or delete is merged with a
sibling, or the pair is split evenly; an internal root with one child is
replaced by it.

**API:** `create`, `open(root)`, `root`, `get`, `insert`, `remove`,
`range` / `range_rev` (bounds), `cursor` (`seek`, `seek_after`, `seek_end`,
`next_entry`, `prev_entry`), `destroy`, `check` (ordering, separators,
equal leaf depth, minimum fill, overflow lengths, no page used twice;
returns the pages used).  `&self` reads, `&mut self` writes: callers wrap
the tree in an `RwLock` (D11).  Nodes are decoded into structs to be read
or changed — simple, but allocation-heavy (≈ 30 µs per operation in the
million-operation test); an in-place read path is a known optimisation.

### 4.5 Write-ahead log (`wal::Wal`)

Segments named by start LSN (20 digits, `.wal`); an LSN is a byte position
in the log.  Record: `len u32, crc u32 (CRC-32C of LSN ‖ payload), lsn u64,
payload`.  `append` → LSN past the record; `sync_to(lsn)` is group commit
(one `fsync` for everything appended; waiters for earlier LSNs return at
once); `replay(from, f)`; `truncate_before(lsn)`; `size`, `start`, `end`,
`durable`.

**Crash rules.**  `open` cuts a torn tail of the last segment; a bad record
anywhere else is corruption.  Before a new segment is created, the previous
one *and its directory entry* are synced; segments are deleted oldest
first, one directory sync each — so a crash loses only a suffix or a
prefix, never leaves a gap (a torn-crash sweep found both rules missing).
A failed `fsync` poisons the log.

### 4.6 Store (`store::Store`)

The B+tree made durable by the WAL.  `Store::open(platform, dir, opts)`
creates `dir/data` and `dir/wal/` if needed and recovers; returns
`Recovery { meta, replayed }`.  `write(&Batch)` appends the batch to the
WAL and applies it under the tree's write lock (visible at once);
`wait_durable(lsn)`; `commit` = both.  `read()` gives the tree's read
guard; `get`.  `checkpoint()` syncs the WAL up to the applied LSN, commits
the pager with root, checkpoint LSN and the last batch's `meta`, then
truncates the WAL.  `wal_since_checkpoint()` tells callers when to
checkpoint.  A `Batch` is `meta: u64` (the transaction layer's commit
timestamp) and ordered puts/deletes; its WAL encoding is versioned.  Keys
over `MAX_KEY` are refused before logging.  Any failure poisons the store.
Recovery checkpoints once if it replayed anything; a checkpoint the WAL
does not line up with (ahead of its end, or its start after it) is
corruption.  Options: `cache_pages` (default 1024), `wal_segment_size`
(default 16 MiB).

### 4.7 Tests

- `pager.rs` — round trips (simulated and real), the immutability rule,
  flipped bits, misdirected and unwritten pages, foreign files, header
  copies, free list reuse and spanning pages, poisoning, torn writes, and a
  crash at every I/O point (both modes): exact committed state, every live
  page intact, every page accounted for.
- `buffer.rs` — hits, write-back on eviction, pinning, the copy-on-write
  check, reopen, and an 8-thread stress test on real and simulated files.
- `btree.rs` — model-based property test with commits between epochs,
  deep trees, oversized keys, megabyte values, crashes after a commit, a
  crash at every I/O point of a 4-commit workload (exactly the last commit
  or the one in flight), and the million-operation test (`--ignored`; CI
  runs it on x86_64 Linux).
- `store.rs` — replay, acknowledged vs. unacknowledged writes, WAL
  truncation, corruption, poisoning, concurrency, real files, and a crash
  at every I/O point (both modes, 8 seeds) of batches with unsynced writes
  piling up across segments and checkpoints.

Mutations each caught by these tests: writing committed pages in place,
reusing freed committed pages before a commit, header before data, skipped
rebalancing, wrong split separators, segment creation without a directory
sync, checkpoint without a WAL sync (and, by `pager.rs`'s first version:
allocating from the durable free list, in-place create, no file extension).

## 5. Transaction layer (`nucleus-txn`)

### 5.1 Versions (`mvcc`)

Each version of a user key is a store entry
`escape(key) 00 00 ‖ (u64::MAX − ts) big-endian → tag ‖ value` (tag 1
value, 0 tombstone).  `escape` doubles `00` into `00 FF`, so the encoding
is prefix-free and bytewise order is (user key ascending, timestamp
descending).  A read at `ts` is one seek to `encode_key(k, ts)`.
`first_version(k)` and `after_versions(k)` bound all versions of `k`.  A
user key may be at most 1014 bytes after escaping.

### 5.2 Engine and transactions

`Engine::open(platform, dir, EngineOptions)` (cheap to clone).
`begin()` / `begin_with(TxnOptions { isolation, read_only, at })`.
`Transaction`: `get`, `put`, `delete`, `scan(start, end)` (ascending;
merges the transaction's own writes), `commit() -> Timestamp`,
`rollback()`; dropping it rolls back.  `Engine`: `visible`, `checkpoint`,
`vacuum`, `version_count`, `store`.

**Snapshot isolation.**  A transaction reads at its start timestamp — the
engine's *visible* timestamp, which advances only once a commit is durable,
so nobody reads what a crash could undo.  Writes are buffered.  Commit,
under the commit lock: for each written key, if its newest version (durable
or not) is newer than the snapshot → `Error::Conflict` (retryable; first
committer wins); else one WAL batch with the next timestamp, applied at
once; then the group-commit wait, then *visible* moves.  A checkpoint runs
when the WAL since the last one exceeds `checkpoint_wal_bytes` (64 MiB).
Write skew is possible under snapshot isolation (tested and documented).

**Serializable** (`Isolation::Serializable`, §M14).  The transaction also
records the keys it read and the key ranges its scans consumed (a scan
counts up to the last row taken).  The engine keeps the keys of recent
commits with their timestamps (pruned below the oldest running
transaction once it holds 4096 keys).  At commit, if any recorded key or
range was written by a commit newer than the snapshot, the commit fails
with a retryable `Conflict`.  Read-only transactions never conflict.  This
is FoundationDB's read-conflict check rather than Cahill's SSI (D12): more
aborts in some patterns, much less state, and it distributes.  A write
whose key already has a newer version fails at the write, under both
levels.

**Garbage collection.**  Watermark = the oldest running snapshot (or
*visible*) minus `history` commits.  A commit also deletes, for each key it
writes, every version but the newest at or below the watermark (and that
one if it is a tombstone); `vacuum()` does the same for all keys, in chunks.
Snapshots register under the same lock the watermark is read under, so GC
never removes a version a snapshot can see.  `begin_with(at)` reads an older
snapshot within the history window (read-only), else `SnapshotTooOld`.

**Scans** read the store in batches of 128 visible keys, re-seeking each
time, so no store lock is held between calls; MVCC makes the re-seek
consistent.  Reverse scans are not implemented yet.

Errors: `Storage`, `Conflict { key }`, `SnapshotTooOld`, `ReadOnly`,
`KeyTooLarge`, `Corrupt`; `is_retryable()`.

### 5.3 Tests (`tests/mvcc.rs`)

Own writes and rollback; stable snapshots (no dirty or non-repeatable
reads, no phantoms); uncommitted writes invisible; first committer wins;
write skew possible; no lost updates (6 threads incrementing with retries);
transfers between 8 accounts from 4 threads keep the total in every
snapshot a concurrent reader takes; GC with pinned snapshots, tombstones
and vacuum; history; zero bytes and prefixes in keys; model-based scans
inside transactions; recovery of data and timestamps; a crash at every I/O
point (both modes).  Mutations caught: no conflict check; a GC watermark
that ignores running snapshots.

## 6. KV contract (`nucleus-kv`) and the CLI

### 6.1 The contract

What every layer above the core uses (principle 1), and what the
distributed implementation will provide (§F3).

- `trait Kv { fn begin(&self, TxnOptions) -> Result<Box<dyn KvTxn>> }`
- `trait KvTxn`: `read_ts`, `get(&self)`, `scan(&self, start, end) ->
  KvIter` (ascending), `put(&mut self)`, `delete(&mut self)`,
  `commit(self: Box<Self>) -> Timestamp`, `rollback`.  Reads take `&self`
  so a scan can stay open while the same transaction reads keys.
- `TxnOptions { isolation, read_only, at }` (`read_only()`,
  `serializable()`).
- `Error`: `Conflict { key }` (the only retryable one — `is_retryable()`),
  `SnapshotTooOld`, `ReadOnly`, `KeyTooLarge`, `Storage(boxed)`.
- Helpers: `transact(kv, opts, f)` runs `f` and commits, retrying on
  conflicts (100 attempts); `prefix_end(prefix)` gives the exclusive upper
  bound of a prefix scan.
- Not yet in the contract: reverse scans, scan limits pushed down.  They
  will be added as provided methods, so implementations keep compiling.

`Database` is the local implementation: `Database::open(dir, Options)` on
the real file system, `open_on(platform, dir, Options)` on any platform;
`checkpoint`, `vacuum`, `latest`, `close`.  `Options`: `cache_bytes` (8 MiB),
`checkpoint_wal_bytes` (64 MiB), `wal_segment_bytes` (16 MiB), `history`
(0).  `tests/contract.rs` holds the contract as a suite any implementation
must pass (run on the simulation and on real files, plus 8 threads through
`Arc<dyn Kv>`).

### 6.2 The `nucleus` CLI

```
nucleus --version
nucleus kv <dir> get <key> | put <key> <value> | delete <key> | scan [<prefix>]
nucleus kv <dir> checkpoint | vacuum
nucleus kv <dir>              # shell: the same commands, plus begin/commit/rollback
nucleus sql <dir> [-c <sql>]  # SQL shell (statements end with ;), a script on stdin, or one command
nucleus server <dir> [--listen <addr:port>] [--password <pw>]   # §9
```

Keys and values are text; any byte can be written as `\xNN` (`\\` for a
backslash), and output escapes non-printable bytes the same way.  Outside
`begin` each command is its own transaction; an open transaction at the end
of input is rolled back.  `crates/cli/tests/cli.rs` drives the binary.

### 6.3 Packaging

`scripts/package.sh <rust-target> [<out>]` builds a release and writes, per
PLAN.md "Packaging and install": `nucleus-<v>-linux-<arch>.tar.gz` (gnu),
`…-linux-<arch>-musl.tar.gz`, `…-macos-<arch>.tar.gz`,
`…-windows-<arch>.zip`, and for musl targets `nucleus-<v>-d-os-<arch>.tar.gz`
(`<arch>` `x86_64`, `aarch64`, `i386`).  Every archive has one top directory
`nucleus-<v>/` with `bin/`, `LICENSE`, `README.md`; the d-os one adds the
`.dos-app` manifest from `integrations/d-os/`.  CI packages every tested
target, unpacks it and runs `nucleus --version`.  `.github/workflows/
release.yml` does the same for every target on a `v<version>` tag (the tag
must match `Cargo.toml`) and attaches the archives to a GitHub release.  No
release has been tagged yet.

**d-os:** the archives are built and checked in CI (layout, manifest), but
installing and running them on d-os needs the d-os harness and has not been
done yet.

### 6.4 Benchmark baseline

`cargo run --release -p nucleus-kv --example bench` (real files).  First
numbers, 2026-10-17, Apple Silicon laptop, APFS (`F_FULLFSYNC`), 100-byte
values:

| Workload | Rate |
|----------|-----:|
| single-row commits (one fsync each) | 242 /s |
| rows in 500-row commits | 23 184 /s |
| single-row commits, 8 threads (group commit) | 698 /s |
| point reads (one read-only snapshot) | 98 881 /s |
| full scan | 3.6 M rows/s |

Commits are fsync-bound, as they should be on a durable store.  Point reads
are bounded by decoding whole nodes per level (§4.4).

## 7. d-os settings store (`integrations/d-os`, §M6.5)

`nucleus-dos-settings`: `Layer` (`machine`, `user/<uid>`), `Settings` over
any `&dyn Kv` — `get`, staged `set` / `unset`, `commit` (one transaction),
`discard`, `snapshot`, `dump`, `dump_at(ts)`, `layers`, `import` / `export`
(the `d-os.conf` format).  Keys `cfg/machine/<k>`, `cfg/user/<uid>/<k>`.
The `nucleus-settings <dir>` binary serves it over a line protocol on
stdin/stdout (commands and replies in `integrations/d-os/README.md`); it
keeps 10 000 commits of history.  Packaged in the d-os archive and linked
into `/bin` by the manifest; CI starts it from the unpacked archive.

Tests (`integrations/d-os/tests/settings.rs`): layer separation (including
`user/10` vs `user/1000`), import/export round trip, reading old settings,
the protocol through the binary, and a crash at every I/O point of a
multi-key save (both modes): the old or the new set, never a mix, and never
losing an acknowledged save.

What remains is d-os's: starting the service and wiring it to the kernel
cache (see the README's "Switching d-os over").

## 8. SQL layer (`nucleus-sql`)

Entry point: `Session::new(Arc<dyn Kv>)`, then `execute(sql)` or
`run(&Statement, params)`; results are `Output::Rows { columns, rows }` or
`Output::Command(tag)`.  Errors carry PostgreSQL SQLSTATE codes
(`Error { code, message }`).

### 8.1 Types and encodings (`types`, `encoding`)

Types: `BOOLEAN`, `BIGINT` (also `INT`, `INTEGER`, `SMALLINT`, `INT2/4/8`
— all 64-bit), `DOUBLE PRECISION` (also `FLOAT`, `REAL`), `TEXT` /
`VARCHAR(n)` / `CHAR` (length enforced for `VARCHAR(n)`).  `Value` has SQL
comparison (`NULL` → unknown, numbers compare across int/float), a sort
order with `NULL` last, casts (text parses to any type; float → int
rounds; bool → text is `true`/`false`), and PostgreSQL text output.

Key encoding (order-preserving, self-delimiting): `00` NULL; `01` bool;
`02` + `i ^ i64::MIN` big-endian; `03` + sign-flipped/inverted IEEE bits
(`-0` = `0`, one `NaN` above everything); `04` + text with `00` → `00 FF`,
terminated `00 01`.  Row encoding: version `01`, column count, tagged
values (missing trailing columns read as NULL, so `ADD COLUMN` needs no
rewrite unless it has a default).

### 8.2 Catalog and keyspace (`catalog`)

| Key | Value |
|-----|-------|
| `00 01 <table name>` | table definition (columns, primary key, indexes, checks) |
| `00 02 <sequence>` | counter (`table_id`) |
| `00 03 <table id>` | next hidden row id (tables without a primary key) |
| `00 04 <table id>` | statistics (§8.8) |
| `01 <table id> 00000000 <pk>` | the row |
| `01 <table id> <index id> <index values> [<pk>]` | index entry → encoded pk |

A unique index entry is keyed by the index values alone (unless one is
NULL), so two transactions inserting the same value write the same key and
the second conflicts; other entries append the primary key.  The catalog is
read through the statement's own transaction, so DDL is transactional and
needs no cache invalidation.

### 8.3 Parser (`lexer`, `parser`, `ast`)

Statements: `CREATE TABLE [IF NOT EXISTS]` (column constraints `PRIMARY
KEY`, `NOT NULL`, `UNIQUE`, `DEFAULT`, `CHECK`; table constraints `PRIMARY
KEY (…)`, `UNIQUE (…)`, `[CONSTRAINT n] CHECK (…)`), `DROP TABLE [IF
EXISTS]`, `CREATE [UNIQUE] INDEX [IF NOT EXISTS]`, `DROP INDEX [IF EXISTS]`,
`ALTER TABLE … ADD [COLUMN]`, `INSERT … VALUES | SELECT | DEFAULT VALUES
[RETURNING]`, `UPDATE … SET … [WHERE] [RETURNING]`, `DELETE … [WHERE]
[RETURNING]`, queries, `BEGIN`/`START TRANSACTION` (`ISOLATION LEVEL …`,
`READ ONLY|WRITE`), `COMMIT`/`END`, `ROLLBACK`, `SET TRANSACTION …`, `SET
name = value`, `SHOW name`, `EXPLAIN`, `ANALYZE [table]`.

Queries: `SELECT [DISTINCT]`, `FROM` with comma joins, `[INNER|LEFT|RIGHT|
CROSS] JOIN … ON | USING`, subqueries in `FROM`, `WHERE`, `GROUP BY`
(expressions, positions, aliases), `HAVING`, `UNION [ALL]`, `VALUES`,
`ORDER BY` (`ASC|DESC`, `NULLS FIRST|LAST`, positions, aliases,
expressions), `LIMIT`, `OFFSET`.  Expressions: arithmetic, `||`,
comparisons, `AND/OR/NOT`, `IS [NOT] NULL|TRUE|FALSE|DISTINCT FROM`,
`[NOT] BETWEEN`, `[NOT] IN (list | subquery)`, `[NOT] EXISTS`, scalar
subqueries, `[NOT] LIKE / ILIKE`, `CASE`, `CAST` / `::`, `$n`.  Functions:
`lower upper length trim substr replace strpos concat coalesce nullif
greatest least abs round floor ceil sqrt power version`; aggregates `count
sum avg min max` (with `DISTINCT`, `count(*)`).

Not yet: `FOREIGN KEY` (refused with `0A000`), `FULL JOIN`, `EXCEPT`,
`INTERSECT`, window functions, `COPY`, column alias lists on subqueries,
date/time and decimal types.

### 8.4 Binder (`binder`)

Resolves names through nested scopes — a subquery sees enclosing rows as
`Outer { depth, index }`, which makes it *correlated* — reports ambiguous
(`42702`) and unknown (`42703`, `42P01`) names, infers types (an untyped
string literal or `NULL` takes the other side's type in comparisons and
arithmetic, never in `||`; a `$n` takes its context's type — including
`INSERT`/`UPDATE` target columns, for `describe`), lifts aggregates into an
`Aggregate` node (grouped expressions matched structurally; other column
references are `42803`), and places `ORDER BY` keys (output name or
position, else a hidden extra column, refused under `DISTINCT`).

### 8.5 Plans and the executor (`plan`, `exec`, `expr`)

Plan nodes: `Scan` (table, access path, filter), `Values`, `Filter`,
`Project`, `Join` (inner/left; nested loop or hash), `Aggregate`, `Sort`,
`Limit`, `Distinct`, `Union`.  The executor runs them as iterators; joins
materialise their right input, sorts and aggregates their input.
Subqueries run per row when correlated, once per execution otherwise
(cached in the execution context).  Expressions use three-valued logic,
checked integer arithmetic (`22003`), division by zero (`22012`).

### 8.6 Optimizer (`optimizer`)

Predicate pushdown (through projections, into join sides, outer-join safe),
WHERE conjuncts into join conditions, hash joins on equality conjuncts,
access paths: primary key or index with an equality prefix and a range on
the next column; the filter always stays, so an access path only narrows.
`EXPLAIN` prints the plan (columns as `#n`).

### 8.7 Writes, constraints and sessions (`dml`, `session`)

Writes collect their rows first, then write (no Halloween problem).
Constraint errors: `23502` NOT NULL, `22001` too long, `23514` CHECK (a
NULL check passes), `23505` duplicate key; `42804` wrong assignment type.
`CREATE INDEX` backfills (and refuses existing duplicates for `UNIQUE`);
`ADD COLUMN` with a default rewrites existing rows.

Sessions: outside `BEGIN` every statement is its own transaction, retried
up to 10 times on `40001`; inside, the KV transaction starts with the
first statement (so `SET TRANSACTION` may follow `BEGIN`), an error moves
the block to *failed* (`25P02` until `ROLLBACK`; `COMMIT` then rolls
back).  `REPEATABLE READ`, `READ COMMITTED` and `READ UNCOMMITTED` all run
as snapshot isolation; `SERIALIZABLE` as §5.2.  `SET
default_transaction_isolation`; `SHOW transaction_isolation |
server_version | …`.

### 8.8 Statistics and costs (`stats`)

`ANALYZE [table]` stores rows, and per column NULLs, distinct values and a
16-bucket equi-depth histogram.  Selectivities: `=` 1/ndv, ranges from the
histogram, `IS NULL` the null fraction, `IN` n/ndv, `LIKE` 0.1, combined
by AND/OR/NOT.  For analyzed tables the access path is the cheapest of a
full scan and every index (a secondary index pays a row fetch per row),
`EXPLAIN` shows `rows=`, and chains of up to 8 inner joins are reordered by
dynamic programming (no cross products unless nothing connects; the
smaller input becomes the hash build side; a projection restores the
column order).  Without statistics the rule-based plan is kept.

### 8.9 Tests

- `tests/slt/*.slt` (eleven scripts: basics, expressions, joins,
  aggregates, subqueries, indexes, constraints, transactions, plans,
  statistics, serializable) through `nucleus_sql::slt`, which also runs
  them over the wire (§9).  `SLT=<name>` runs one script.
- `tests/encoding.rs`: key order and round trips, property-tested.
- `cargo run -p nucleus-sql --example sql_script -- file.sql` prints the
  results of a script.

## 9. Server (`nucleus-server`)

`nucleus server <dir> [--listen 127.0.0.1:5432] [--password <pw>]`, or
`nucleus_server::serve(listener, kv, Config)`.  PostgreSQL protocol 3.0:
SSL/GSS requests declined (`N`), trust or cleartext-password
authentication (`28P01` on failure), parameter status (`server_version`
16.0 (nucleus), UTF8, ISO dates), backend key data, `ReadyForQuery` with
the transaction status.  Simple query protocol (statements one by one, the
first error stops the rest) and extended protocol: Parse (parameter types
inferred, or given), Bind (text or binary parameters), Describe (statement
and portal), Execute with a row limit (`PortalSuspended`), Sync, Close,
Flush; after an error, messages are skipped until Sync.  Types on the wire:
`bool` 16, `int8` 20 (`int2`/`int4` accepted), `float8` 701 (`float4`
accepted), `text` 25 (`varchar` accepted); binary formats for all of them.
One thread per connection.

Not supported: `COPY`, cancellation, `pg_catalog` (so `psql`'s `\d`
commands fail; plain SQL works).

Tests: `crates/server/tests/wire.rs` runs every slt script over the wire
with a minimal client, checks password authentication and the SSL refusal,
and drives the extended protocol with the `postgres` crate (prepared
statements, typed and NULL parameters, binary results, SQLSTATE codes,
transactions, portals, `RETURNING`).  `scripts/psql-smoke.sh` (run by CI on
Linux) uses the real `psql`.

## 10. Change log

- **2026-10-17 — §F1 shipped:** SQL layer (§M7–§M11), server (§M12),
  statistics and costs (§M13), serializable (§M14); `nucleus sql` and
  `nucleus server`.
- **2026-10-17 — §M6.5, nucleus side:** the settings service, packaged for
  d-os.
- **2026-10-17 — §M6 shipped** (d-os run pending): KV contract, `Database`,
  CLI, packaging for every platform and d-os, release workflow, benchmark
  baseline.  Version 0.1.0.
- **2026-10-17 — §M2–§M5 shipped; §M1 revised.**  Pager: the durable state
  is immutable, free list in dedicated pages, no leaks (the first version's
  linked list and reserve are gone).  Buffer pool, copy-on-write B+tree,
  WAL + store with checkpoints and recovery, MVCC with snapshot isolation.
  CI reports test failures as annotations; long tests run on one target.
- **2026-10-17 — §M1 shipped** (CI green on all nine jobs): page format, CRC-32C, A/B
  header, crash-safe free list with reserve, atomic create, poisoning; crash
  sweep; `[profile.test] opt-level = 1` (crash sweep 36 s → 5 s).
- **2026-10-17 — §M0 shipped** (CI green: lint + all eight build targets).  Workspace, platform layer (real POSIX +
  Windows, simulation with faults and crashes), seed harness, conformance and
  simulation tests, CI for every build target.
