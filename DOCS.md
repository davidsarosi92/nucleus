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
| 5 | Change log |

---

## 1. Repository layout

| Path | Crate | Contents | Milestone |
|------|-------|----------|-----------|
| `crates/platform` | `nucleus-platform` | file system, clock and randomness traits; real and simulated implementations; simulation harness | §M0 |
| `crates/storage` | `nucleus-storage` | page format and pager; buffer pool, B+tree, WAL to come | §M1 ◐, §M2–§M4 |
| `crates/txn` | `nucleus-txn` | MVCC and transactions — empty | §M5 |
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
  `SimFault::Crash`.  After a crash every old handle fails with
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

### 4.1 Page format (`page`)

A database file is an array of 8 KiB pages (`PAGE_SIZE`); page `n` is at
offset `n × 8192`.  Every page, the header pages included, starts with a
24-byte page header (little-endian):

| Offset | Size | Field |
|-------:|-----:|-------|
| 0  | 4 | CRC-32C of bytes `4..8192` |
| 4  | 1 | page type |
| 5  | 3 | reserved, zero |
| 8  | 8 | LSN (unused until §M4) |
| 16 | 8 | the page's own id |

The payload is the remaining 8168 bytes (`PAYLOAD_SIZE`).  Page types below
16 (`PageType::FIRST_USER`) belong to the pager: 0 unused, 1 header, 2 free.
Verification reports *why* a page failed: `checksum mismatch`, `never
written (all zeros)`, `page id mismatch (misdirected write)`.

`crc32c` is a portable table-driven CRC-32C (Castagnoli), checked against
the RFC 3720 vectors; a hardware version can replace it without changing any
stored value.

### 4.2 Pager (`pager::Pager`)

**API.**  `Pager::create(fs, path)`, `Pager::open(fs, path)`;
`read_page(id) -> Page`, `write_page(id, &mut Page)` (stamps id and
checksum; caller types only), `allocate() -> PageId`, `free(id)`,
`root(slot)`/`set_root(slot, page)` (16 slots), `sync()`, `close()`,
`check()` (walks and validates the free list), and `page_count`,
`free_count`, `reserved`, `durable_header`.  Shareable between threads: page
reads and writes do not take the pager lock.

**Errors** (`nucleus_storage::Error`): `Io`, `NotADatabase` (magic, format
version or page size), `Corrupt { page, reason }`, `InvalidPage` (outside
the file, a header page, or a pager page type), `Poisoned`.

**Header.**  Pages 0 and 1 hold two copies; copy `seq % 2` holds sequence
number `seq`.  An update writes the older copy with `seq + 1`, so a torn
header write leaves the other copy intact; `open` picks the valid copy with
the highest `seq`.  Payload: magic `NUCLEUS\0`, format version (1), page
size, `seq`, page count, free list head, free list length, 16 root slots.

**`sync` order.**  (1) Refill the reserve from the free list; (2) extend the
file to the page count the header will claim (allocated pages nobody wrote
are zeros); (3) `fsync` the data; (4) write the header copy; (5) `fsync`
again.  If any step fails the pager is **poisoned**: every later call fails
with `Poisoned`, and the file must be reopened.

**Free list and the reserve.**  Free pages are linked through their first
payload bytes.  Allocation order: pages freed since the last sync (not on
the durable list), then the reserve (taken off the list before the last
commit), then a new page at the end.  Never a page the durable header lists
as free — allocating one and crashing would leave the durable free list
running through user data.  A crash forgets the reserve: up to
`RESERVE_TARGET` (32) pages leak, nothing is corrupted.  `close` returns the
reserve to the list.  `open` refills the reserve (and so commits once) when
the free list is non-empty.

**`create` is atomic**: the file is built as `<path>.creating`, synced,
renamed into place, and the directory synced.  After a crash, `path` either
does not exist or is a complete, empty database.

**What the pager does not guarantee**: atomicity of callers' page writes.  A
page rewritten or freed after a sync keeps its new contents even when a
crash rolls the header back.  That is §M4's WAL.

### 4.3 Tests

`crates/storage/tests/pager.rs`: round trip (simulated and real file
system), flipped bit, misdirected write, unwritten pages, invalid and
reserved pages, foreign files, one and two damaged header copies, free list
reuse order, `close` returning the reserve, poisoning after a failed sync,
torn page writes detected (both "old page intact" and "torn, caught" occur
across seeds).

**The crash sweep** (`crash_at_every_io_point`): a seeded workload — six
epochs of allocations, writes, rewrites and frees, one of them freeing more
pages than the reserve holds — is run once to count its I/O operations, then
once per operation with a crash scheduled there, in both crash modes, for 4
seeds.  After each crash the file must reopen, pass `check`, be in the state
of the last completed sync or the one in flight (identified by root slot 0),
have exactly that page count, have lost no free pages beyond one reserve,
and hold the right contents in every live page not touched after that sync.

Mutations checked against the tests, each caught: allocating from the
durable free list (sweep: "on the free list but not a free page" — caught
only once a bulk-free epoch was added; before that the case never arose),
writing the header before syncing the data (sweep), creating the file in
place (sweep), not extending the file before the header (unit test).

## 5. Change log

- **2026-10-17 — §M1 pager** (awaiting CI): page format, CRC-32C, A/B
  header, crash-safe free list with reserve, atomic create, poisoning; crash
  sweep; `[profile.test] opt-level = 1` (crash sweep 36 s → 5 s).
- **2026-10-17 — §M0 shipped** (CI green: lint + all eight build targets).  Workspace, platform layer (real POSIX +
  Windows, simulation with faults and crashes), seed harness, conformance and
  simulation tests, CI for every build target.
