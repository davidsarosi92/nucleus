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
| `crates/sql` | `nucleus-sql` | types, encodings, catalog (`nc_catalog`), parser, binder, optimizer, executor, sessions; the extension API (`ext`) | §M7–§M14.5, §M14.19 |
| `crates/server` | `nucleus-server` | front ends (the nucleus protocol), authentication | §M12, §M14.7, §M14.19 |
| `crates/client` | `nucleus-client` | the Rust driver | §M14.6–§M14.8 ✅ |
| `crates/testkit` | `nucleus-testkit` | the sqllogictest runner and the SQL layer's scripts, for the core, front ends and modules | §M14.19 |
| `modules/pg-compat` | `nucleus-pg-compat` | PostgreSQL compatibility: the protocol, `pg_catalog`, `information_schema`, the `pg_*` functions (its own README) | §M12, §M14.5, §M14.19 |
| `editions/nucleus` | `nucleus-cli` (binaries `nucleus`, `ncsql`) | the full edition: `nucleus kv`, `nucleus sql` / `ncsql`, `nucleus server` with every front end | §M6, §M12, §M14.6 ✅ |
| `integrations/d-os` | `nucleus-dos-settings` (binary `nucleus-settings`) | the d-os settings service and app manifest | §M6, §M6.5 ◐ |
| `drivers` | — | drivers for C, C++, Python, JS/TS, Java (§11) | §M14.8 ✅ |
| `examples` | `nucleus-example` (Rust) | the quickstart in every driver's language | §M14.8 ✅ |
| `examples/extension` | `nucleus-example-extension` (binary `geo-demo`) | the sample module `geo`: a type, a function, a catalog view; the guide for writing a module (its README) | §M14.19 ✅ |
| `scripts` | — | `package.sh`, `psql-smoke.sh`, `driver-tests.sh` | §M6, §M12, §M14.8 |

`crates/` is the core and the public API crates; `modules/` are modules,
each a folder built to leave the repository (its own README, changelog,
licence and version; dependencies on the public API crates only, by
version and path; in code, of `nucleus-sql` only `nucleus_sql::ext`);
`editions/` are the builds that choose modules.  `scripts/check-boundaries.py`
checks this both ways and `scripts/detach-test.sh` builds and tests every
module from its packaged form alone (§2).

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
(`scripts/psql-smoke.sh`, §9), the `drivers` job runs every driver's
tests and every example against a server (`scripts/driver-tests.sh`, §11),
and the `modules` job checks the module boundaries and detaches every
module (§1):

```sh
cargo build --release -p nucleus-cli
scripts/driver-tests.sh target/release/nucleus [rust python js java c cpp]
scripts/psql-smoke.sh target/release/nucleus     # needs psql
scripts/check-boundaries.py                      # core ↛ modules; modules → public API only
scripts/detach-test.sh [<module package> …]      # cargo package, then test each unpacked copy
```

The detachability test packages the core crates and the modules
(`cargo package`, which also builds each from its package against the
others'; in a target directory of its own, fresh each run, since cargo
reuses a registry crate of an unchanged version), unpacks each module
into an empty directory, points its nucleus dependencies at the packaged
core crates (`[patch.crates-io]`) and runs its tests there.

  The `i686` musl tests run as 32-bit binaries on a 64-bit Linux
kernel, which is what catches 64-bit `usize` assumptions.  The `tls` module's
cryptography (ring) has C and assembly, so a musl target needs a C
compiler for it: `musl-gcc` (Debian / Ubuntu `musl-tools`) for x86_64 and
aarch64, the multilib `gcc` for i686 — CI sets `CC_<target>`, e.g.
`CC_x86_64_unknown_linux_musl=musl-gcc`.

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
- `Rng::fill_unpredictable(buf)` — the operating system's generator on
  the real platform (UUIDs, tokens); the simulation's seed in a
  simulation.  `Kv::random_bytes` (§6.1) and `CallContext::random` reach it.
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
  `elapsed()` — monotonic time, for measuring how long something took
  (`EXPLAIN ANALYZE`, statement statistics; simulated in a simulation);
  `back_off(attempt)` — the wait before a retry (exponential with
  jitter, through the platform, so a simulation does not wait);
  `random_bytes(buf)` — unpredictable bytes from the platform.
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
| smallint | `SMALLINT`, `INT2` | 16-bit |
| integer | `INTEGER`, `INT`, `INT4` | 32-bit |
| bigint | `BIGINT`, `INT8` (and, with pg-compat, `OID`) | 64-bit |
| real | `REAL`, `FLOAT4` | 32-bit IEEE 754 |
| double precision | `DOUBLE PRECISION`, `FLOAT`, `FLOAT8` | 64-bit IEEE 754 |
| numeric | `NUMERIC [(p, s)]`, `DECIMAL`, `DEC` | exact: 128-bit mantissa and a scale, ≤ 38 digits |
| text | `TEXT`, `VARCHAR(n)` (and, with pg-compat, `NAME`) | `VARCHAR(n)` length enforced |
| character | `CHAR(n)`, `CHARACTER(n)` (`CHAR` is `CHAR(1)`; with pg-compat, `BPCHAR`) | blank-padded |
| bytea | `BYTEA` | a binary string |
| date | `DATE` | days since 2000-01-01, 4713 BC – 294276 AD |
| time | `TIME [WITHOUT TIME ZONE]` | µs since midnight, `00:00` – `24:00` |
| timestamp | `TIMESTAMP [WITHOUT TIME ZONE]` | µs since 2000-01-01 |
| timestamptz | `TIMESTAMPTZ`, `TIMESTAMP WITH TIME ZONE` | µs since 2000-01-01 UTC |
| interval | `INTERVAL` | months, days, µs (kept apart) |
| inet | `INET` | an IPv4 or IPv6 address, optionally with a mask length |
| cidr | `CIDR` | an IPv4 or IPv6 network (host bits zero) |
| arrays | `type[]` | one dimension, elements of the type or NULL |
| uuid | `UUID` | 16 bytes, a registered type (§8.12) |
| enums | `CREATE TYPE name AS ENUM (…)` | a label of the database's enum |
| registered types | their name (`geopoint`) | a module's type: its canonical bytes (§8.12) |

**`uuid`** is the first type built as a registered one (`uuid.rs`):
input as 32 hex digits in either case, a hyphen allowed after any group of
four, optionally in braces; output lower case `8-4-4-4-12`; equality,
hashing, ordering (byte order, as PostgreSQL's) and a fixed 16-byte key
encoding, so it can be a primary key or indexed; casts to and from
`text`; `min` / `max` (from its ordering — PostgreSQL has none).
Functions: `gen_random_uuid()` / `uuidv4()` (version 4, from the
platform's unpredictable randomness), `uuidv7()` (version 7: unix
milliseconds, 12 bits of the millisecond's fraction, random bits;
increasing within a process — the recommended key, new rows land at the
end of the index), `uuid_extract_version(u)` (for RFC 9562 variants),
`uuid_extract_timestamp(u)` (versions 1 and 7).  `crates/testkit/slt/20_uuid.slt`'s
expected results came from PostgreSQL 18.

**Integers by width.**  An integer literal is `integer` if it fits, else
`bigint` (PostgreSQL's rule).  Arithmetic on two integers has the wider
operand's type (`smallint + smallint` is `smallint`, `smallint + integer`
`integer`) and overflows at that width (`22003`, `integer out of range`);
so does a function whose result is a narrow integer (`abs(-32768::smallint)`).
Casts between widths and from `numeric` (half away from zero) or `double
precision` (half to even, as PostgreSQL's `rint`) check the range; text
input takes an optional sign, decimal digits, or `0x` / `0o` / `0b` ones,
`_` between digits (PostgreSQL 16), and spaces around — out of range is
`22003` (`value "40000" is out of range for type smallint`).  `sum` of
`smallint` or `integer` is `bigint`, `avg` of any integer `numeric`;
`count` is `bigint`; `length`, `strpos`, `array_length` and `date - date`
are `integer`.  Columns are range-checked on every write.  Values are one
64-bit integer in memory; the row encoding stores each in the fewest of 2,
4 or 8 bytes that holds it, and keys hold every width in 8, so an index
serves a comparison across widths and widening a column keeps its keys.
`crates/testkit/slt/23_integers.slt` (and pg-compat's `02_types.slt` for
the types, through `pg_typeof`) came from PostgreSQL 18.

**`real` and `double precision`.**  A float prints as PostgreSQL 12+
prints it: the shortest digits that read back to the same value, in fixed
notation when the leading digit's power of ten is from −4 up to 14 (a
double) or 5 (a real), else as `1.2345679e+08`; `-0`, `NaN`,
`Infinity`.  Input out of range — beyond the largest, or nonzero digits
that round to zero — is `22003` (`"1e40" is out of range for type
real`), as is an arithmetic result that overflows or underflows (`value
out of range: overflow`).  `real` with `real` is computed in 32 bits and
is `real`; `real` with any other number is `double precision`
(PostgreSQL has no mixed real operators), but `CASE`, `UNION` and
`greatest` unify `real` with integers and `numeric` as `real`.  `sum` of
reals is `real`, `avg` a double; `round`, `trunc`, `floor`, `ceil` and
`sign` of a real are a double's, and `round` of a float is half to even
(`rint`).  A real becomes `numeric` through 6 significant digits, a double
through 15.  A real is stored in 4 bytes (row tag 14); keys widen it to a
double's encoding, so an index on a real serves double bounds.
`crates/testkit/slt/24_real.slt` came from PostgreSQL 18.

**`bytea`** (`bytea.rs`): input as `\x` and hex digit pairs (spaces
between pairs allowed) or the escape form (characters as their UTF-8
bytes, `\\` a backslash, `\ooo` an octal byte); output the hex form
(`\xdeadbeef`); a text literal or text casts to it by its input form, and
it casts to text as its output.  Byte-wise order (a shorter prefix
first), key tag `0C` (its bytes escaped as text's), row tag `15`.  `||`,
`length` and `octet_length` (bytes), `substring` / `substr` (also
`substring(x FROM a FOR b)`), `position(sub IN x)`, `get_byte`,
`set_byte`, `encode(b, 'hex' | 'base64' | 'escape')` (base64 lines of 76
characters, as PostgreSQL's) and `decode`, `sha256` (bytea) and `md5`
(text or bytea, a hex text) — the digests built in, without a
dependency.  `crates/testkit/slt/25_bytea.slt` came from PostgreSQL 18.

**`char(n)`** follows the standard: a value is stored padded with blanks
to `n`; a longer one is `22001` unless the excess is blanks, and an
explicit cast truncates.  Trailing blanks do not count: in comparisons
(with another `char`, or as text with text), keys (stored without them),
`length`, `||` and the cast to text (which drop them), and functions
taking text; `octet_length` and `LIKE` see the padded value, as
PostgreSQL's.  `char` with `text` unifies to `text`.  Key tag `0E`, row
tag `18`.  Columns declared `char(n)` before §M14.26 were text and open
as text.  `crates/testkit/slt/27_char.slt` came from PostgreSQL 18.

**`inet` and `cidr`** (`inet.rs`): input as PostgreSQL's — dotted IPv4,
IPv6 with `::` and an embedded IPv4 tail, `/bits`; a `cidr` refuses host
bits set (`22P02`) and takes abbreviated IPv4 (`10`, `192.168/16`, the
mask from the address class when omitted).  Output: an `inet` without
`/bits` at a full mask, a `cidr` always with it, IPv6 lower case with the
longest run of zero groups as `::` and `::ffff:a.b.c.d` dotted.  Order:
IPv4 first, then the address masked to its mask length, the mask length,
the whole address — PostgreSQL's — in which a network's addresses are one
contiguous range; keys follow it (tag `0F`), and an `inet` equals a
`cidr` of the same value.  Operators `<<`, `<<=`, `>>`, `>>=`, `&&`;
functions `host`, `network`, `masklen`, `family`, `broadcast`, `netmask`,
`hostmask`, `abbrev`, `set_masklen`, `inet_same_family`; casts between
them (to `cidr` clears the host bits) and from text.  `x <<= net` and
`x << net` imply `x` lies between `net` and its broadcast address at a
full mask: the binder adds those bounds (the functions'
`implied_range`, §8.12), so an index on `x` serves containment as a
range scan.  `crates/testkit/slt/28_inet.slt` came from PostgreSQL 18.

**Enums** (`enums.rs`): `CREATE TYPE mood AS ENUM ('sad', 'ok',
'happy')`, ordered by declaration; `ALTER TYPE mood ADD VALUE [IF NOT
EXISTS] 'meh' [BEFORE | AFTER 'ok']`, `ALTER TYPE mood RENAME VALUE 'sad'
TO 'blue'`, `DROP TYPE [IF EXISTS] mood` (refused, `2BP01`, while a column
has it).  An enum is the database's, in its catalog (`00 09 <name>`), and
transactional as the rest of it.  Each label has a sort key, a base-256
fraction between 0 and 1 without trailing zero bytes (byte order is its
order); a value is stored as its key alone (key tag `10`, row tag `21`),
and there is always room between two keys, so a new label goes anywhere
and a renamed one keeps its key: neither rewrites a row (a test compares
the table's bytes).  Reading a row gives each value its label
(`enums::Labels`, one catalog read per enum and scan); text becomes an
enum where a transaction is at hand — binding, a cast as it runs,
assignment to a column — and a parameter for one arrives as its label.
Values of different enums do not compare (`42883`); an unknown label is
`22P02`.  Arrays of enums: not yet.  `nc_catalog.enums` lists the labels
with their keys.  `crates/testkit/slt/29_enum.slt` came from PostgreSQL
18.

**`time`**: input as `HH:MM[:SS[.f]]`, `HHMM`, `HHMMSS`, with `AM` /
`PM`, or `allballs` (a zone after it is ignored, as PostgreSQL's `time`
does; `24:00:00` is the end of the day, a leap second rounds into it);
out of range is `22008`, bad syntax `22007`.  `time ± interval` (the
interval's time part, around the clock), `time - time` (an interval),
`date + time` (a timestamp); casts from timestamps and intervals (their
time of day) and to an interval; `extract` / `date_part` of `hour`,
`minute`, `second`, `milliseconds`, `microseconds`, `epoch` (others are
`0A000`, as PostgreSQL's); `make_time(h, m, s)`, `localtime`.  Key tag
`0D`, row tag `17`.  `time(p)` is accepted and µs are kept (as for
timestamps); `time with time zone` is refused (`0A000`) — `timestamptz`
covers what it would.  `crates/testkit/slt/26_time.slt` came from
PostgreSQL 18.

`INSERT … VALUES` binds each value against its target column, as
PostgreSQL does: an untyped literal takes the column's type, and each row's
value is converted as it is stored rather than unified with the other rows'
(`VALUES ('-inf'), (1.5)` into a `real` column).

Before widths (table record version 5 and older), `INT`, `INTEGER` and
`SMALLINT` columns were 64-bit with `BIGINT`'s column tag, so the catalog
cannot tell them apart: such a database opens with them as `bigint`, its
values intact; `ALTER TABLE … ALTER c TYPE integer` narrows one, checking
every value.  Likewise `REAL` columns were `double precision` and open as
one.

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
`01`, a `NULL` element as `02` — after the others, as comparisons have
it — then `00`); `0B` a registered type (its stable id, then its own key
encoding).  Key encoding version 2 (the format record, §8.2): version 1
wrote a `NULL` array element as `01 00`, before the others — the law kit
caught the disagreement — and the first write to a version-1 database
rewrites the keys of every table with an array in a key
(`dml::rewrite_keys`; the decoder reads both).  Row encoding: version `02`, column count, tagged values
(missing trailing columns read as NULL, so `ADD COLUMN` needs no rewrite
unless it has a default); an integer is tag `12` (2 bytes), `13` (4) or
`2` (8), the fewest that hold it (version `01`, still read, had only `2`);
a real is tag `14`, a bytea `15`; a registered type's value is tag `11`,
its id, its encoding version and its canonical bytes; and tag `16` (a
`u32` length) is a value stored out of line.

**Large values out of line** (`large`).  A value whose encoding is longer
than 2 000 bytes (`large::THRESHOLD`) — a file in a `bytea`, a document
in a `text`, any type — is stored in chunks of up to 32 KiB under the
table's reserved index id `FFFFFFFF`, keyed by the row's primary key, the
column and the chunk number; the row keeps tag 16 and the length, and
reads (`large::read_row`) put the value back.  The chunks are written and
removed with their row, in its transaction: a snapshot sees the chunks of
the row version it sees, a rollback or a crash leaves none behind (the
crash test writes large values), a changed primary key moves them, and
dropping the table removes them.  An update that leaves a large value as
it was writes none of its chunks.  The consistency check finds a value
missing chunks and chunks without their value.  A registered type's id is
the FNV-1a hash of its name, the same in every process (two names with
one id are refused when registered).

**Every type is a `Type`** (§8.12), the built-in ones included: the
binder, DDL and aggregates ask a type's abilities before relying on them,
and refuse what it lacks at bind or DDL time, naming the ability — `type
geopoint has no ordering, which ORDER BY needs; it has a distance: ORDER
BY x <-> value` (`42883`), `… no key encoding, which a primary key needs`
(`42704`).  Equality: `=`, `<>`, `IN`, simple `CASE`, `JOIN USING`;
equality and hashing: `DISTINCT` (also in aggregates), `GROUP BY`, `UNION`
(not `ALL`), `INTERSECT`, `EXCEPT`; ordering: `<` …, `ORDER BY` (also in
aggregates), `min`, `max`; key encoding (and equality for a primary key
or `UNIQUE`): keys and indexes.  The built-in types have every ability
(their values are native, and the hot paths run native code).

### 8.2 Catalog and keyspace (`catalog`)

| Key | Value |
|-----|-------|
| `00 00` | the format record: keyspace layout (1) and key encoding (2) versions, `u16` each |
| `00 01 <table name>` | table definition (columns, primary key, indexes, checks) |
| `00 02 <sequence>` | counter (`table_id`) |
| `00 03 <table id>` | next hidden row id (tables without a primary key) |
| `00 04 <table id>` | statistics (§8.8) |
| `00 05 <type> 00 <ordering>` | an ordering taught to a type: its mapping (SQL over `value`) |
| `00 06 <symbol> 00 <left> 00 <right>` | an operator taught: its function |
| `00 07 <from> 00 <to>` | a cast taught: its function and context |
| `00 08 <type>` | the oldest encoding version a registered type's stored values may be in |
| `00 09 <enum>` | an enum: its labels and their sort keys, in order |
| `00 0A <role>` | a role: its attributes and password verifier (§9) |
| `00 0B <role> 00 <member>` | a membership: its admin option |
| `00 0C <kind> <object>` | an object's ACL (privileges per grantee, and per column) |
| `00 0D <role> 00 <kind>` | a role's default privileges for objects it makes |
| `00 0E <table> 00 <policy>` | a row-level security policy |
| `00 0F <login>` | a token: its role, ceiling, expiry and secret's verifier (§9) |
| `01 <table id> 00000000 <pk>` | the row |
| `01 <table id> <index id> <index values> [<pk>]` | index entry → encoded pk |
| `01 <table id> FFFFFFFF <pk> <column u16> <chunk u32>` | a chunk of a value stored out of line |

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
NULL`, index entries exactly as the rows imply, unique indexes, values
stored out of line whole and no chunks without one, foreign keys, no keys
of unknown tables or indexes).

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

Not yet: window functions, `COPY`, `TIMETZ`, time zones other than
UTC, multi-dimensional arrays, data-modifying CTEs, `LATERAL`, user
functions.

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
`EXPLAIN` prints the plan (columns as `#n`); `EXPLAIN ANALYZE` runs it
too (§8.13).

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

- The SQL layer's sqllogictest scripts live in the test kit,
  `crates/testkit/slt/*.slt` (one per area, numbered: basics,
  expressions, joins, aggregates, subqueries, indexes, constraints,
  transactions, plans, statistics, … through the types, roles,
  privileges, row-level security, tokens, access rules, protection and
  the analyzer),
  embedded in `nucleus-testkit` (`core_scripts()`) with its runner
  (`nucleus_testkit::slt`), so the client's tests and modules run them
  without reading files outside their folders.  `tests/slt.rs` runs them
  locally, `SLT=<name>` one of them.  The expected results of scripts
  12–18 were produced by PostgreSQL 16 (records marked `nucleus-only`
  excepted: concurrency, and what PostgreSQL does not do).
  `no_postgresql_without_pg_compat` checks that a session without the
  module sees nothing of PostgreSQL's catalog.
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

### 8.11 System catalogs (`ncatalog`; `modules/pg-compat`)

nucleus's own catalog is **`nc_catalog`**: read-only views made from the
catalog when scanned (D13), registered by the built-in module (§8.12).

| Table | Rows |
|-------|------|
| `nc_catalog.schemas` | `name`, `description` (the session registry's schemas) |
| `nc_catalog.tables` | `table_id`, `name`, `columns`, `primary_key` (names), `has_row_id`, `indexes`, `checks`, `foreign_keys`, `referenced_by`, `key_prefix` (hex), `analyzed_rows`, `owner` |
| `nc_catalog.columns` | `table_id`, `table_name`, `position`, `name`, `type`, `nullable`, `default_expr`, `in_primary_key` |
| `nc_catalog.indexes` | `table_id`, `table_name`, `index_id` (0 = primary), `name`, `columns`, `is_unique`, `is_primary`, `key_prefix`, `orderings` (per column, the taught ordering it is indexed in, or NULL) |
| `nc_catalog.constraints` | `table_id`, `table_name`, `name`, `kind`, `definition` |
| `nc_catalog.foreign_keys` | `table_id`, `table_name`, `name`, `columns`, `referenced_table`, `referenced_columns`, `on_delete`, `on_update` |
| `nc_catalog.statistics` | `table_id`, `table_name`, `column_name`, `rows`, `nulls`, `distinct_values`, `histogram` |
| `nc_catalog.orderings` | `type`, `name`, `mapping`, `used_by` (the indexes in it) |
| `nc_catalog.operators` | `symbol`, `left_type`, `right_type`, `function`, `source` (`distance`: a type's ability; `registered`: a module's; `taught`) |
| `nc_catalog.casts` | `from_type`, `to_type`, `function`, `context` |
| `nc_catalog.enums` | `type`, `label`, `position`, `sort_key` (hex) |
| `nc_catalog.roles` | `name`, `superuser`, `login`, `inherit`, `createrole`, `connection_limit`, `has_password`, `valid_until` |
| `nc_catalog.role_members` | `role`, `member`, `admin_option`, `condition` (a `WHEN` condition, or NULL) |
| `nc_catalog.access_rules` | `position`, `name`, `roles`, `addresses`, `front_ends`, `auth_methods`, `tls`, `action`, `read_only`, `as_role` (in order) |
| `nc_catalog.audit` | `seq`, `at`, `kind` (`login`, `ddl`, `statement`), `ok`, `code`, `session_user_name`, `user_name`, `client_addr`, `front_end`, `auth_method`, `tls`, `access_rule`, `detail`, `hash` (an administrator's; empty for others) |
| `nc_catalog.audit_chain` | `records`, `intact`, `broken_at`, `problem` (the chain walked) |
| `nc_catalog.database_settings` | `name`, `value`, `is_set`, `default`, `description` (`ALTER DATABASE nucleus SET …`) |
| `nc_catalog.statements` | `user_name`, `query` (constants as `$n`), `calls`, `errors`, `total_ms`, `mean_ms`, `min_ms`, `max_ms`, `rows_read`, `rows_returned`, `last_call` (statement statistics, §8.13; an administrator sees all, others their own) |
| `nc_catalog.sessions` | `id`, `user_name`, `session_user_name`, `client_addr`, `front_end`, `auth_method`, `tls`, `access_rule`, `read_only`, `as_role`, `roles` (active at login), `started` (the sessions a front end admitted; an administrator sees all, others their own) |
| `nc_catalog.policies` | `table_name`, `name`, `permissive`, `command`, `roles`, `using`, `with_check` |
| `nc_catalog.tokens` | `name`, `login`, `role`, `read_only`, `tables`, `valid_until`, `created`, `created_by`, `max_rows`, `max_scan`, `timeout`, `rate_per_minute` (the tokens the current role may manage; never a secret) |
| `nc_catalog.grants` | `kind`, `object`, `column`, `grantee`, `privilege`, `grantable` (the ACLs written out) |
| `nc_catalog.types` | `name`, `aliases`, `category`, `module` (which registered it), `equality`, `hashing`, `ordering`, `key_encoding`, `distance` (its operator), `encoding_version` |

**PostgreSQL compatibility: the `pg-compat` module** (`modules/pg-compat`,
`nucleus_pg_compat::PgCompat`, §8.12).  Only sessions whose registry has it see any of what follows —
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
indexes and constraints numbered after it (the module's `catalog.rs` has
the table).
One role, `nucleus`.  This is what makes `psql`'s `\d`, `\d table`,
`\d+`, `\dt`, `\di`, `\dv`, `\dn`, `\du`, `\l` work; `\df` and others
that need functions, sequences or permissions do not.  `pg_typeof(x)` is
one of its functions too.

**Teaching a type an ordering** (§M14.19): `ALTER TYPE t ADD ORDERING
name BY mapping` — `mapping` is SQL over `value` (a `t`), into a type with
an order, and must be immutable (no subqueries, parameters, or functions
that are not `Volatility::Immutable`: `22023` otherwise).  Values then
sort as their mappings: `ORDER BY x USING name [DESC]` (in aggregates
too), and an index can hold the mappings: `CREATE INDEX [name] ON t (col
USING name)` (not `UNIQUE`: a mapping may have ties, which the primary key
breaks; such an index is kept current by every write but is no access
path for predicates on the column).  `ALTER TYPE t ALTER ORDERING name BY
mapping` redefines it and rebuilds the indexes in it; `ALTER TYPE t DROP
ORDERING [IF EXISTS] name` is refused while an index uses it (`2BP01`,
naming the index) unless `CASCADE`, which drops those indexes.  A plain
`ORDER BY` of a type without an order names the orderings taught to it.
**Collations are text orderings:** `x COLLATE name` sorts and compares
(`=`, `<`, …) in text ordering `name` — `C`, `default`, `POSIX`,
`ucs_basic` are text's own byte order, any other one taught (`ALTER TYPE
text ADD ORDERING ci BY lower(value)`); the value itself is unchanged.
Orderings live in the catalog (key `00 05`), so teaching is transactional
DDL; indexes record them per column (table record version 5).
`CREATE INDEX` without a name makes one up (`table_columns_idx`).

**Encoding versions** (registered types): each stored value carries its
type's encoding version.  A value in an older one is upgraded as it is
read (`Type::upgrade(from, bytes)`), and writes use the current one, so
old and new values live side by side; `ALTER TYPE t UPGRADE` rewrites
every row and key holding `t` in one transaction.  The catalog records the
oldest version stored (`00 08`; `nc_catalog.types.stored_version`).  An
upgrade that changes the type's keys (`Type::upgrade_changes_keys`) is
done by the first transaction that may write, before anything reads those
keys (a read-only statement before it gets `25006`); a database storing a
version newer than the build is refused (`0A000`).  `tests/upgrade.rs`
plays successive builds of one type, crashes `UPGRADE` at every I/O
operation (all old or all new afterwards), and changes keys.

**`ALTER TABLE t ALTER [COLUMN] c [SET DATA] TYPE type [USING expr]`**:
every value converted — by `expr` over the row, else by a taught cast of
context `ASSIGNMENT` or `IMPLICIT`, else a built-in assignment cast
(between numbers, between dates and timestamps, to text; anything else
needs `USING`: `42804`) — and stored as the column now says (lengths,
precision), every row, key and index entry rewritten, checks, defaults,
keys and uniqueness holding afterwards (`22001`, `23505`, `23514`).  One
transaction: readers keep the old column until it commits.  A column a
foreign key uses cannot change type yet.  `22_alter_column_type.slt`'s
results came from PostgreSQL 18.

**Operators outside the grammar's own.**  The lexer reads operators as
PostgreSQL does — the longest run of `+-*/<>=~!@#%^&|`?`, not into a
comment, a run of more than one not ending in `+` or `-` unless it holds
one of `~!@#%^&|`?` — so `a=-1` is `=` and `-`, `a<->b` one operator.
Those the grammar does not know (`<->`, `@>`) all parse at one precedence,
PostgreSQL's for "any other operator" (that of `||`), and the binder finds
them by symbol and operand types: a type's distance (its `Distance`
ability's operator: `ORDER BY at <-> '(47.5,19.04)'`), a module's
(`Registry::add_operator`), or one taught: `ALTER TYPE t ADD OPERATOR
symbol (left, right) [RETURNS type] AS function` / `DROP OPERATOR [IF
EXISTS] symbol (left, right)` (the grammar's own operators cannot be
taught: `22023`).  **Casts taught:** `ALTER TYPE t ADD CAST TO target AS
function [EXPLICIT | ASSIGNMENT | IMPLICIT]` / `DROP CAST [IF EXISTS] TO
target`, between types one of which is registered (the built-in casts are
fixed); `CAST(x AS target)` and `x::target` then call the function, and
`ALTER COLUMN … TYPE` without `USING` takes an `ASSIGNMENT` or `IMPLICIT`
one.  (`INSERT` and `UPDATE` do not apply taught casts unwritten yet.)  Naming a type's distances
in SQL (`ADD DISTANCE`) comes with the nearest-neighbour indexes of
§M14.12; today a type's distance is its ability's.
`crates/testkit/slt/21_orderings.slt` and `tests/types.rs` (`geopoint`
taught `by_geohash`) test it.

### 8.12 Extension API (`ext`, `builtin`)

What a module adds to the SQL layer (principle 12), versioned
(`ext::API_VERSION`, 1).  **The guide for writing one is
[`examples/extension/README.md`](examples/extension/README.md)**, with the
sample module `geo` (`geopoint`, `geohash`, `geo.cities`) built on this
API alone, embedded and as a custom server build (`geo-demo serve`).

- `Module` — `name()`, `register(&mut Registry)`.
- `ScalarFunction` — `name()` (for `EXPLAIN`), `arity()`,
  `resolve(&[ArgInfo]) -> Signature` (bind-time typing: per argument
  `Keep`, `Untyped(t)` — an untyped literal, `NULL` or parameter takes
  `t` —, `Convert(t)` or `Common`; the result a type, the arguments'
  common type, or a `Constant`), `strict()` (default true: a `NULL`
  argument gives `NULL` without a call), `volatility()` (`Immutable`,
  `Stable`, `Volatile` — the default; only immutable functions may define
  an ordering), `implied_range()` (for a function of two arguments true
  only when the first lies in a range computed from the second: two
  functions giving the bounds; the binder adds the comparisons beside the
  call, for index ranges — `inet <<= cidr` uses it), `call(&CallContext,
  &[Value])`.
  `CallContext` gives the statement's transaction, the registry,
  `now()`, `clock()` and `random(buf)` (all through the database's
  platform).
- `Aggregate` — `name()`, `arity()`, `accepts_star()` (`count(*)`),
  `keeps_nulls()` (default false: rows whose first argument is `NULL` are
  skipped), `resolve` (as a function's), `start() -> Box<dyn
  Accumulator>` (`add(&[Value])`, `finish()`), one per group.  The engine
  does the grouping, `DISTINCT` (over the arguments) and `ORDER BY` inside
  the call.
- `TableFunction` — `name()`, `arity()`, `resolve(&[ArgInfo]) ->
  TableSignature` (per argument, and the output columns; one column for
  now, named after the function or its alias), `rows(&CallContext,
  &[Value])`.  `ext::common_type` is the binder's rule for `Common`
  arguments, for a `resolve` that needs it.
- `CatalogView` — `schema()`, `name()`, `columns()`, `rows(&CallContext)`.
- `Type` — `name()`, `aliases()`, `category()`, `input(text)`,
  `output(&Value)`, and its abilities, each its own interface it has or
  not: `equality()` (`Equality`), `hashing()` (`Hashing`), `ordering()`
  (`TotalOrder`), `key_encoding()` (`KeyEncoding`: order-preserving,
  self-delimiting), `distance()` (`Distance`: an operator and a
  distance); `encoding_version()`; `examples()` (on which registration
  checks the laws: a type that breaks one is not registered).  A
  registered type's values are
  `Value::Ext(TypeRef, bytes)` — `Value::ext(t, bytes)`, `v.ext_bytes()`
  — whose bytes are canonical: equality and hashing are on them
  (`CanonicalBytes` implements both), so they agree by construction; its
  type is `DataType::Ext(TypeRef)`.  `TypeRef` is the type interned once
  per name (small, `Copy`): `name()`, `id()`, `imp()`,
  `TypeRef::named(name)`.  The catalog stores a registered type by name;
  a database naming a type this process has never registered still opens
  (`TypeRef::named_or_missing`: values print as hex, input fails naming
  the missing type).  `ncp` sends registered types as text for now (their
  binary form with §M14.19's `UUID`), `pg-compat` as `text`.
- `Registry` — `empty()`, `builtin()` (the `builtin` module: every
  built-in function, aggregate and table function, the schemas `public`
  and `nc_catalog`, the `nc_catalog` views), `shared_builtin()`,
  `add(&dyn Module)` (refused, and the registry unchanged, if the module
  or anything it registers is already there), `add_function(names, f)`,
  `add_aggregate(names, a)` (functions and aggregates share one
  namespace), `add_table_function(f)`, `add_schema(name,
  description, searched)` (a searched schema's views are found by
  unqualified names after the user's tables, as `pg_catalog`'s),
  `add_view(v)`, `add_type(t)`, `add_alias(name, ty)` (pg-compat's
  `oid`, `name`, `bpchar`, …), and the lookups `function`, `aggregate`,
  `table_function`, `view`, `type_named`, `schemas`, `views`, `types`,
  `modules`.  A session can name only its registry's types; a type is
  written `name`, `schema.name`, `name 'literal'` or `x::name`, and
  `x::name` where `name` is no type but a function `__name` exists calls
  it (pg-compat's `::regclass`).

**Laws** (`nucleus_sql::laws`, run property-based by the test kit's
`nucleus_testkit::laws`): for each ability a type has — equality is an
equivalence; hashing agrees with equality; the order is total and agrees
with equality; the key encoding's byte order is the type's order, it
decodes back and is self-delimiting — and always: text output parses back
to an equal value, the row encoding reads back what it wrote, and the
engine's own comparisons and hashing (`Value`'s) agree with the type's.
`Registry::add_type` checks them on the type's examples;
`laws::check(ty, generator, cases)` on generated values (`laws::Gen`, a
seeded generator; `laws::generator(ty)` for the built-in types, their
arrays and `uuid`).  `crates/sql/tests/laws.rs` runs the kit on every
built-in type and array, and shows a broken type caught and refused.

`tests/extension.rs` is a module from outside the crate adding a
function, an aggregate, a table function and a view, used through SQL;
`tests/types.rs` one adding the `geopoint` type (equality and a distance,
no order, no key encoding) — stored, reread, compared, grouped, and
refused exactly where an ability is missing.

A session runs against one registry: `Session::new(kv)` the built-in one,
`Session::with_registry(kv, registry)` any.  The binder finds functions
and views there; bound calls and view scans hold the registered objects,
so execution needs no lookup.  The built-in functions share one
implementation behind the same interface.

### 8.13 The SQL analyzer (`profile`, `statements`)

**`EXPLAIN ANALYZE <query>`** (also `EXPLAIN (ANALYZE [bool], TIMING
[bool]) …`; other options are refused with `0A000`) runs the query and
prints the plan with, on every node, the optimizer's estimate beside what
the node did: `(rows=40) (actual rows=38 loops=1 time=0.120 ms)` — rows
over all its starts, the starts (a nested loop's inner side, a correlated
subquery: once per outer row), and the time spent in it, its inputs
included.  A node the run never reached says `(never executed)`; the last
line is `Execution: n rows in t ms`.  `TIMING OFF` leaves the times out
(repeatable output, for tests).  Time is the database's monotonic clock
(`Kv::elapsed`).  The query runs with the session's privileges, budget and
row-level security, like the query itself; only queries for now (`UPDATE`
and `DELETE` have plain `EXPLAIN`).  A large gap between `rows=` and
`actual rows=` points at stale or missing statistics (`ANALYZE`).

**Statement statistics** (`nc_catalog.statements`): every statement a
front end (or `Session::execute`) gave the text of, per role
(`current_user`) and normalized text — constants replaced by `$n`
(numbered after the statement's own parameters), white space as one
space; case and spacing do not split a statement, and the text shown is
the first run's.  Per statement: calls, failures, total, mean, shortest
and longest time (ms), rows read (scanned) and rows returned or changed,
the last call.  An administrator sees every role's, anyone else their
own; `nc_statements_reset()` forgets every role's (an administrator) or
the caller's own.  They live in the process, by database (a restart
starts over), at most 5000 statements (the least called makes room);
statements of the database's own probe sessions are not counted.

## 9. Server (`nucleus-server`)

```
nucleus server <dir> [--listen <addr:port>] [--pg-compat [<addr:port>]]
                     [--front-end <name> [<addr:port>]]
                     [--user <name>] [--password <pw>] [--no-auth] [--log-queries]
```

The server speaks **the nucleus protocol** (§10, `nucleus_server::serve`)
on `--listen` (default `127.0.0.1:20035`).  **Who logs in is the
database's roles** (`roles.rs`, §M14.25): `CREATE ROLE` / `CREATE USER`
(a role with `LOGIN`) `[WITH] SUPERUSER | LOGIN | INHERIT | CREATEROLE |
CONNECTION LIMIT n | PASSWORD 'p' | VALID UNTIL 't' | IN ROLE r, …` (and
their `NO…` forms), `ALTER ROLE … options | RENAME TO`, `DROP ROLE [IF
EXISTS]` (refused while it owns a table, `2BP01`, or is the session's,
`55006`), `GRANT role TO role [WITH ADMIN OPTION]` (a cycle is
`0LP01`), `REVOKE [ADMIN OPTION FOR] role FROM role`, `SET ROLE r` /
`RESET ROLE`.  Passwords are kept as SCRAM-SHA-256 verifiers
(`nucleus-crypto`: SHA-256, HMAC, PBKDF2, 4096 iterations), never in
clear.  An administrator (`SUPERUSER`) may do everything; `CREATEROLE`
manages roles other than administrators, and a role it makes is
administered by its maker; a role may change its own password; anyone
else gets `42501`.  `current_user` is the role a statement runs as,
`session_user` the one that logged in (`access::with_who`, as `now()`'s
time); a table's owner is its maker (`nc_catalog.tables.owner`, table
record version 7).

**Privileges** (`privileges.rs`): `GRANT privilege [(columns)], … | ALL
ON [TABLE] t, … | ALL TABLES IN SCHEMA public | SCHEMA s | FUNCTION f
[(args)] | TYPE t | DATABASE nucleus TO role | PUBLIC, … [WITH GRANT
OPTION]`, `REVOKE [GRANT OPTION FOR] … FROM …` (on a table also from its
columns), `ALTER DEFAULT PRIVILEGES [FOR ROLE r] [IN SCHEMA public] GRANT
| REVOKE … ON TABLES …`.  Each object has an ACL — written out on its
first `GRANT` or `REVOKE`, until then its kind's default: the owner has
everything; `PUBLIC` may `EXECUTE` functions, use types and schema
`public`, and `CONNECT` to the database; nothing else (so making a table
or a type needs `CREATE` on schema `public`, as in PostgreSQL 15).
Administrators and owners are never refused; privileges come through
inherited roles and `PUBLIC`.  A user's statement is bound with checks
(`Binder::checked`): reading a table needs `SELECT` on it or on some
column, and every column read — in the select list, `*`, `WHERE`, joins,
`RETURNING`, an `UPDATE`'s expressions — `SELECT` on it; `INSERT` on the
columns written, `UPDATE` on those set, `DELETE` on the table;
`EXECUTE` on each function and aggregate called (internal binding —
constraints, defaults, mappings — checks nothing).  DDL is checked before
it runs (`authorize_ddl`): changing, dropping or indexing a table, or
altering or dropping a type, is its owner's; a foreign key needs
`REFERENCES` on the columns it references; a column's registered or enum
type needs `USAGE`.  A login needs `CONNECT`.  Granting needs the
owner's rights or the grant option (else `42501` — where PostgreSQL
warns that nothing was granted).  A refusal is `42501` naming the
privilege and the object (`permission denied: SELECT on column salary of
table pay`), never a value; `EXPLAIN` is checked as the statement is.
Dropping a role is refused while it holds privileges.
`crates/testkit/slt/31_privileges.slt` was checked against PostgreSQL 18
in one session (as another role too).

**Row-level security** (`policies.rs`): `ALTER TABLE t ENABLE | DISABLE
ROW LEVEL SECURITY`, `… [NO] FORCE ROW LEVEL SECURITY`, `CREATE POLICY p
ON t [AS PERMISSIVE | RESTRICTIVE] [FOR ALL | SELECT | INSERT | UPDATE |
DELETE] [TO roles | PUBLIC] [USING (cond)] [WITH CHECK (cond)]`, `DROP
POLICY [IF EXISTS] p ON t` (the table owner's).  On a table with it, for
a role that is neither an administrator nor — unless forced — the owner,
a command touches only rows passing `(its permissive USING conditions,
OR-ed) AND (its restrictive ones)` — no permissive policy, no row — and
new rows (`INSERT`, `UPDATE`'s result) must pass the `WITH CHECK` ones (or
`USING` where none is given), else `42501` "new row violates row-level
security policy".  Conditions see the row and the session:
`current_user`, `current_setting('nucleus.tenant')` (`SET name = value`,
dotted names allowed, `RESET name | ALL`; `current_setting(name,
missing_ok)` is the core's).  The binder applies the policies wherever a
user's statement reads a table, as a barrier, `Plan::Secure` ("Row
Security" in `EXPLAIN`): the optimizer may narrow the scan by key ranges
from the policy and from the statement's bounds on a column by a value
free of the row (evaluated once, on no row), but no other predicate of the
statement is evaluated on a row before the policies have passed it —
`UPDATE` and `DELETE` likewise filter by the policy first — so an error,
a count, a join, a subquery or a side effect reveals nothing of a hidden
row (`crates/testkit/slt/32_row_security.slt`, the leak cases among
them, agrees with PostgreSQL 18 run in one session).

**Tokens** (`tokens.rs`): API keys for programs and AI agents.  `CREATE
TOKEN name [FOR role] [READ ONLY] [ON [TABLE] t, …] [VALID UNTIL 't']`
returns one row — `name`, `login` (`nct_` and 16 hex digits) and
`secret` (64 hex digits), shown this once; only the secret's
SCRAM-SHA-256 verifier is stored.  A client logs in with `login` as its
user and `secret` as its password, so every driver, `psql` and any
PostgreSQL client take a token as they are (and prove it by SCRAM like
any password).  The session acts as the token's role (`current_user`,
`session_user`) under the token's *ceiling* (`access::Ceiling`), which
narrows the role's privileges and holds through `SET ROLE`: `READ ONLY`
runs every transaction read only (a write is `25006`); `ON TABLE …`
admits no other table and no new one (`42501`).  Tokens may be made for
a role that cannot log in.  A role makes and drops its own tokens; a
member with the admin option, those of its role; an administrator, any
— an administrator's tokens only an administrator.  A token session
cannot make tokens or set passwords (`42501`), so no credential outlives
a revoked token.  `DROP TOKEN [IF EXISTS] name` revokes one; dropping
its role drops it, renaming follows; a dropped table leaves the tokens
scoped to it; after `VALID UNTIL` it does not log in (`28P01`, as any
refusal).  Role names starting `nct_` are reserved.  Row, time and cost
budgets come with §M14.25's protection step (T6); schema and branch
scopes when there are schemas and branches.

**Access rules** (`rules.rs`): `CREATE ACCESS RULE name [FOR role, … |
FOR ALL] [FROM 'cidr', …] [VIA ncp | pg, …] [AUTH password | token |
certificate | oidc | ldap | trust, …] [WITH TLS | WITHOUT TLS] {ALLOW
[READ ONLY] [AS ROLE r] | REJECT} [BEFORE rule | AFTER rule]`, `DROP
ACCESS RULE [IF EXISTS] name` (an administrator's).  After a client
authenticates, the rules are tried in order and the first whose
conditions all hold decides — its role (the user or a role it is a member
of), the client's address, the front end, the method it authenticated by,
TLS: `REJECT`, or no rule matching, refuses the login (`28000`, naming the
rule or saying none admits it); `ALLOW` admits it under the rule's
ceiling, `READ ONLY` (every transaction read only, `25006`) and `AS ROLE
r` (every privilege must be `r`'s too — a ceiling, never more).  With no
rules every login is admitted; a local session (`ncsql <dir>`, an
embedded database) is under none, the way back in after a lockout.
Dropping a role takes it out of the rules (a rule left for no role, or
whose ceiling role went, is dropped).  **Conditional memberships:**
`GRANT role TO member [WITH ADMIN OPTION] WHEN (condition)` — a boolean
over the connection, `source` (`inet`, the client's address), `auth`,
`front_end`, `tls` (`source <<= '10.2.0.0/16' AND auth = 'certificate'`),
checked when made (`42703`, `42804`) — holds only in a session whose
connection meets it, decided at login: inherited privileges, `SET ROLE`
and access rules' `FOR` see it only then; granted again without `WHEN`, it
holds always.  **The connection** is the session's, read only (`55P02` on
`SET`): `current_setting('nucleus.client_addr')`, `nucleus.front_end`,
`nucleus.auth_method`, `nucleus.tls` (`on` / `off`),
`nucleus.access_rule` — so a row-level security policy can use it too —
and `nc_catalog.sessions` lists the sessions; `ncsql`'s `\conninfo` shows
them.  Behind a load balancer, `--trusted-proxy <cidr>` (repeatable,
`Config::trusted_proxies`) takes the client's address from the PROXY
protocol header (HAProxy's, version 1 or 2) of a proxy in it — which must
send one; nobody else's is read (`transport::client_address`).  An
address alone is weak identity (shared networks, NAT, a spoofing
neighbour): for more than read access a rule should require TLS or a
certificate too.

**Protection.**  *The database's settings*: `ALTER DATABASE nucleus SET
name = value | RESET name` (an administrator's; `dbsettings.rs`, catalog
`00 11`, `nc_catalog.database_settings`) — `password_min_length`
(default 1), `password_lifetime` (an interval, default `infinity`),
`statement_timeout` (default `0`, none), `login_lock_failures` (10),
`login_lock_time` (`15 minutes`); a duration is milliseconds, `5s`,
`100ms`, `2min`, `1h`, `1d` or an interval.  *Passwords*: at least
`password_min_length` characters and not the role's name (`22023`; a
verifier given as such, from a dump, is not checked); one set without
`VALID UNTIL` expires after `password_lifetime`.  *Failed logins*
(`nucleus_server::throttle`): from the third in a row, by user name or by
client address, an answer waits a quarter second, doubling up to four;
after `login_lock_failures` the role is locked for `login_lock_time` —
even its password is refused (`28000`); a success clears the count;
unknown names count alike; the counts are the server's memory.
*Connection limits*: a role's `CONNECTION LIMIT` (administrators aside)
and the server's `--max-connections` (`Config::max_connections`) — past
either, `53300`.  *Statement budgets* (`budget.rs`): `SET
statement_timeout = …` (or the database's) ends a statement that runs
longer (`57014`), checked as rows are read or made — scans, table
functions, recursive rounds; a token's `ROWS n` (the most rows a
statement returns), `SCAN n` (the most it reads) — `54000` — `TIMEOUT
'…'` and `RATE n PER MINUTE` (statements, `53400`): `CREATE TOKEN … ROWS
1000 SCAN 100000 TIMEOUT '5s' RATE 60 PER MINUTE`.  *An insecure start*:
`nucleus server` refuses to listen beyond loopback without
authentication, or with the default password while the database is
unclaimed, unless `--insecure`.

**The audit log** (`audit.rs`; catalog `00 12 <seq>`, its head `00
13`): every login — admitted, with the rule, or refused, with why (a
wrong password, a token not accepted, a rule, a limit) — and every
statement that changes the catalog (`ddl`: tables, types, indexes, roles,
grants, policies, tokens, rules, settings), always; statements as the
database's settings ask (`audit_statements = all`, `audit_roles = 'a,b'`
— those roles' and their members', `audit_tables = 't,u'` — those that
touch these tables, by the privilege checks' lookups).  A record holds
when, who (session and current user), the connection (address, front end,
method, TLS, rule), what (the statement's text) and whether it went
through (the SQLSTATE if not).  It is written in the transaction of what
it records — a statement rolled back leaves none, a committed one always
has one — and a refusal, a read or a login in one of its own.  Each
record's hash is SHA-256 over its fields and the previous record's hash;
`nc_catalog.audit_chain` walks the chain: a changed record, a gap, or
records cut off the end (the head names more) show, with where.  A local
session's DDL is recorded too.  Records are not pruned yet; a stream of
them comes with §M16.5.

**Bootstrap.**  A database whose catalog has no administrator that can
log in with a password is *unclaimed*: the server's `--user` /
`--password` (default `nucleus` / `nucleus`) log in as an administrator,
so a fresh server and client work together unconfigured.  `nucleus init
<dir> [--user <name>]` claims it — creates the first administrator, the
password from `NUCLEUS_PASSWORD` or standard input — or one is made with
`CREATE USER … SUPERUSER PASSWORD …`; from then on only the catalog's
roles log in (a wrong user or password, a role without `LOGIN` or an
expired password: `28P01`, not told apart).  `--no-auth` accepts any
user that is a role (any name while unclaimed).
Passwords are proved by SCRAM-SHA-256 and never sent (`ncp` version 4,
and the PostgreSQL front end; older `ncp` clients still send theirs in
clear).  **TLS** (the `tls` module, `modules/tls`, over rustls):
`--tls-cert <pem> --tls-key <pem>` offers it on every front end to
clients that ask — `ncp` clients by starting a TLS handshake (§10),
PostgreSQL clients by `SSLRequest` (`sslmode=require`, `verify-full`);
clients that do not ask still connect in plain text until access rules
can require it (§M14.25 T5).  TLS 1.2 and 1.3.  **Client certificates**
(mutual TLS): `--tls-client-ca <pem>` asks clients for a certificate of
that CA — not required, but one that is sent must verify, or the
handshake fails; a verified certificate whose subject common name is the
user logging in stands in for the password, for a role with `LOGIN`
(while unclaimed, the configured user) and never for a token
(`Authenticator::accepts_certificate`, `certificate_login`); a
certificate naming someone else changes nothing — the password is asked
as before.  **OpenID Connect** (the `oidc` module, `modules/oidc`):
`--oidc-issuer <url> --oidc-audience <name> [--oidc-jwks <file>]
[--oidc-user-claim <claim>] [--oidc-role-claim <claim>]` takes identity
tokens (JWTs) of that issuer through OAUTHBEARER (§10) as well as
passwords: the signature is checked against the issuer's keys (the file's,
or found by OpenID Connect Discovery over HTTPS and fetched again for an
unknown key, at most once a minute) — RS256/384/512, PS256/384/512,
ES256, ES384, EdDSA, never `none` — with `iss`, `aud`, `exp` and `nbf` (a
minute of leeway).  The user claim (`sub` by default) must name a role
that exists and may log in, whether the database is claimed or not
(`28000` otherwise, as for any refused token); the role claim's roles are
granted for the session (`access::Who::session_roles`: those that exist
and are not administrators, while the session acts as itself, and `SET
ROLE` to them); every claim is `current_setting('nucleus.claims.<name>')`
(names in lower case; an array's items joined by commas), which `SET`
cannot change (`55P02`) — so a policy can use a tenant claim
(`Session::login_as`, `access::Identity`, `nucleus_server::Bearer`).
**LDAP** (the `ldap` module, `modules/ldap`): `--ldap-url <url>
(--ldap-bind-dn <template with {user}> | --ldap-search-base <dn>
[--ldap-search-attribute <attr>] [--ldap-search-dn <dn>
--ldap-search-password <pw>]) [--ldap-starttls] [--ldap-ca <pem>]` — a
role that may log in and has no password of its own has it checked by the
directory (a simple bind as the template's DN, the name escaped, or as the
one entry a search finds; an empty password is never sent).  The server
must then receive the password, so it asks for it only over TLS: `PLAIN`
on `ncp` (§10), the cleartext method on pg-compat; without TLS such a login
is refused (`28000`) before a password is sent, and an `ncp` client before
version 4, which sends it in Hello, is refused too.  Roles with a password
keep SCRAM (`nucleus_server::Directory`, `directory_user`).  One thread per connection.  `--log-queries`
(`Config::log_queries`) prints every statement a client sends, and every
error, to stderr — to see what a tool asks for.

**Front ends and authentication are registered** (principle 12): a
front end is a `FrontEnd` (`name`, `default_addr`, `handle(stream, kv,
config, id)`) — the built-in `Ncp` in `FrontEnds::builtin()`, `Pg` from
the pg-compat module, which the `nucleus` edition adds;
`serve_front_end(front, listener, kv, config)` runs any.  The server
starts `ncp` and each `--front-end <name>` (`--pg-compat` is `--front-end
pg`).  `Config { auth, registry, log_queries, tls }` (`tls`: a
`transport::Tls`, `Config::with_tls`; the front end negotiates it and
reads and writes through the stream it returns; `TlsInfo` says what was
agreed, and `client_name` who a verified client certificate names): `auth` is an `Authenticator`
(`describe`, `wants_password`, `accepts(kv, user, password)`) —
`Password` (the catalog's roles, or while unclaimed one user;
`Config::password`, the default `nucleus` / `nucleus`) or `Trust`
(`Config::trust`, `--no-auth`); after it accepts, a front end logs the
session in (`Session::login`).  For SCRAM it gives a user's verifier
(`verifier(kv, user)`: the catalog role's, with `LOGIN` and `VALID UNTIL`
checked, or while unclaimed one derived from the configured password),
and `fake_salt` / `scram_nonce` for the exchange; `ncp` version 4 uses it
(§10).  Further methods (tokens, certificates, OIDC, LDAP) are §M14.25's
later steps.  `Config::registry` is what sessions have (the
built-ins by default; a custom build adds its modules —
`Config::with_registry`); pg-compat's front end adds its module on top.

**PostgreSQL compatibility** (optional, D14): `--pg-compat` also listens
for the PostgreSQL protocol (`nucleus_pg_compat::Pg`, default
`127.0.0.1:5432`), with the same credentials, so `psql` and PostgreSQL
drivers and tools can connect.  Its sessions have the `pg-compat` module
(§8.11); `ncp` sessions do not.  It is frozen: kept working, not extended
for the catalog queries particular tools send (some, like Valentina
Studio, ask for PostgreSQL internals nucleus does not have).  PostgreSQL
protocol 3.0:
SSL requests accepted (`S`, then TLS) when the server has TLS, else
declined (`N`), as are GSS requests; the request is read unbuffered, so
nothing sent before the handshake counts as sent inside it; trust or
SCRAM-SHA-256 authentication — over TLS SCRAM-SHA-256-PLUS too, offered
first, which libpq takes by default: the proof is bound to the TLS
session (`tls-server-end-point`, RFC 5929: the server certificate hashed
by its signature's hash; certificates whose signature names no hash, as
Ed25519, get no binding), so a man in the middle cannot relay it, and a
client claiming it cannot bind (`y,,`) is refused where the server can
(`channel_binding=require` works)
(SASL, without channel binding; never a cleartext password; `28P01` on
failure), parameter status (`server_version`
16.0 (nucleus), UTF8, ISO dates), backend key data, `ReadyForQuery` with
the transaction status.  Simple query protocol (statements one by one, the
first error stops the rest) and extended protocol: Parse (parameter types
inferred, or given), Bind (text or binary parameters), Describe (statement
and portal), Execute with a row limit (`PortalSuspended`), Sync, Close,
Flush; after an error, messages are skipped until Sync.  Types on the wire:
`bool` 16, `int8` 20 (`int2`/`int4` accepted), `float8` 701 (`float4`
accepted), `text` 25 (`varchar`, `bpchar`, `name` accepted), `numeric`
1700, `date` 1082, `timestamp` 1114, `timestamptz` 1184, `interval` 1186,
and their arrays, `uuid` 2950 (binary: its 16 bytes); other registered
types as `text`; text and binary formats for all of them (binary numeric
and dates checked against PostgreSQL's `*_send`).

Not supported: `COPY`, cancellation.  `psql`'s `\d` family works through
the compatibility catalog (§8.11).

Tests: `crates/client/tests/client.rs` runs every slt script over the
nucleus protocol and checks typed values both ways, parameters, errors,
transactions and passwords; `modules/pg-compat/tests/wire.rs` runs every
slt script (the kit's and the module's) over the PostgreSQL protocol with
a minimal client, checks password authentication and the SSL refusal,
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
bytes of UTF-8.  From version 4 the password never travels: the client
proves it with SCRAM-SHA-256 (RFC 5802 / 7677); before 4 it is sent in
Hello in clear text.  **TLS:** a client that wants it starts a TLS
handshake as soon as it is connected, before Hello; the server sees the
handshake's first byte (`0x16`, never a frame kind) and, if it has TLS,
answers it — every frame then travels inside TLS; a server without TLS
closes the connection.  No round trip is spent on asking, and the version
of the protocol does not change.

**Frames.** Each message is a `u8` kind, a `u32` body length (the 5
header bytes not included; at most 1 GiB), then the body.

| Kind | Direction | Body |
|------|-----------|------|
| `H` Hello | client → server, first | `u16` version (4; 1–3 still accepted), string user, string database, string password (version 4: empty — it is proved, not sent; before 4: the password, empty for none), string client name |
| `A` Authentication | server → client | string: the mechanisms, space-separated (`SCRAM-SHA-256`; over TLS with a channel binding `SCRAM-SHA-256-PLUS SCRAM-SHA-256`), then SCRAM's server-first and server-final messages |
| `a` Authentication response | client → server | string: SCRAM's client-first, then client-final message — or OAUTHBEARER's initial response, or PLAIN's |
| `W` Welcome | server → client | `u16` version (the lower of the client's and the server's), string server (`nucleus 0.1.0`), `u32` session id |
| `Q` Query | client → server | string SQL, `u8` result format (0 typed, 1 text), `u16` parameter count, the parameters as values |
| `C` Columns | server → client | `u16` count, then per column: string name, type |
| `D` Row | server → client | one value per column |
| `K` Done | server → client | string command (`SELECT`, `INSERT`, `CREATE TABLE`, `BEGIN`, …), `u64` rows returned or affected (0 when it does not apply) |
| `E` Error | server → client | string SQLSTATE (5 characters), string message |
| `R` Ready | server → client | `u8` transaction status: 0 idle, 1 in a transaction block, 2 in a failed block |
| `X` Bye | client → server | empty; the server closes |

**Session.** The client sends Hello.  If the server wants a password and
the client speaks version 4, they run SCRAM-SHA-256: the server sends `A`
with the mechanisms — `SCRAM-SHA-256`, and over TLS, when the server's
certificate defines a `tls-server-end-point` value (RFC 5929: the
certificate hashed by its signature's hash, SHA-256 for MD5 and SHA-1),
`SCRAM-SHA-256-PLUS SCRAM-SHA-256` — or, for a role whose password a
directory checks (LDAP), over TLS only, `PLAIN` instead of SCRAM — and with
an OpenID Connect issuer `OAUTHBEARER` after them.  PLAIN (RFC 4616): one
`a` with `<NUL><user><NUL><password>`; a client sends it only over TLS.  A client with an identity token takes
OAUTHBEARER: one `a` with RFC 7628's initial response
(`n,,\x01auth=Bearer <token>\x01\x01`); the token names the user (Hello's is
not used), and the server answers Welcome or Error (`28000`, with why).
Otherwise the client `a` client-first
(`<gs2>n=,r=<nonce>`, the user being Hello's), whose GS2 header picks:
`p=tls-server-end-point,,` binds the proof to this TLS session (so a man in
the middle, who has a session of its own with each side, cannot relay
it), `y,,` says the client could bind but was not offered it (refused by
a server that can — the offer was stripped on the way), `n,,` no binding;
the server `A` server-first (`r=<nonce>,s=<salt>,i=<iterations>`); the
client `a` client-final (`c=<base64 of the GS2 header, and the binding
data after it when bound>,r=<nonce>,p=<proof>`); the server `A` server-final
(`v=<signature>`), which the client checks — a server that cannot sign
does not know the password's verifier, and the client hangs up.  An
unknown user gets a salt of its own (derived from a server secret), so
the exchange does not tell which users exist.  The server then answers
Welcome and Ready, or Error and closes (`28P01` wrong user or password —
an unclaimed database takes the server's credentials, by default user
`nucleus`, password `nucleus`; `08P01` unsupported version or a malformed
message, an `a` out of turn included).  Then any number of Query messages, each answered
by, per statement, `[C D*] K` (Columns and Rows only for statements that
return rows), and finally Ready.  A failing statement sends Error instead
of Done, the remaining statements of that Query are skipped, and Ready
follows.  A Query with parameters must hold one statement; `$1`, `$2`, …
refer to them.  Transactions are SQL (`BEGIN`, `COMMIT`, `ROLLBACK`), as in
§8.7; outside a block every statement commits on its own.

**Versions.**  2 adds registered types (`uuid`, a module's type): tag 11
below.  3 adds tags 12–20, §M14.26's types.  4 proves the password by
SCRAM-SHA-256 instead of sending it (§M14.25).  An older client gets what
its version knows — registered types as text before 2; before 3,
`smallint` and `integer` as `bigint` (tag 2), `real` as a double (tag 3),
`bytea`, `time`, `char(n)`, `inet`, `cidr` and enums as text — so older
drivers keep working.

**Types** (in Columns): a `u8` tag — 0 unknown (sent as text), 1 boolean,
2 bigint, 3 double precision, 4 text, 5 numeric, 6 date, 7 timestamp, 8
timestamptz, 9 interval, 10 array followed by the element's tag, 11 a
registered type followed by its name (a string: `uuid`); version 3: 12
smallint, 13 integer, 14 real, 15 bytea, 16 time, 17 `char(n)`, 18 inet,
19 cidr, 20 an enum followed by its name.

**Values** (in Rows and parameters): a `u8` tag, then:

| Tag | Value | Payload |
|-----|-------|---------|
| 0 | NULL | — |
| 1 | boolean | `u8` 0 or 1 |
| 2 | bigint | `i64` |
| 3 | double precision (and, for now, real) | `f64` (IEEE 754 bits) |
| 4 | text | string |
| 5 | numeric | `i128` mantissa (16 bytes, two's complement), `u8` scale: value = mantissa × 10^-scale |
| 6 | date | `i32` days since 2000-01-01 |
| 7 | timestamp | `i64` µs since 2000-01-01 00:00 |
| 8 | timestamptz | `i64` µs since 2000-01-01 00:00 UTC |
| 9 | interval | `i32` months, `i32` days, `i64` µs |
| 10 | array | `u32` count, then each element as a value |
| 11 | a registered type (version 2) | string type name, `u32` length, the value's binary (canonical) form — for `uuid` its 16 bytes |
| 12 | smallint (version 3) | `i16` |
| 13 | integer (version 3) | `i32` |
| 14 | real (version 3) | `f32` (IEEE 754 bits) |
| 15 | bytea (version 3) | `u32` length, the bytes |
| 16 | time (version 3) | `i64` µs since midnight (`0` – `86 400 000 000`) |
| 18, 19 | inet, cidr (version 3) | `u8` family (4 or 6), `u8` mask length, the address (4 or 16 bytes, network order) |

A `char(n)` value and an enum's are text (tag 4): their column types say
what they are.  An integer goes as the column's width (a `bigint`, or an
expression's integer of no column, as tag 2); a parameter may use any
integer tag.

Tags 0–10 are the tags of the storage's row encoding version 1 (§8.1), but
the protocol's encoding is its own (`native::wire_value`, `parameter`):
the storage may store a value differently without the protocol
changing.  A cidr parameter with host bits set is refused (`22P02`).  A registered type's carries its name instead
of its id, so drivers need no table of ids.  A tag-11 parameter of a type
the server does not know is refused (`42704`).  With result format 1 (text) every non-NULL value is
sent as text (tag 4), rendered as `ncsql` prints it.  Parameters are
converted to the type their place needs (a text parameter is parsed, as an
untyped literal would be).

## 11. Drivers (`drivers/`)

**Rust:** `nucleus-client` (`crates/client`, no dependencies).
`Client::connect(Config)` (defaults: `127.0.0.1:20035`, user, password and
database `nucleus`), `execute(sql, params) -> Reply` (columns, rows,
command, count), `rows(sql, params)`, `query(sql, params, Format)` for
several statements or text results, `status()`, `close()`.  `Value` covers
every nucleus type (`SmallInt(i16)`, `Integer(i32)`, `Int(i64)`,
`Real(f32)`, `Float(f64)`, numeric as mantissa and scale, `Bytes`,
dates and timestamps as days and µs since 2000-01-01, `Time` as µs since
midnight, intervals, `Inet` / `Cidr { addr: IpAddr, bits }`, arrays,
`Uuid([u8; 16])`, `Other { type_name, bytes }` for another registered
type; `char(n)` and enum values as text, their columns typed `Char` and
`Enum(name)`) with `From` conversions for parameters.  Every driver
speaks protocol version 4 and logs in by SCRAM-SHA-256 (the C driver with
its own SHA-256, HMAC, PBKDF2 and base64; the others with their
platform's).  **TLS** in each: the server's certificate checked against
the platform's CA certificates (Rust: Mozilla's, through the `tls`
module) or a CA file of one's own, and the host name; or unverified
(encrypted, not authenticated).  Rust: `Config::tls` with
`nucleus_tls::ClientTls::{new, with_ca_file, without_verification}`
(the client crate names only the `nucleus_client::Tls` interface); C:
`cfg.tls`, `cfg.tls_ca`, `cfg.tls_no_verify` in a build with
`NC_WITH_OPENSSL` (`make TLS=openssl`; `nc_tls_supported()`), C++ the
same fields on `nucleus::Config`; Python `connect(tls=True, tls_ca=…,
tls_verify=False)` (`ssl`); JavaScript `{ tls, tlsCa, tlsVerify }`
(`node:tls`); Java `Config.tls(true)`, `tlsCa(path)`, `tlsVerify(false)`
(JSSE).  A client certificate (mutual TLS): Rust
`ClientTls::with_client_certificate_files`, C / C++ `tls_cert`,
`tls_key`, Python `tls_cert=`, `tls_key=`, JavaScript `tlsCert`,
`tlsKey`, Java `tlsCertificate(cert, key)` (a PKCS #8 key); `ncsql
--tls-cert <pem> --tls-key <pem>`.  An identity token (OAUTHBEARER) instead
of the password: Rust `Config::token`, C / C++ `token`, Python `token=`,
JavaScript `token`, Java `Config.token(…)`, `ncsql --token` or `NCTOKEN`.
Every driver answers PLAIN (a directory's password) and only over TLS.

**C:** `drivers/c` (`nucleus.h`, `nucleus.c`; C99, POSIX sockets or
Winsock).  `nc_connect(&cfg, &err)`, `nc_query(conn, sql, params, n, &err)`
(the last statement's result, or NULL with the SQLSTATE in `err`),
accessors (`nc_rows`, `nc_value_at`, `nc_column_type`, `nc_command`,
`nc_count`), `nc_format` (text as `ncsql` prints it), value constructors
(`nc_int`, `nc_text`, `nc_numeric`, `nc_date`, `nc_array`, `nc_uuid_value`,
`nc_ext`, `nc_smallint`, `nc_integer`, `nc_real`, `nc_bytea`, `nc_time`,
`nc_inet`, `nc_cidr`, …), `nc_uuid_parse`, `nc_inet_parse`,
`nc_result_free`, `nc_close`.  `uuid` is `NC_UUID` (an `nc_uuid` of 16
bytes), another registered type `NC_EXT` (name and bytes;
`nc_column_type_name`); `NC_SMALLINT` / `NC_INTEGER` in `as.integer`,
`NC_REAL` in `as.real4`, `NC_BYTEA` a pointer and length, `NC_TIME` µs
since midnight, `NC_INET` / `NC_CIDR` a family, mask length and address;
`NC_CHAR` and `NC_ENUM` columns (an enum's name by
`nc_column_type_name`) hold `NC_TEXT` values.  Results own their values.

**C++:** `drivers/cpp/nucleus.hpp`, header-only C++17 over the C driver:
`nucleus::Connection` (RAII, movable), `query(sql, {params})` → `Result`,
`transaction(fn)`, `nucleus::Error` with `code()`; `nucleus::Value` is a
`std::variant` (NULL, bool, int64, double, string, `Numeric`, `Date`,
`Timestamp`, `TimestampTz`, `Interval`, `Array`, `Uuid`, `Ext`,
`SmallInt`, `Integer`, `Real`, `Bytes`, `Time`, `Inet`, `Cidr` — the last
seven wrappers, so an `int` literal stays a bigint parameter).

**Python:** `drivers/python`, package `nucleus` (pure Python 3.8+).
`nucleus.connect()`, `conn.execute(sql, params)` → `Result` (`rows`,
`records()`, `columns`, `command`, `count`), `conn.query` for several
statements, `with conn.transaction():`, DB-API cursors; values as `bool`,
`int`, `float` (a real by its shortest digits), `Decimal`, `str`, `bytes`,
`date`, `time` (24:00:00 as `time.max`), `datetime` (naive / UTC),
`Interval`, `ipaddress` interfaces (inet) and networks (cidr),
`uuid.UUID`, `list`, `nucleus.Ext` (another registered type); errors are `nucleus.Error` subclasses by SQLSTATE class
(`IntegrityError` for 23, `ProgrammingError` for 42, …).

**JavaScript / TypeScript:** `drivers/js`, package `nucleus-db` (Node;
TypeScript source that Node 22.6+ runs directly, `npm run build` for
`dist/`).  `await connect()`, `query(sql, params)` → `Result`, `queryAll`,
`transaction(fn)`; queries on one connection are queued.  Values: integers
and reals as `number` (`bigint` beyond 2^53; a real by its shortest
digits), `Decimal` (exact, with `parse` / `toString`), `Buffer` (bytea),
`Date` for dates and timestamps (UTC; millisecond precision when read),
`Time`, `Interval`, `Inet` / `Cidr`, uuid as its canonical string, `Ext`
(another registered type), arrays, `null`; errors are `NucleusError` with `code`.

**Java:** `drivers/java`, package `io.github.davidsarosi92.nucleus` (Java
17+, `javac` or Maven).  `NucleusClient.connect(new Config())`,
`query(sql, params...)` → `Result` (`rows()`, `records()`), `queryAll`,
`transaction(fn)`, `NucleusException` with `code()`; values as `Boolean`,
`Short`, `Integer`, `Long`, `Float`, `Double`, `BigDecimal`, `String`,
`byte[]`, `LocalDate`, `LocalTime`, `LocalDateTime`, `OffsetDateTime`
(UTC), `Interval`, `Inet` / `Cidr` (over `InetAddress`), `UUID`, `Ext`
(another registered type), `List`.

**Tests and examples.**  Every driver has the same conformance cases
(each type both ways, NULL and arrays, text parameters, errors and their
SQLSTATE, transactions, a wrong password, TLS verified, unverified and
refused for an unknown CA, a login by client certificate, and one by a
test issuer's token — `scripts/fixtures/oidc`, whose key was thrown away
once the token was made — and by a directory's password,
`scripts/fake-ldap.py`, over TLS and refused without), and `examples/<language>/` has the same
quickstart in each.  `scripts/driver-tests.sh <nucleus binary>
[languages]` starts a server on a free port — with TLS, from a CA and a
certificate it makes with `openssl`, when that is installed — and runs
them all; the CI job `drivers` does so on Linux.  `scripts/psql-smoke.sh`
checks `psql` with and without TLS.

## 12. Change log

- **2026-10-23 — §M14.9 `EXPLAIN ANALYZE` and statement statistics (A1,
  A2):** every plan node measured (rows, starts, time) beside the
  estimates; `nc_catalog.statements` per role and normalized statement,
  `nc_statements_reset()`; `Kv::elapsed`, the monotonic clock.
- **2026-10-23 — §M14.25 audit (T7) — §M14.25 shipped:** a hash-chained
  audit log in the catalog: logins, DDL and every change of access,
  statements by role or table as the database's settings ask; written with
  what it records; `nc_catalog.audit` and `audit_chain`, which find an
  edit, a gap or a cut end.
- **2026-10-23 — §M14.25 protection (T6):** database settings (`ALTER
  DATABASE … SET`), password length / name / lifetime rules, failed
  logins slowed and locked, connection limits, statement timeouts and
  token budgets (rows, scan, rate), the insecure start refused.
- **2026-10-23 — §M14.25 access rules (T5):** `CREATE / DROP ACCESS
  RULE` (role, address, front end, method, TLS; `ALLOW [READ ONLY] [AS
  ROLE r]` or `REJECT`; first match), `GRANT … WHEN (…)` memberships held
  only under their condition, the connection as read-only settings,
  `nc_catalog.sessions` and `access_rules`, the PROXY protocol from
  trusted proxies; tested over real sockets from several addresses.
- **2026-10-23 — §M14.25 LDAP (T4):** the `ldap` module — passwords of
  roles without their own checked by a directory (bind, or search and
  bind; LDAPS, StartTLS), asked for only over TLS (`PLAIN` on `ncp`, the
  cleartext method on pg-compat); every driver answers PLAIN, over TLS.
- **2026-10-23 — §M14.25 OpenID Connect (T4):** the `oidc` module —
  identity tokens through OAUTHBEARER on `ncp`, their claims as read-only
  settings (`nucleus.claims.*`, `55P02`), a role claim's roles for the
  session; `--oidc-*`, `ncsql --token`; a token option in every driver.
  A bearer login needs an existing role that may log in even while the
  database is unclaimed.
- **2026-10-23 — §M14.25 SCRAM-SHA-256-PLUS on `ncp` (T4):** over TLS the
  server offers `SCRAM-SHA-256-PLUS SCRAM-SHA-256`; every driver binds its
  proof to the session (`tls-server-end-point`), or sends `y` when the
  offer is missing.
- **2026-10-23 — §M14.25 SCRAM-SHA-256-PLUS (T4):** pg-compat binds SCRAM
  to the TLS session (`tls-server-end-point`; `TlsInfo::channel_binding`),
  checked with `psql channel_binding=require` on RSA SHA-256, RSA SHA-384
  and ECDSA SHA-384 certificates.
- **2026-10-23 — §M14.25 client certificates (T4):** `--tls-client-ca`;
  a verified certificate naming the user logs it in without a password,
  on both front ends; client certificates in every driver and `ncsql`.
- **2026-10-23 — §M14.25 TLS (T4):** the `tls` module (rustls): `ncp`
  clients start a handshake before Hello, PostgreSQL clients send
  `SSLRequest`; `--tls-cert` / `--tls-key`, `ncsql --tls | --tls-ca |
  --tls-no-verify`; TLS in all six drivers (C with OpenSSL behind
  `NC_WITH_OPENSSL`); the core gains only `transport` (`Stream`, `Tls`).
  The Rust client tries every address a host name has.
- **2026-10-23 — §M14.25 tokens (T4):** `CREATE TOKEN … [READ ONLY] [ON
  TABLE …] [VALID UNTIL …]`, `DROP TOKEN`; a login name and a secret,
  proved by SCRAM in every driver and `psql`; the session runs as the
  token's role under its ceiling (`access::Ceiling`, kept through `SET
  ROLE`); `nc_catalog.tokens`.
- **2026-10-23 — §M14.25 SCRAM on the wire (T4):** `ncp` version 4 —
  the password is proved by SCRAM-SHA-256 (`A` / `a` messages), never
  sent; the server signs back; unknown users get a stable fake salt.
  Every driver speaks it; older clients still log in with a cleartext
  password.  pg-compat logs in by SCRAM-SHA-256 too (PostgreSQL's SASL;
  checked with `psql`'s `require_auth=scram-sha-256`).
- **2026-10-23 — §M14.25 row-level security (T3):** policies (permissive
  and restrictive, per command and role, `USING` / `WITH CHECK`),
  `ENABLE` / `FORCE ROW LEVEL SECURITY`, a barrier node the optimizer
  respects, `current_setting` and `RESET` in the core;
  `nc_catalog.policies`.
- **2026-10-23 — §M14.25 privileges (T2):** `GRANT` / `REVOKE` on
  tables and columns, schemas, functions, types and the database, `WITH
  GRANT OPTION`, `PUBLIC`, default privileges; checked when a user's
  statement is bound and before DDL; enum owners; `nc_catalog.grants`.
- **2026-10-23 — §M14.25 users and roles (T1):** roles in the catalog
  (`CREATE / ALTER / DROP ROLE`, `CREATE USER`, membership with
  inheritance and admin option, `SET ROLE`), SCRAM-SHA-256 password
  verifiers (`nucleus-crypto`), table owners, logins checked against the
  catalog once a database is claimed (`nucleus init`), `current_user` /
  `session_user`; `nc_catalog.roles`, `role_members`; pg-compat's
  `pg_roles`, `pg_auth_members`, `relowner`.
- **2026-10-22 — §M14.26 shipped.**  Core types: integers by width,
  `real`, `bytea` and large values out of line, `time`, `char(n)`,
  `inet` / `cidr`, enums; `ncp` version 3 in every driver.
- **2026-10-22 — §M14.26 `ncp` version 3:** native tags for the new
  types; every driver speaks it (Rust, C, C++, Python, JavaScript, Java),
  each with the types both ways in its conformance tests; older clients
  get what their version knows.
- **2026-10-22 — §M14.26 enums:** `CREATE TYPE … AS ENUM`, `ADD VALUE
  … BEFORE / AFTER` and `RENAME VALUE` without rewriting a row (sort keys
  with room between them), `DROP TYPE`, `nc_catalog.enums`.
- **2026-10-22 — §M14.26 `inet` / `cidr`:** addresses and networks,
  containment operators as index range scans (`ScalarFunction::
  implied_range`), the network functions; pg-compat's inet and cidr.
- **2026-10-22 — §M14.26 `char(n)`:** blank-padded text with the
  standard's rules; pg-compat's bpchar.
- **2026-10-22 — §M14.26 `time`:** a time of day (input forms, `24:00`,
  arithmetic with dates, intervals and times, `extract`, `make_time`,
  `localtime`); pg-compat's time.
- **2026-10-22 — §M14.26 large values out of line:** values over 2 000
  bytes, of any type, stored in chunks keyed by their row; kept by
  updates that leave them; in the consistency check and the crash test;
  `repeat()`.
- **2026-10-22 — §M14.26 `bytea`:** binary strings (hex and escape
  forms, `encode` / `decode`, `sha256`, `md5`, the byte functions),
  `position(… IN …)` and `substring(… FROM … FOR …)` in the grammar;
  pg-compat's bytea.
- **2026-10-18 — §M14.26 `real`:** a 32-bit float beside `double
  precision`, PostgreSQL's float output and range errors for both, float
  overflow and underflow checked, `round` half to even; `INSERT … VALUES`
  bound against the target columns; pg-compat's float4.
- **2026-10-18 — §M14.26 integers by width:** `smallint`, `integer`,
  `bigint` as types of their own (ranges, overflow per width, promotion,
  `integer` literals, PostgreSQL 16's integer input); row encoding
  version 2; `ncp`'s own value encoder; pg-compat's int2 / int4.

- **2026-10-18 — §M14.19 shipped: the sample extension (T7).**
  `examples/extension` (the `geo` module, `geo-demo`, the guide for
  writing a module), held to the module rules by the boundary check;
  `nucleus_server::Config::registry`; every driver and edition folder with
  its own changelog, licence and version.
- **2026-10-18 — §M14.19 encoding versions and `ALTER COLUMN … TYPE`
  (T6):** values upgraded as read, `ALTER TYPE … UPGRADE` (crash-tested),
  key-changing upgrades before the keys are used, newer versions refused;
  `ALTER TABLE … ALTER COLUMN … TYPE … [USING]`.
- **2026-10-17 — §M14.19 operators and casts (T5b):** PostgreSQL's
  operator lexing; operators outside the grammar's own at one precedence,
  resolved by symbol and types — a type's distance (`<->`), a module's
  (`Registry::add_operator`), or taught (`ALTER TYPE … ADD / DROP
  OPERATOR`); casts taught (`… ADD / DROP CAST TO`);
  `nc_catalog.operators`, `nc_catalog.casts`.
- **2026-10-17 — §M14.19 teaching orderings (T5, orderings):** `ALTER
  TYPE … ADD / ALTER / DROP ORDERING … BY mapping`, `ORDER BY … USING`,
  `COLLATE` as a text ordering, indexes in an ordering (rebuilt when it is
  redefined, guarding its drop), `nc_catalog.orderings`; function
  volatility.
- **2026-10-17 — §M14.19 the law kit (T4):** `nucleus_sql::laws` and
  `nucleus_testkit::laws`; registration checks a type's examples.  It
  caught arrays' key encoding putting a `NULL` element first (comparisons
  put it last): key encoding version 2, and version-1 databases are
  upgraded on their first write.
- **2026-10-17 — §M14.19 `uuid` everywhere (T3):** `ncp` version 2 (tag
  11: registered types by name and binary form; version 1 still served,
  with them as text); all six drivers on version 2 with native uuids
  (Rust `Value::Uuid`, C `nc_uuid`, C++ `Uuid`, Python `uuid.UUID`, JS
  string, Java `UUID`) and a generic form for other registered types;
  pg-compat's `uuid` (oid 2950, text and binary).
- **2026-10-17 — §M14.19 `uuid` (T3, first part):** the type and
  `gen_random_uuid` / `uuidv4`, `uuidv7`, `uuid_extract_version`,
  `uuid_extract_timestamp`; `Rng::fill_unpredictable`,
  `Kv::random_bytes`; `20_uuid.slt`, checked against PostgreSQL 18.
- **2026-10-17 — §M14.19 types as modules (T1, T2):** `ext::Type` with
  its abilities as separate interfaces; the built-in types registered
  through it; registered types (`Value::Ext`, `DataType::Ext`) stored in
  rows, keys and the catalog, compared, grouped and listed
  (`nc_catalog.types`), and refused where an ability is missing; named
  types in the grammar; PostgreSQL's type names and `::regclass` moved
  out of the parser into pg-compat.
- **2026-10-17 — §M14.19 modules built to leave the repository:** the
  layout `crates/` (core), `modules/`, `editions/`; PostgreSQL
  compatibility is `modules/pg-compat` (from `nucleus-sql`'s `pgcatalog`
  and `nucleus-server`'s `pg`), on `nucleus_sql::ext` alone; the test kit
  `nucleus-testkit` (runner and scripts); the CLI is `editions/nucleus`;
  `scripts/check-boundaries.py` and `scripts/detach-test.sh`, and the CI
  job `modules`.
- **2026-10-17 — §M14.19 front ends and authentication registered:**
  `nucleus_server::{FrontEnd, FrontEnds, Authenticator}`; the CLI starts
  the front ends it is given (`--front-end`).
- **2026-10-17 — §M14.19 aggregates and table functions registered:**
  `ext::Aggregate` / `Accumulator`, `ext::TableFunction`; the built-ins
  (`count` … `bool_or`, `generate_series`, `unnest`) register through
  them; `tests/extension.rs`.
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
