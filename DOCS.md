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
| 10 | The nucleus protocol (`ncp`) |
| 11 | Drivers (`drivers/`) |
| 12 | Change log |

---

## 1. Repository layout

| Path | Crate | Contents | Milestone |
|------|-------|----------|-----------|
| `crates/platform` | `nucleus-platform` | file system, clock and randomness traits; real and simulated implementations; simulation harness | §M0 |
| `crates/storage` | `nucleus-storage` | page format, pager, buffer pool, B+tree, WAL, store | §M1–§M4 ✅ |
| `crates/txn` | `nucleus-txn` | MVCC, snapshot isolation, GC | §M5 ✅ |
| `crates/kv` | `nucleus-kv` | the KV contract and the local `Database` | §M6 ✅ |
| `crates/sql` | `nucleus-sql` | types, encodings, catalog (`nc_catalog`, PostgreSQL views), parser, binder, optimizer, executor, sessions | §M7–§M14.5 ✅ |
| `crates/server` | `nucleus-server` | the nucleus protocol; the PostgreSQL protocol (optional) | §M12, §M14.7 ✅ |
| `crates/client` | `nucleus-client` | the Rust driver | §M14.6–§M14.8 ✅ |
| `crates/cli` | `nucleus-cli` (binaries `nucleus`, `ncsql`) | `nucleus kv`, `nucleus sql` / `ncsql`, `nucleus server` | §M6, §M12, §M14.6 ✅ |
| `integrations/d-os` | `nucleus-dos-settings` (binary `nucleus-settings`) | the d-os settings service and app manifest | §M6, §M6.5 ◐ |
| `drivers` | — | drivers for C, C++, Python, JS/TS, Java (§11) | §M14.8 ✅ |
| `examples` | `nucleus-example` (Rust) | the quickstart in every driver's language | §M14.8 ✅ |
| `scripts` | — | `package.sh`, `psql-smoke.sh`, `driver-tests.sh` | §M6, §M12, §M14.8 |

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
only); on Linux it also runs the real `psql` against `--pg-compat`
(`scripts/psql-smoke.sh`, §9), and the `drivers` job runs every driver's
tests and every example against a server (`scripts/driver-tests.sh`, §11):

```sh
cargo build --release -p nucleus-cli
scripts/driver-tests.sh target/release/nucleus [rust python js java c cpp]
scripts/psql-smoke.sh target/release/nucleus     # needs psql
```

  The `i686` musl tests run as 32-bit binaries on a 64-bit Linux
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
- `Clock` — `now()` monotonic since platform creation, `wall()`, `sleep(d)`
  (the simulated clock advances by `d` at once and waits at most 1 ms of
  real time, so threads backing off on the simulation still spread out).
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
- `sim_clock().advance(d)` (and `clock().sleep(d)`) is the only thing
  that moves time.  Wall time
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
takes an exclusive lock on `dir/LOCK` (`FileSystem::lock`: `flock` /
`LockFileEx` on the OS, a lock table in the simulation that a crash
clears) and holds it as long as the store — a second process (or store)
fails with `Error::Locked` ("in use by another process") instead of
corrupting the database — then creates `dir/data` and `dir/wal/` if
needed and recovers; returns `Recovery { meta, replayed }`.  `write(&Batch)` appends the batch to the
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
`rollback()`; dropping it rolls back.  `Timestamp` is opaque outside the
crate (principle 3): compared, printed and parsed (`Display`, `FromStr`),
never computed with.  `Engine`: `visible`, `checkpoint`,
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

**Validated reads** (§M14.5).  `get_validated` / `scan_validated` record
the key or consumed range in the same read set at *any* isolation level, so
a snapshot-isolation transaction can make one decision safe without
becoming serializable: its commit fails if a newer commit wrote what it
read.  Foreign key checks use them (§8.10).

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

- `trait Kv`: `begin(&self, TxnOptions) -> Result<Box<dyn KvTxn>>`;
  `wall_clock()` — the time as the implementation keeps it (the local
  one's platform clock), the only clock layers above read (principle 4);
  `back_off(attempt)` — the wait before a retry (exponential with
  jitter, through the platform, so a simulation does not wait).
- `trait KvTxn`: `read_ts`, `get(&self)`, `scan(&self, start, end) ->
  KvIter` (ascending), `get_validated` / `scan_validated` (reads checked
  at commit at any isolation level, §5.2), `put(&mut self)`,
  `delete(&mut self)`,
  `commit(self: Box<Self>) -> Timestamp`, `rollback`.  Reads take `&self`
  so a scan can stay open while the same transaction reads keys.
- `TxnOptions { isolation, read_only, at }` (`read_only()`,
  `serializable()`).
- `Error`: `Conflict { key }` (the only retryable one — `is_retryable()`),
  `SnapshotTooOld`, `ReadOnly`, `KeyTooLarge`, `Storage(boxed)`.
- Helpers: `transact(kv, opts, f)` runs `f` and commits, retrying on
  conflicts (100 attempts, `back_off` between them); `prefix_end(prefix)` gives the exclusive upper
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
nucleus sql [<dir>] [-h <host[:port]>] [-p <port>] [-U <user>] [-d <db>] [--password <pw>] [-c <sql>]
ncsql …                       # the same shell under its own name
nucleus server <dir> [--listen <addr:port>] [--password <pw>] [--log-queries]   # §9
```

Keys and values are text; any byte can be written as `\xNN` (`\\` for a
backslash), and output escapes non-printable bytes the same way.  Outside
`begin` each command is its own transaction; an open transaction at the end
of input is rolled back.  `crates/cli/tests/cli.rs` drives the binaries.

**`ncsql`** (§M14.6) is nucleus's SQL shell: with a directory it opens
that database itself; otherwise it connects to a nucleus server over the
nucleus protocol (default `127.0.0.1:20035`, user, password and database
`nucleus`; the password also from `NCPASSWORD`).  Statements end with `;` (a script on stdin,
or one `-c` command); the prompt shows `*` in a transaction block and `!`
in a failed one.  Its backslash commands read `nc_catalog` (§8.11), so they
work the same locally and remotely:

| Command | Shows |
|---------|-------|
| `\d`, `\dt`, `\tables` | tables, with index, foreign key and statistics counts |
| `\d <table>` | columns, then indexes (with index ids), foreign keys, referencing tables, checks; the title has the table id, key prefix and whether it is analyzed |
| `\indexes [<table>]`, `\di` | indexes |
| `\fks [<table>]` | foreign keys from or to a table |
| `\stats <table>` | `ANALYZE` statistics |
| `\schemas`, `\dn` · `\types`, `\dT` | schemas, types |
| `\timing` · `\conninfo` · `\?` · `\q` | timing toggle, connection, help, quit |

The network side is `nucleus-client` (`crates/client`), the Rust driver
(§11).

### 6.3 Packaging

`scripts/package.sh <rust-target> [<out>]` builds a release and writes, per
PLAN.md "Packaging and install": `nucleus-<v>-linux-<arch>.tar.gz` (gnu),
`…-linux-<arch>-musl.tar.gz`, `…-macos-<arch>.tar.gz`,
`…-windows-<arch>.zip`, and for musl targets `nucleus-<v>-d-os-<arch>.tar.gz`
(`<arch>` `x86_64`, `aarch64`, `i386`).  Every archive has one top directory
`nucleus-<v>/` with `bin/` (`nucleus` and `ncsql`), `LICENSE`, `README.md`;
the d-os one adds the `.dos-app` manifest from `integrations/d-os/` (linking
`/bin/nucleus`, `/bin/ncsql`, `/bin/nucleus-settings`).  CI packages every
tested target, unpacks it and runs `nucleus --version` and `ncsql --help`.  `.github/workflows/
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

### 8.1 Types and encodings (`types`, `encoding`, `decimal`, `datetime`)

| Type | Names | Value |
|------|-------|-------|
| boolean | `BOOLEAN`, `BOOL` | |
| bigint | `BIGINT`, `INT`, `INTEGER`, `SMALLINT`, `INT2/4/8`, `OID` | 64-bit (every integer type) |
| double precision | `DOUBLE PRECISION`, `FLOAT`, `REAL` | IEEE 754 |
| numeric | `NUMERIC [(p, s)]`, `DECIMAL`, `DEC` | exact: 128-bit mantissa and a scale, ≤ 38 digits |
| text | `TEXT`, `VARCHAR(n)`, `CHAR`, `NAME` | `VARCHAR(n)` length enforced |
| date | `DATE` | days since 2000-01-01, 4713 BC – 294276 AD |
| timestamp | `TIMESTAMP [WITHOUT TIME ZONE]` | µs since 2000-01-01 |
| timestamptz | `TIMESTAMPTZ`, `TIMESTAMP WITH TIME ZONE` | µs since 2000-01-01 UTC |
| interval | `INTERVAL` | months, days, µs (kept apart) |
| arrays | `type[]` | one dimension, elements of the type or NULL |

Decimal literals (`1.5`, `1e3`) are `numeric`, as in PostgreSQL; integers
too big for bigint too; only values beyond numeric's 38 digits fall back to
double.  Numeric arithmetic is exact and never rounds silently (overflow is
`22003`): `+`/`-` keep the larger scale, `*` adds them, `/` keeps at least
16 significant digits (PostgreSQL's rule); rounding is half away from zero;
`NUMERIC(p, s)` rounds to `s` and checks `p`.  Numbers compare across
types (numeric against double as doubles); dates and timestamps compare
across each other (a date as its midnight); intervals by their span (a
month as 30 days).  `avg` of integers is numeric.

Date/time: sessions are in UTC.  Input `YYYY-MM-DD`, `YYYY-MM-DD[ T]HH:MM[:SS
[.f]][zone]` (`Z`, `UTC`, `±HH[:MM]`; ignored for `timestamp`), `epoch`,
` BC`; intervals as `N unit …` (µs to millennia, fractions spill into
smaller units), `[-]H:M[:S]`, `@ … ago`, ISO 8601 `P…`.  Output is
PostgreSQL's ISO style (`2024-03-10 12:05:06+00`, `1 year 2 mons 3 days
04:05:06`).  Arithmetic: `date ± int`, `date - date`, `± interval` (months
by the calendar, the day clamped), `timestamp - timestamp` (days and
time), `interval × / number`.  Typed literals: `DATE '…'`, `INTERVAL '…'`,
any `type 'text'`.  Arrays: `'{a,"b c",NULL}'`, `ARRAY[…]`,
`ARRAY(subquery)`, `a[i]` (1-based, NULL out of range), `||`.

Key encoding (order-preserving, self-delimiting): `00` NULL; `01` bool;
`02` + `i ^ i64::MIN` big-endian; `03` + sign-flipped/inverted IEEE bits
(`-0` = `0`, one `NaN` above everything); `04` + text with `00` → `00 FF`,
terminated `00 01`; `05` numeric (sign class, decimal exponent, digits;
equal values of different scale share a key); `06` date; `07`/`08`
timestamps; `09` interval (its span); `0A` arrays (each element after
`01`, then `00`).  Row encoding: version `01`, column count, tagged values
(missing trailing columns read as NULL, so `ADD COLUMN` needs no rewrite
unless it has a default).

### 8.2 Catalog and keyspace (`catalog`)

| Key | Value |
|-----|-------|
| `00 00` | the format record: keyspace layout and key encoding versions (`u16` each, both 1) |
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

The format record (principle 6) is checked by a session's first
statement and written by the first write to a database the SQL layer has
not used; a newer format than the build reads is refused with `0A000`
rather than misread.  `check::check(txn)` lists every inconsistency between
the catalog, the rows and the indexes (rows under their primary key, `NOT
NULL`, index entries exactly as the rows imply, unique indexes, foreign
keys, no keys of unknown tables or indexes).

### 8.3 Parser (`lexer`, `parser`, `ast`)

Statements: `CREATE TABLE [IF NOT EXISTS]` (column constraints `PRIMARY
KEY`, `NOT NULL`, `UNIQUE`, `DEFAULT`, `CHECK`, `REFERENCES`; table
constraints `PRIMARY KEY (…)`, `UNIQUE (…)`, `CHECK (…)`, `FOREIGN KEY (…)
REFERENCES …`, each with an optional `CONSTRAINT name`), `DROP TABLE [IF
EXISTS] … [CASCADE]`, `CREATE [UNIQUE] INDEX [IF NOT EXISTS]`, `DROP INDEX
[IF EXISTS]`, `ALTER TABLE … ADD [COLUMN] …`, `ALTER TABLE … ADD
[CONSTRAINT n] FOREIGN KEY | CHECK | UNIQUE …`, `ALTER TABLE … DROP
CONSTRAINT [IF EXISTS] n`, `INSERT … VALUES | SELECT | DEFAULT VALUES
[RETURNING]`, `UPDATE … SET … [WHERE] [RETURNING]`, `DELETE … [WHERE]
[RETURNING]`, queries, `BEGIN`/`START TRANSACTION` (`ISOLATION LEVEL …`,
`READ ONLY|WRITE`), `COMMIT`/`END`, `ROLLBACK`, `SET TRANSACTION …`, `SET
name = value`, `SHOW name`, `EXPLAIN`, `ANALYZE [table]`.  Table names may
be qualified with `public.` (and, to read, `nc_catalog.`, `pg_catalog.`,
`information_schema.`).

Queries: `WITH [RECURSIVE] name [(cols)] AS (…)`, `SELECT [DISTINCT]`,
`FROM` with comma joins, `[INNER|LEFT|RIGHT|FULL|CROSS] JOIN … ON | USING`,
subqueries and functions in `FROM` (`generate_series`, `unnest`, any
scalar function; `WITH ORDINALITY`), alias column lists (`AS t (a, b)`),
`WHERE`, `GROUP BY` (expressions, positions, aliases), `HAVING`, `UNION |
EXCEPT | INTERSECT [ALL]` (INTERSECT binding tighter), `VALUES`, `ORDER BY`
(`ASC|DESC`, `NULLS FIRST|LAST`, positions, aliases, expressions), `LIMIT`,
`OFFSET`.  Expressions: arithmetic, `||`, comparisons, `AND/OR/NOT`, `IS
[NOT] NULL|TRUE|FALSE|DISTINCT FROM`, `[NOT] BETWEEN`, `[NOT] IN (list |
subquery)`, `[NOT] EXISTS`, `op ANY|SOME|ALL (array | subquery)`, scalar
subqueries, `[NOT] LIKE / ILIKE`, `~ ~* !~ !~*` (regular expressions,
linear-time), `CASE`, `CAST` / `::`, typed literals, `E'…'` strings,
`COLLATE` (accepted, ignored: text compares bytewise), `OPERATOR(op)`,
`$n`, `extract(field FROM x)`.  Functions: `lower upper length trim substr
replace strpos concat coalesce nullif greatest least abs round trunc floor
ceil sign mod sqrt power regexp_replace version now current_timestamp
current_date localtimestamp clock_timestamp extract date_part date_trunc
make_date age justify_days justify_hours justify_interval to_timestamp
array_length array_upper array_lower cardinality array_append
array_to_string pg_typeof current_user current_database current_schema
current_setting` (and the catalog functions of §8.11); aggregates `count
sum avg min max string_agg array_agg bool_and bool_or every` (with
`DISTINCT`, `count(*)`, `ORDER BY` inside the call).

Not yet: window functions, `COPY`, `TIME` / `TIMETZ`, time zones other than
UTC, multi-dimensional arrays, data-modifying CTEs, `LATERAL`, user
functions.  Every integer is 64-bit (`pg_typeof(1)` is `bigint`).

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
`Project`, `Join` (inner, left, full; nested loop or hash), `Aggregate`,
`Sort`, `Limit`, `Distinct`, `SetOp` (union, except, intersect),
`Cte` (computed once per execution, shared by its references),
`Recursive` / `WorkTable` (`WITH RECURSIVE`: the step re-runs over the
previous round until it yields nothing; `UNION` drops repeats, so cycles
end), `Function` (functions in `FROM`), `Virtual` (catalog tables).  The executor runs them as iterators; joins
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
NULL check passes), `23505` duplicate key, `23503` foreign key; `42804`
wrong assignment type (assignment converts within numbers, within dates
and timestamps, and from or to text).
`CREATE INDEX` backfills (and refuses existing duplicates for `UNIQUE`);
`ADD COLUMN` with a default rewrites existing rows.

Sessions: outside `BEGIN` every statement is its own transaction, retried
up to 10 times on `40001` (with the KV contract's `back_off`); inside, the KV transaction starts with the
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

- `tests/slt/*.slt` (nineteen scripts: basics, expressions, joins,
  aggregates, subqueries, indexes, constraints, transactions, plans,
  statistics, serializable, set operations, full joins, CTEs, numeric,
  date/time, foreign keys, arrays and regular expressions, catalogs)
  through `nucleus_sql::slt`, which also runs them over the wire (§9).
  `SLT=<name>` runs one script.  The expected results of scripts 12–18
  were produced by PostgreSQL 16 (records marked `nucleus-only` excepted:
  concurrency, and what PostgreSQL does not do).
- `tests/slt-pg/*.slt`: the pg-compat module's scripts (`pg_catalog`,
  `information_schema`, `pg_*` functions), run with it registered — and
  over the PostgreSQL protocol by the server's tests;
  `pg_compat_only_where_registered` checks that sessions without it see
  none of it.
- `tests/encoding.rs`: key order and round trips, property-tested (every
  type).
- `tests/crash.rs`: random DDL and DML (in autocommit and in blocks)
  through `Session` on the simulation, with a crash at every I/O operation
  in both crash modes; after reopening, the state is the one after every
  statement that returned (or the one in flight) and `check` finds
  nothing.  `tests/format.rs`: the format record.  `tests/slt.rs` also
  checks that `now()` is the simulated clock and fixed within a
  transaction (`clock_timestamp()` is not).
- `cargo run -p nucleus-sql --example sql_script -- file.sql` prints the
  results of a script.

### 8.10 Foreign keys (`fk`)

A foreign key references the primary key or a unique constraint of a table
(itself allowed), with columns of the same types; MATCH SIMPLE (a row with
a NULL in its key references nothing).  Checks run at the end of each
statement, so a statement may insert a parent after its child, or swap
keys:

- **child side** — every inserted row, and every updated row whose key
  changed, must find its referenced row: a point lookup on the parent's
  primary or unique index;
- **parent side** — for every deleted row, or updated row whose referenced
  key changed (unless another row now holds the key), the referencing rows
  are found (through an index whose leading columns are the foreign key, or
  a full scan) and the action runs: `NO ACTION` / `RESTRICT` fail
  (`23503`), `CASCADE` deletes them or updates their columns, `SET NULL` /
  `SET DEFAULT` update them (a default must reference a row too).  Cascaded
  writes run the same checks, recursively.

Both lookups are validated reads (§5.2): an insert racing the delete of its
parent fails one of the two transactions at commit with `40001`, at any
isolation level, while children of one parent do not conflict with each
other.  PostgreSQL blocks on row locks instead.  Index the foreign key
columns: a parent-side check without such an index reads, and so
validates, the whole child table.  Not supported: deferrable constraints,
MATCH FULL.  `DROP TABLE` of a referenced table needs `CASCADE` (which
drops the referencing constraints); dropping an index a foreign key needs
is refused (`2BP01`).

### 8.11 System catalogs (`ncatalog`, `pgcatalog`)

nucleus's own catalog is **`nc_catalog`**: read-only views made from the
catalog when scanned (D13), registered by the built-in module (§8.12).

| Table | Rows |
|-------|------|
| `nc_catalog.schemas` | `name`, `description` (the session registry's schemas) |
| `nc_catalog.tables` | `table_id`, `name`, `columns`, `primary_key` (names), `has_row_id`, `indexes`, `checks`, `foreign_keys`, `referenced_by`, `key_prefix` (hex), `analyzed_rows` |
| `nc_catalog.columns` | `table_id`, `table_name`, `position`, `name`, `type`, `nullable`, `default_expr`, `in_primary_key` |
| `nc_catalog.indexes` | `table_id`, `table_name`, `index_id` (0 = primary), `name`, `columns`, `is_unique`, `is_primary`, `key_prefix` |
| `nc_catalog.constraints` | `table_id`, `table_name`, `name`, `kind`, `definition` |
| `nc_catalog.foreign_keys` | `table_id`, `table_name`, `name`, `columns`, `referenced_table`, `referenced_columns`, `on_delete`, `on_update` |
| `nc_catalog.statistics` | `table_id`, `table_name`, `column_name`, `rows`, `nulls`, `distinct_values`, `histogram` |
| `nc_catalog.types` | `name`, `aliases`, `category` |

**PostgreSQL compatibility: the `pg-compat` module** (`pgcatalog::PgCompat`,
§8.12).  Only sessions whose registry has it see any of what follows —
the PostgreSQL front end's (§9); `ncp` sessions and the embedded
`Session::new` do not (principle 11).  PostgreSQL clients hard-code
PostgreSQL's catalog names, so the same catalog is also shown as
`pg_catalog` (`pg_namespace`, `pg_class`, `pg_attribute`, `pg_attrdef`,
`pg_type`, `pg_index`, `pg_constraint`, `pg_am`, `pg_collation`,
`pg_roles`, `pg_database`, `pg_tablespace`, `pg_tables`, `pg_indexes`, and
empty `pg_policy`, `pg_trigger`, `pg_inherits`, `pg_statistic_ext`,
`pg_publication*`, `pg_description`, `pg_auth_members`) and
`information_schema` (`schemata`, `tables`, `columns`,
`table_constraints`, `key_column_usage`), with the functions their queries
call (`format_type`, `pg_get_expr`, `pg_get_indexdef`,
`pg_get_constraintdef`, `pg_get_userbyid`, `pg_table_is_visible`,
`pg_partition_ancestors`, `has_*_privilege`, `obj_description`, …) and
`::regclass` / `regtype` / `regnamespace` casts (`regclass` maps a name to
an oid and an oid to a name).  Unqualified `pg_*` names resolve to these
unless a user table has the name.  Oids: PostgreSQL's for types,
namespaces and access methods; a table is `16384 + 1000 × table id`, its
indexes and constraints numbered after it (`pgcatalog.rs` has the table).
One role, `nucleus`.  This is what makes `psql`'s `\d`, `\d table`,
`\d+`, `\dt`, `\di`, `\dv`, `\dn`, `\du`, `\l` work; `\df` and others
that need functions, sequences or permissions do not.  `pg_typeof(x)` is
one of its functions too.

### 8.12 Extension API (`ext`, `builtin`)

What a module adds to the SQL layer (principle 12), versioned
(`ext::API_VERSION`, 1):

- `Module` — `name()`, `register(&mut Registry)`.
- `ScalarFunction` — `name()` (for `EXPLAIN`), `arity()`,
  `resolve(&[ArgInfo]) -> Signature` (bind-time typing: per argument
  `Keep`, `Untyped(t)` — an untyped literal, `NULL` or parameter takes
  `t` —, `Convert(t)` or `Common`; the result a type, the arguments'
  common type, or a `Constant`), `strict()` (default true: a `NULL`
  argument gives `NULL` without a call), `call(&CallContext, &[Value])`.
  `CallContext` gives the statement's transaction and the registry.
- `CatalogView` — `schema()`, `name()`, `columns()`, `rows(&CallContext)`.
- `Registry` — `empty()`, `builtin()` (the `builtin` module: every
  built-in function, the schemas `public` and `nc_catalog`, the
  `nc_catalog` views), `shared_builtin()`, `add(&dyn Module)` (refused,
  and the registry unchanged, if the module or anything it registers is
  already there), `add_function(names, f)`, `add_schema(name,
  description, searched)` (a searched schema's views are found by
  unqualified names after the user's tables, as `pg_catalog`'s),
  `add_view(v)`, and the lookups `function`, `view`, `schemas`, `views`,
  `modules`.

A session runs against one registry: `Session::new(kv)` the built-in one,
`Session::with_registry(kv, registry)` any.  The binder finds functions
and views there; bound calls and view scans hold the registered objects,
so execution needs no lookup.  The built-in functions share one
implementation behind the same interface.

## 9. Server (`nucleus-server`)

```
nucleus server <dir> [--listen <addr:port>] [--pg-compat [<addr:port>]]
                     [--user <name>] [--password <pw>] [--no-auth] [--log-queries]
```

The server speaks **the nucleus protocol** (§10, `nucleus_server::serve`)
on `--listen` (default `127.0.0.1:20035`).  Clients log in as user
`nucleus` with password `nucleus` unless `--user` / `--password` say
otherwise; `--no-auth` accepts anyone (`Config::trust()`); a wrong user or
password is `28P01`.  Every nucleus client tries `nucleus` / `nucleus` by
default, so a fresh server and client work together unconfigured.
Passwords travel in clear text: keep servers on trusted networks (TLS is
later work).  One thread per connection.  `--log-queries`
(`Config::log_queries`) prints every statement a client sends, and every
error, to stderr — to see what a tool asks for.

**PostgreSQL compatibility** (optional, D14): `--pg-compat` also listens
for the PostgreSQL protocol (`nucleus_server::serve_pg`, default
`127.0.0.1:5432`), with the same credentials, so `psql` and PostgreSQL
drivers and tools can connect.  Its sessions have the `pg-compat` module
(§8.11); `ncp` sessions do not.  It is frozen: kept working, not extended
for the catalog queries particular tools send (some, like Valentina
Studio, ask for PostgreSQL internals nucleus does not have).  PostgreSQL
protocol 3.0:
SSL/GSS requests declined (`N`), trust or cleartext-password
authentication (`28P01` on failure), parameter status (`server_version`
16.0 (nucleus), UTF8, ISO dates), backend key data, `ReadyForQuery` with
the transaction status.  Simple query protocol (statements one by one, the
first error stops the rest) and extended protocol: Parse (parameter types
inferred, or given), Bind (text or binary parameters), Describe (statement
and portal), Execute with a row limit (`PortalSuspended`), Sync, Close,
Flush; after an error, messages are skipped until Sync.  Types on the wire:
`bool` 16, `int8` 20 (`int2`/`int4` accepted), `float8` 701 (`float4`
accepted), `text` 25 (`varchar`, `bpchar`, `name` accepted), `numeric`
1700, `date` 1082, `timestamp` 1114, `timestamptz` 1184, `interval` 1186,
and their arrays; text and binary formats for all of them (binary numeric
and dates checked against PostgreSQL's `*_send`).

Not supported: `COPY`, cancellation.  `psql`'s `\d` family works through
the compatibility catalog (§8.11).

Tests: `crates/client/tests/client.rs` runs every slt script over the
nucleus protocol and checks typed values both ways, parameters, errors,
transactions and passwords; `crates/server/tests/wire.rs` runs every slt
script over the PostgreSQL protocol with a minimal client, checks password authentication and the SSL refusal,
and drives the extended protocol with the `postgres` crate (prepared
statements, typed and NULL parameters, binary results, SQLSTATE codes,
transactions, portals, `RETURNING`).  `scripts/psql-smoke.sh` (run by CI on
Linux) uses the real `psql`, including `\d`, `\d+`, `\dt`, `\di`, `\dn`,
`\l` and a foreign key's `Referenced by`.

## 10. The nucleus protocol (`ncp`)

nucleus's own client protocol (D14): what `nucleus server` speaks by
default and what nucleus's drivers (§11) implement.  This section is the
whole specification.

**Transport.** TCP, default port **20035** (`NC` in ASCII).  Every integer
is **little-endian**.  A *string* is a `u32` byte length and that many
bytes of UTF-8.  Passwords travel in clear text: use it on trusted
networks (TLS is later work).

**Frames.** Each message is a `u8` kind, a `u32` body length (the 5
header bytes not included; at most 1 GiB), then the body.

| Kind | Direction | Body |
|------|-----------|------|
| `H` Hello | client → server, first | `u16` version (1), string user, string database, string password (empty: none), string client name |
| `W` Welcome | server → client | `u16` version, string server (`nucleus 0.1.0`), `u32` session id |
| `Q` Query | client → server | string SQL, `u8` result format (0 typed, 1 text), `u16` parameter count, the parameters as values |
| `C` Columns | server → client | `u16` count, then per column: string name, type |
| `D` Row | server → client | one value per column |
| `K` Done | server → client | string command (`SELECT`, `INSERT`, `CREATE TABLE`, `BEGIN`, …), `u64` rows returned or affected (0 when it does not apply) |
| `E` Error | server → client | string SQLSTATE (5 characters), string message |
| `R` Ready | server → client | `u8` transaction status: 0 idle, 1 in a transaction block, 2 in a failed block |
| `X` Bye | client → server | empty; the server closes |

**Session.** The client sends Hello; the server answers Welcome and Ready,
or Error and closes (`28P01` wrong user or password — servers default to
user `nucleus`, password `nucleus`; `08P01` unsupported version or a
malformed message).  Then any number of Query messages, each answered
by, per statement, `[C D*] K` (Columns and Rows only for statements that
return rows), and finally Ready.  A failing statement sends Error instead
of Done, the remaining statements of that Query are skipped, and Ready
follows.  A Query with parameters must hold one statement; `$1`, `$2`, …
refer to them.  Transactions are SQL (`BEGIN`, `COMMIT`, `ROLLBACK`), as in
§8.7; outside a block every statement commits on its own.

**Types** (in Columns): a `u8` tag — 0 unknown (sent as text), 1 boolean,
2 bigint, 3 double precision, 4 text, 5 numeric, 6 date, 7 timestamp,
8 timestamptz, 9 interval, 10 array followed by the element's tag.

**Values** (in Rows and parameters): a `u8` tag, then:

| Tag | Value | Payload |
|-----|-------|---------|
| 0 | NULL | — |
| 1 | boolean | `u8` 0 or 1 |
| 2 | bigint | `i64` |
| 3 | double precision | `f64` (IEEE 754 bits) |
| 4 | text | string |
| 5 | numeric | `i128` mantissa (16 bytes, two's complement), `u8` scale: value = mantissa × 10^-scale |
| 6 | date | `i32` days since 2000-01-01 |
| 7 | timestamp | `i64` µs since 2000-01-01 00:00 |
| 8 | timestamptz | `i64` µs since 2000-01-01 00:00 UTC |
| 9 | interval | `i32` months, `i32` days, `i64` µs |
| 10 | array | `u32` count, then each element as a value |

This is the storage's own row encoding (§8.1), so values cross the wire
exactly as stored.  With result format 1 (text) every non-NULL value is
sent as text (tag 4), rendered as `ncsql` prints it.  Parameters are
converted to the type their place needs (a text parameter is parsed, as an
untyped literal would be).

## 11. Drivers (`drivers/`)

**Rust:** `nucleus-client` (`crates/client`, no dependencies).
`Client::connect(Config)` (defaults: `127.0.0.1:20035`, user, password and
database `nucleus`), `execute(sql, params) -> Reply` (columns, rows,
command, count), `rows(sql, params)`, `query(sql, params, Format)` for
several statements or text results, `status()`, `close()`.  `Value` covers
every nucleus type (numeric as mantissa and scale, dates and timestamps as
days and µs since 2000-01-01, intervals, arrays) with `From` conversions
for parameters.

**C:** `drivers/c` (`nucleus.h`, `nucleus.c`; C99, POSIX sockets or
Winsock).  `nc_connect(&cfg, &err)`, `nc_query(conn, sql, params, n, &err)`
(the last statement's result, or NULL with the SQLSTATE in `err`),
accessors (`nc_rows`, `nc_value_at`, `nc_column_type`, `nc_command`,
`nc_count`), `nc_format` (text as `ncsql` prints it), value constructors
(`nc_int`, `nc_text`, `nc_numeric`, `nc_date`, `nc_array`, …),
`nc_result_free`, `nc_close`.  Results own their values.

**C++:** `drivers/cpp/nucleus.hpp`, header-only C++17 over the C driver:
`nucleus::Connection` (RAII, movable), `query(sql, {params})` → `Result`,
`transaction(fn)`, `nucleus::Error` with `code()`; `nucleus::Value` is a
`std::variant` (NULL, bool, int64, double, string, `Numeric`, `Date`,
`Timestamp`, `TimestampTz`, `Interval`, `Array`).

**Python:** `drivers/python`, package `nucleus` (pure Python 3.8+).
`nucleus.connect()`, `conn.execute(sql, params)` → `Result` (`rows`,
`records()`, `columns`, `command`, `count`), `conn.query` for several
statements, `with conn.transaction():`, DB-API cursors; values as `bool`,
`int`, `float`, `Decimal`, `str`, `date`, `datetime` (naive / UTC),
`Interval`, `list`; errors are `nucleus.Error` subclasses by SQLSTATE class
(`IntegrityError` for 23, `ProgrammingError` for 42, …).

**JavaScript / TypeScript:** `drivers/js`, package `nucleus-db` (Node;
TypeScript source that Node 22.6+ runs directly, `npm run build` for
`dist/`).  `await connect()`, `query(sql, params)` → `Result`, `queryAll`,
`transaction(fn)`; queries on one connection are queued.  Values: integers
as `number` (`bigint` beyond 2^53), `Decimal` (exact, with `parse` /
`toString`), `Date` for dates and timestamps (UTC; millisecond precision
when read), `Interval`, arrays, `null`; errors are `NucleusError` with
`code`.

**Java:** `drivers/java`, package `io.github.davidsarosi92.nucleus` (Java
17+, `javac` or Maven).  `NucleusClient.connect(new Config())`,
`query(sql, params...)` → `Result` (`rows()`, `records()`), `queryAll`,
`transaction(fn)`, `NucleusException` with `code()`; values as `Boolean`,
`Long`, `Double`, `BigDecimal`, `String`, `LocalDate`, `LocalDateTime`,
`OffsetDateTime` (UTC), `Interval`, `List`.

**Tests and examples.**  Every driver has the same conformance cases
(each type both ways, NULL and arrays, text parameters, errors and their
SQLSTATE, transactions, a wrong password), and `examples/<language>/` has
the same quickstart in each.  `scripts/driver-tests.sh <nucleus binary>
[languages]` starts a server on a free port and runs them all; the CI job
`drivers` does so on Linux.

## 12. Change log

- **2026-10-17 — §M14.19 registries, first part:** the extension API
  (`ext`: `Module`, `ScalarFunction`, `CatalogView`, `Registry`); the
  built-in functions and `nc_catalog` register through it; PostgreSQL
  compatibility is the `pg-compat` module, in the PostgreSQL front end's
  sessions only (`nc_catalog` no longer goes through it); its scripts
  in `tests/slt-pg`.
- **2026-10-17 — §M14.19 principle audit, first fixes:** `now()` and
  retry back-off go through the platform (`Kv::wall_clock`,
  `Kv::back_off`, `Clock::sleep`); `Timestamp` is opaque; the SQL layer's
  format record (`00 00`); the SQL consistency check and crash test.

- **2026-10-17 — the data directory lock:** one process at a time per
  database (§4.6); tested on the OS and the simulation, in the KV contract
  and with a running server.
- **2026-10-17 — §M14.8 drivers:** C, C++, Python, JS/TS and Java beside
  the Rust one, each with conformance tests; `examples/` in every language;
  the CI job `drivers`.
- **2026-10-17 — §M14.7 the nucleus protocol (`ncp`):** the server's
  default protocol (port 20035, user / password `nucleus`); the Rust
  driver and `ncsql` use it; PostgreSQL's protocol is optional
  (`--pg-compat`).
- **2026-10-17 — §M14.6 `ncsql`:** the nucleus SQL shell, local or over the
  network, with `nc_catalog` commands; `nucleus-client`; the server's
  `--log-queries`.
- **2026-10-17 — §M14.5 SQL completeness:** EXCEPT / INTERSECT, FULL
  JOIN, CTEs (recursive too), NUMERIC and date/time types, arrays, regular
  expressions, functions in FROM, foreign keys (with validated reads in
  the KV contract), the `nc_catalog` system catalog and the PostgreSQL
  compatibility catalog for `psql`'s `\d`.

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
