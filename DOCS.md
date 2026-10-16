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
| 6 | Change log |

---

## 1. Repository layout

| Path | Crate | Contents | Milestone |
|------|-------|----------|-----------|
| `crates/platform` | `nucleus-platform` | file system, clock and randomness traits; real and simulated implementations; simulation harness | §M0 |
| `crates/storage` | `nucleus-storage` | page format, pager, buffer pool, B+tree, WAL, store | §M1–§M4 ✅ |
| `crates/txn` | `nucleus-txn` | MVCC, snapshot isolation, GC | §M5 ✅ |
| `crates/kv` | `nucleus-kv` | the KV contract — empty | §M6 |
| `crates/cli` | `nucleus-cli` (binary `nucleus`) | `--version` only | §M6 |

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
Write skew is possible (tested and documented).

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

## 6. Change log

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
