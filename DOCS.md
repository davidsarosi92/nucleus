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
| `crates/kernels` | `nucleus-kernels` | the vectorized executor's batch kernels, compiled per CPU level and chosen at run time; a neural network's forward pass (f32 matrix product, GELU, softmax, layer normalization) | §M14.20, §M14.22 |
| `crates/sql` | `nucleus-sql` | types, encodings, catalog (`nc_catalog`), parser, binder, optimizer, executor, sessions; the extension API (`ext`) | §M7–§M14.5, §M14.19 |
| `crates/wire` | `nucleus-wire` | the nucleus protocol's wire format, for both ends: versions, message kinds, value tags, frames, values, the columnar layout (§10) | §M14.20 W8 |
| `crates/server` | `nucleus-server` | front ends (the nucleus protocol), authentication | §M12, §M14.7, §M14.19 |
| `crates/client` | `nucleus-client` | the Rust driver | §M14.6–§M14.8 ✅ |
| `crates/testkit` | `nucleus-testkit` | the sqllogictest runner and the SQL layer's scripts, for the core, front ends and modules | §M14.19 |
| `modules/pg-compat` | `nucleus-pg-compat` | PostgreSQL compatibility: the protocol, `pg_catalog`, `information_schema`, the `pg_*` functions (its own README) | §M12, §M14.5, §M14.19 |
| `modules/tls` | `nucleus-tls` | TLS over rustls, for the front ends and the Rust client | §M14.25 ✅ |
| `modules/oidc` | `nucleus-oidc` | OpenID Connect: logins by identity tokens | §M14.25 ✅ |
| `modules/ldap` | `nucleus-ldap` | LDAP: passwords checked by a directory | §M14.25 ✅ |
| `modules/http` | `nucleus-http` | just enough HTTP/1.1 over TLS for the modules that call out (oidc, embeddings) | §M14.22 |
| `modules/embeddings` | `nucleus-embeddings` | embedding model providers: OpenAI-compatible endpoints, Cohere, Voyage (§8.23) | §M14.22 |
| `modules/local-models` | `nucleus-local-models` | the `local` model provider: models run inside nucleus — WordPiece and Unigram tokenizers, BERT and XLM-RoBERTa encoders on `ext::kernels`, files from Hugging Face or a directory kept in the database (§8.23) | §M14.22 |
| `modules/mcp` | `nucleus-mcp` | a Model Context Protocol server: AI models list, describe and query a database (§6.5) | §M14.10 |
| `editions/nucleus` | `nucleus-cli` (binaries `nucleus`, `ncsql`) | the full edition: `nucleus kv`, `nucleus sql` / `ncsql`, `nucleus server` with every front end, `nucleus mcp` | §M6, §M12, §M14.6 ✅ |
| `editions/embedded` | `nucleus-embedded` (a `cdylib`, `staticlib` and `rlib`) | the embedded edition: a database in memory or in a directory inside the application, its sessions over the nucleus protocol through a pipe in the process, behind a C API (`include/nucleus_embedded.h`) that Python and Node load (§11, *In the process*); for `wasm32-wasip1` the browser's, its sessions stepped (§11, *In the browser*) | §M14.23 |
| `integrations/d-os` | `nucleus-dos-settings` (binary `nucleus-settings`) | the d-os settings service and app manifest | §M6, §M6.5 ◐ |
| `drivers` | — | drivers for C, C++, Python, JS/TS, Java (§11); Python's platform wheel (`build_wheel.py`) and JavaScript's Node-API addon (`native/`) for the embedded edition; JavaScript's browser entry (`src/browser.ts`, the engine in WebAssembly, `wasm/build.sh`) | §M14.8 ✅, §M14.23 |
| `examples` | `nucleus-example` (Rust) | the quickstart in every driver's language | §M14.8 ✅ |
| `examples/extension` | `nucleus-example-extension` (binary `geo-demo`) | the sample module `geo`: a type, a function, a catalog view; the guide for writing a module (its README) | §M14.19 ✅ |
| `scripts` | — | `package.sh`, `psql-smoke.sh`, `driver-tests.sh`; `gen-word-breaks.py`, `gen-stop-words.py` (the text analyzers' tables, §8.19) | §M6, §M12, §M14.8, §M14.21 |
| `docs/llm.md` | — | the dialect for AI models: what there is, what is missing and what to use instead, the catalog, hints, the analyzer | §M14.10 ✅ |
| `docs/benchmarks.md`, `bench/` | — | the benchmark suite against PostgreSQL (`bench/run.py`, run in CI; `tpch.py`, `clickbench.py`; `phases.py`, one build against another step by step; `bench/ann/`, the HNSW storage models on the KV with hnswlib and pgvector as references, and `sql.py`, the index through SQL; `bench/text/`, the full-text data), its latest results and targets | §M14.11, §M14.12 V4, §M14.21 |

`crates/` is the core and the public API crates; `modules/` are modules,
each a folder built to leave the repository (its own README, changelog,
licence and version; dependencies on the public API crates only, by
version and path; in code, of `nucleus-sql` only `nucleus_sql::ext`);
`editions/` are the builds that choose modules.  `scripts/check-boundaries.py`
checks this both ways and `scripts/detach-test.sh` builds and tests every
module from its packaged form alone (§2).

Workspace-wide: Rust edition 2024, toolchain pinned in `rust-toolchain.toml`
(rustup installs it on first use), `unsafe_code` forbidden, `missing_docs`
warned, clippy warnings are errors in CI.  One exception to the first:
`nucleus-kernels` denies unsafe code instead and allows it in its
`kernel!` macro alone, to call a kernel's copy compiled for a CPU feature
once the CPU was found to have it (§8.5).

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

**Fuzzing** (PLAN.md §M14.29 S2; `fuzz/README.md`): `fuzz/` is a crate of
its own (its own workspace, built only by `cargo fuzz` on a nightly
toolchain with AddressSanitizer) with a target for every input that
crosses a trust boundary — `sql_parse`, `values` (every type's text
input), `text_search`, `copy`, `ncp`, `pg_wire`, `scram`, `jwt`, `disk`
(a real database's pages and WAL patched, checksums sealed again or not,
then opened, checked and read) and `model_files`.  An error is an answer;
a panic, a hang, a memory error or an allocation past the limit is a
finding.  The front ends serve any byte stream
(`nucleus_server::handle_native_stream`, `nucleus_pg_compat::handle_stream`:
no TLS, no client address), so the protocol targets need no socket;
their database is in memory under the machine's clock with
`statement_timeout` at 250 ms.  Seeds are kept in `fuzz/seeds/`
(`fuzz/seeds.py` makes them: the sqllogictest statements, recorded
driver and psycopg sessions, a computed SCRAM exchange, tiny model
files); CI's `fuzz` job runs each target three minutes a change, its
corpus kept between runs:

```sh
cd fuzz && cargo +nightly fuzz run -O ncp corpus/ncp seeds/ncp -- -max_total_time=300 -rss_limit_mb=2048 -timeout=10
```

## 3. Platform layer (`nucleus-platform`)

The only way any nucleus code reaches files, time or randomness (PLAN.md
principle 4).

### 3.1 Interface

- `Platform` — `fs()`, `clock()`, `rng()`, `parallelism()` (the threads
  work can usefully run on: the CPUs on the real platform, asked of the
  OS once, when it is made; 1 in the simulation).
- `FileSystem` — `open(path, OpenMode)`, `remove_file`, `rename` (atomic
  replace), `create_dir_all`, `list_dir` (sorted names), `exists`,
  `sync_dir`.
- `File` — positional only, no cursor, all `&self` (shareable between
  threads): `read_at`, `write_at`, `read_exact_at`, `write_all_at`, `sync`,
  `sync_order` (the writes so far ordered before any later one, whatever
  the sync policy — a barrier on macOS, `sync` elsewhere and by default:
  for a sync whose writes a later full sync makes durable anyway),
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

`std`, but for one call.  `sync` follows the platform's `SyncPolicy`
(PLAN.md D17; `RealPlatform::with_sync`, the KV's `Options::sync`, the
server's `--sync`): `Full` is `File::sync_data`, which std implements as
`fdatasync` on Linux/d-os (a length change made durable, the file's
times not — PostgreSQL's and SQLite's choice there; PLAN.md §M14.28 L0),
`fcntl(F_FULLFSYNC)` on macOS (verified in std's `sys/fs/unix.rs`) and
`FlushFileBuffers` on Windows; `Ordered`, the
default, is the same except on macOS, where it is
`fcntl(F_BARRIERFSYNC)` — the writes before it reach the drive before any
after it, without flushing the drive's cache, so files are never left
inconsistent and a crash of the process or the OS loses nothing synced,
while a power loss may lose the last syncs (as PostgreSQL's and SQLite's
macOS defaults).  A file system without barriers gets the full sync.
That `fcntl` and macOS's `sysctlbyname("hw.memsize")` are the crate's
only unsafe code (std has neither call; `barrier_sync`,
`real::physical_memory` — the memory the process may use: the machine's,
or a container's cgroup limit below it, read from `/proc` and
`/sys/fs/cgroup` on Linux; `None` elsewhere).  The simulation's sync is the full one: a power loss
under `Ordered` leaves the files as a crash before the last syncs would,
which the simulation tests.  Positional I/O is
`pread`/`pwrite`, on Windows `seek_read`/`seek_write` (std maps
`ERROR_HANDLE_EOF` to `Ok(0)`).  `sync_dir` opens and `fsync`s the directory
on POSIX (the full `fsync`: what `fdatasync` makes of a directory is not
specified; on macOS by the policy) and only checks that it exists on
Windows (directories cannot be opened as files there; NTFS journals
metadata).  `FileSystem::writes_in_place` is true but on Apple's systems:
APFS writes a changed block elsewhere (copy-on-write), so a synced
overwrite costs it more than an append (measured: 16 KiB written a small
commit against 10–11); the simulation answers by its seed (or
`SimFs::set_writes_in_place`), so the seeded tests run both ways.  The
RNG seed comes from `getrandom`.

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
  dir), the simulation and the file system in memory: I/O semantics, open modes, rename, directories,
  clock, RNG.  Runs on every CI target.
- `tests/sim.rs` — durability, directory sync, rename atomicity, fsyncgate,
  injected faults, scheduled crashes, determinism, torn sectors, and a
  proptest property: after a torn crash every byte is either its last-synced
  value or one a later write put there.  Checked against mutations of the
  crash logic (keeping unsynced writes whole, or in part): each fails a test.

### 3.6 In memory (`memory::MemoryPlatform`)

A database in memory as a product (§M14.23: `:memory:` in the embedded
edition): `MemoryFs` keeps each file as one byte vector, shared by the
file's open handles (a removed or renamed file stays readable through a
handle opened before, as on POSIX) — no durable second image and no
faults, as the simulation has, so a database in memory costs what its
files hold (`MemoryFs::bytes`).  `sync` and `sync_dir` do nothing (the
files go with the process); locks are exclusive per path while held;
`writes_in_place` is true.  Time and randomness come from another
platform — `MemoryPlatform::new()` the machine's (`RealPlatform`),
`MemoryPlatform::with(p)` any.  `tests/conformance.rs` runs its
assertions against it too.

### 3.7 A pool of slots (`slots::SlotPlatform`)

The browser edition's files (§M14.23): a browser opens an OPFS file for
synchronous access only in a worker and only by an asynchronous call,
which the synchronous engine cannot wait for mid-statement — so the host
opens a pool of files ahead (the `Slots` trait: `count`, `read`, `write`,
`size`, `truncate`, `flush`, by slot number; slots may be added between
calls) and `SlotFs` keeps the database's files in them, SQLite's
`opfs-sahpool` idea.  Each slot is one file: a 4 KiB header, then its
bytes.  The header is two 2 KiB copies, each `NCSLOT01`, a generation,
the path's length and bytes, and an FNV-1a checksum; a write goes to the
copy not holding the latest, with a generation above every other in the
pool, and is flushed at once.  So a torn header write leaves the other
copy (the slot as it was), and a rename — the moved file's header
rewritten with the new path, then the replaced file's slot freed — that
a crash cuts between leaves two slots naming the path, of which the
newer wins when the pool is next read (the stale one freed).  Opening,
creating, renaming and removing are header writes on handles already
open; directories are not stored (one exists where it was made in this
run or where a file lies under it); `sync_dir` does nothing (headers are
flushed as written).  A removed or replaced file stays readable through
handles opened before: its slot is reused only when they are dropped.
With every slot taken a new file fails with `StorageFull`;
`free_slots()` tells the host when to add more (it keeps eight free,
adding sixteen).  Locks are per path within the pool, which one page
holds at a time (OPFS's handles are exclusive).  `MemorySlots` is a pool
in memory for tests; `tests/conformance.rs` runs against a pool of
sixteen, and `nucleus-kv`'s contract test opens a database on one, then
again on the same slots.

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
CRC-32C by the CPU's instructions where it has them (ARMv8 CRC, SSE 4.2;
`nucleus_kernels::crc32c`; three streams over neighbouring blocks at
once, joined by shifting with tables — Mark Adler's `crc32c.c`, Intel's
iSCSI paper —, an 8 KiB page in 353 ns rather than one stream's 934 on
Apple M-series, §M14.20 W5), else portable and table-driven — the same
value, checked against the RFC 3720 vectors and against each other.

### 4.2 Pager (`pager::Pager`)

**The rule: the durable state is immutable.**  `write_page` accepts only
pages allocated since the last `sync` (else `Error::Immutable`).  Changing a
committed page means copying it; a freed committed page goes to *pending*
and becomes free after the next commit.  A page allocated and freed within
one epoch is free at once.  So a crash at any moment leaves the last
committed state untouched, and `sync` moves to a new one with one header
write.

**API.**  `create`, `open`; `read_page`, `read_pages` (a run of
neighbouring pages in one read, each verified; reads go through a buffer
each thread keeps, so a page is copied once into its own memory and
nothing is zeroed first), `write_page`, `is_writable`,
`allocate` (lowest free page, else grows the file — on disk ahead of
the pages, by zeros an eighth of its length at a time, 1 to 64 MiB: a
write into blocks the file has costs a fraction of one that makes APFS
allocate them; zeros past the header's page count are not the
database's), `free` (refuses double
frees); `meta`/`set_meta` (16 `u64` slots committed atomically with the
pages); `sync`, `close`; `check` (on-disk free list vs. memory, no page in
two places), `accounting` (free / pending / free-list pages / allocated).
A read checks its page id against the page count and the poisoned flag
mirrored in atomics, not under the pager's lock (§M14.20 W5: a parallel
scan's workers queued on it — 14 % of TPC-H Q1's time on row tables).

**Header** — pages 0 and 1; copy `seq % 2` holds sequence number `seq`; a
commit writes the older copy, `open` takes the valid copy with the highest
`seq`.  Payload: magic `NUCLEUS\0`, format version (2; version 1 files —
before compressed values and key prefixes — are read, and say 2 from their
next commit on, so an older build refuses them rather than misreads them),
page size, `seq`, page count, first free-list page, free count, 16 meta
slots.

**Free list** — a chain of free-list pages (`next u64, count u32`, then up
to 1019 page numbers each), rewritten by every commit into pages the
previous state does not use; the previous free-list pages are free in the
new state.  Nothing leaks: after a crash every page is live, free, or
beyond the page count.

**`sync` order:** write the new free list; extend the file to the page
count (unwritten allocated pages become zeros); a sync ordering the
pages before the header (`sync_order`); write the header copy; `fsync`.
Any failure poisons the pager.

**A commit in two halves** (§M14.20 W4): `sync` is `freeze(meta)` then
`commit_frozen`.  `freeze`, under the pager's lock, takes the state as it
is — the pages allocated so far, the meta slots — as the next to commit,
computes its free list (its pages taken out of the free set, or past the
end) and returns at once; its pages are *frozen*: no longer writable (a
writer copies them, as it copies durable pages), still written back
(`write_page` accepts them; `is_frozen`).  Writing goes on meanwhile in
pages the frozen state does not use — allocated from its free pages or
past its end; a page freed after the freeze is free only after the next
commit.  `commit_frozen` writes the free list and the header without the
lock.  A write past the pages the file is known to hold takes the lock,
as the commit's extension of the file does, so the two never race (by
construction; a single-threaded simulation cannot interleave them).  One
commit in flight at a time.  `create` builds the file as
`<path>.creating`, syncs it, renames it into place and syncs the directory.

### 4.3 Buffer pool (`buffer::BufferPool`)

`fetch(id) -> PageRef` pins a page (dropping the `PageRef` unpins);
`PageRef::read()` / `write()` take the frame's reader/writer latch, and
`write()` marks it dirty — after checking the pager's rule, so a missing
copy-on-write fails at once.  `create(type)` allocates a page into a frame;
`free(id)` drops it (must be unpinned; a dirty page of a commit in flight
is written back first — the frozen state still holds it).
`flush_frozen` writes back the dirty pages of the commit in flight while
writers go on — and readers: a page at a time under its shard's read
lock (PostgreSQL's checkpointer writes a buffer under a share lock the
same way), so hits go on and a load or an eviction waits for one page's
write, not the shard's (holding the shard's write lock through it, a
checkpoint after a large build held searches up for seconds); one flush
at a time.  Eviction is the clock algorithm
over unpinned frames; dirty victims are written back first.  `flush`
writes every dirty frame, `sync` = flush + pager commit.  The frames are
split into up to 16 shards (at least 256 frames each; a page always in the
shard of its id), each with its own page table, free frames, clock hand and
reader/writer lock, so a parallel scan's workers do not queue on one lock.
A hit only reads its shard's page table (threads whose lookups all descend
from one root share it), a change writes it; a frame's pin count and
reference bit are atomics, the bit written only when it changes, and the
page table hashes a page id with one multiplication.  Each shard (with
its hit counter) and each frame takes a 128-byte cache line of its own,
so threads pinning neighbouring frames or finding pages in neighbouring
shards do not write one line.  A miss reads
the page without any (and reads again if a page was written back
meanwhile), so threads reading cold pages do not wait on each other.
`read_ahead(ids)` reads a run of consecutive page ids (up to 16, 128 KiB —
PostgreSQL's `io_combine_limit`) with one read, into frames not yet
referenced, so the clock takes them first if unused; pages cached
meanwhile are kept, and a write-back meanwhile drops the rest.  A B+tree
cursor moving to a leaf that is not cached reads the parent's next leaves
with it, the window growing with the leaves it has moved through since
its last seek (1, 2, 4 … 16, as Linux's read-ahead ramps up), so an index
lookup's entries across a leaf's end read one page and a scan soon reads
16 at a time (§M14.20 W5: with the pager's lock gone, `pread` 23 % →
10 % of TPC-H Q1's time on row tables).  Errors: `PoolExhausted` (every
frame pinned), `Pinned`.  `stats()`: hits, misses, evictions, write-backs.

### 4.4 B+tree (`btree::BTree`, `node`)

Byte-string keys (≤ `MAX_KEY` = 1024) compared bytewise, values of any
size.  **Node format:** kind, count, leftmost child, an offset table, cells
packed from the end.  Leaf cell `klen u16, flags u8, vlen u32, key, value`;
a cell over 2048 bytes moves its value to an overflow chain (`next u64,
len u32, data`) and stores `first page, length`.  Internal cell `klen u16,
child u64, key`; child `i` holds keys in `[keys[i-1], keys[i])`.

**Compression** (§M14.20 V5).  *Key prefixes:* a leaf (kind 3) stores the
prefix its first and last keys share — so all its keys — once, its cells
only the rest; sizes, splits and packing count the prefix once, so a leaf
holds more rows (a table's keys share their table and index ids and the
high bytes of their primary keys: a million-row table's file 93 → 61 MB).
Readers compare a key as prefix and rest (`node::KeyRef`).  *Values:* a
value of 1 KiB or more is stored compressed when that saves an eighth
(shorter ones save little and would be decompressed at every read — a
TPC-H scan spent a third of its time so; values stored compressed before
read as they were)
— an LZ block (LZ4's block format, `lz.rs`, no dependency), flag 2 in the
cell (`length u32, block`) or flag 3 in an overflow chain (`first page,
block length, length`); reads decompress, checking every length and
offset, so a damaged block is an error.  The decoder copies as LZ4's
does, in fixed 16-byte pieces that may run past a sequence's end into
room left after the output (a match overlapping itself a byte at a time
for 16 bytes, then 8 at a time a multiple of its period back), so a
short sequence is a few loads and stores, not a call to `memmove`:
2–2.5× the plain decoder on `hits`' text (1.2 → 2.4 GB/s in 1 KiB
blocks, 1.7 → 4.1 in 64 KiB), tested against it on blocks whole, cut
and damaged.  Large text and documents shrink;
rows of a few short columns do not (too little repeats within one).

**Checksums:** CRC-32C — the CPU's instructions where there are (chosen at
run time), else slicing-by-8 (eight bytes a step, tables built at compile
time); the same values as the bytewise definition, tested.

**Copy-on-write:** a node is rewritten in place only if its page is
writable; otherwise copied to a new page (old one freed) and the parent
updated, up to a new root.  **Balance:** split by bytes; a non-root node
under `MIN_FILL` (¼ page) after an insert or delete is merged with a
sibling, or the pair is split evenly; an internal root with one child is
replaced by it.

**API:** `create`, `open(root)`, `root`, `get`, `insert`, `remove`,
`set_workers(n)`, `apply_sorted(ops)` / `insert_sorted(entries)` (sets and removals sorted
by key, without repeats: each node read and written once for all the keys
reaching it; full leaves and full internal nodes packed to about 7/8, a
small last piece evened out with the one before — a sorted build's
appends leave its pages full, as PostgreSQL's `nbtsort` does; neighbours a batch of
removals left under `MIN_FILL` merged until they are not, seams of merged
internal nodes included), `range` / `range_rev` (bounds; going forward a
range checks its end on a key before reading the key's value, so the first
entry past the end — perhaps long, compressed, in overflow pages — is not
read), `cursor`
(`seek`, `seek_after`, `seek_near`, `seek_end`, `next_entry`, `prev_entry`), `destroy`,
`check` (ordering, separators, equal leaf depth, minimum fill, overflow
lengths, no page used twice; returns the pages used).  `&self` reads,
`&mut self` writes: callers wrap the tree in an `RwLock` (D11).  **Reads
in place:** `get` and cursors read pages through `node::View` (binary
search over the offset table, a key, a child or an entry checked as
`decode` checks it) and copy out only the entry returned; a cursor keeps
its leaf pinned and its path as page ids, and `seek_near` stays in its
leaf when the key falls inside it (seeks to nearby keys in order — a
commit's checks — mostly without a descent).  Writes decode nodes into
structs, but a sorted batch's leaves (`apply_sorted`, so every commit):
a leaf's cells and the new entries are merged by reference and encoded
straight into the page (`node::encode_leaf`; the pieces cut as before,
checked against the decoded packing), so a commit that touches every leaf
allocates nothing per entry.  A batch reaching many leaves (64 or more)
prepares them on workers (`set_workers`; the store gives the platform's
CPUs, the simulation one): the leaves it reaches found first, then the
workers claim them in order (a counter) and merge and encode each into
a page of its own, at most 128 per worker ahead of the walk, while the
walk installs each as it reaches it — the page swapped into the pool's
frame, not copied — and rebuilds the internal nodes; pages freed and
taken in the order one thread takes them, so the tree is the same page
for page (tested so, with a filter and without, and through a pool of
48 frames that evicts all the time).  A worker reads only its leaf and
the leaf's overflow pages, which the walk touches only once it installs
that leaf; the two sides meet without a shared lock per leaf (each
result in its own slot; a condition variable only for a worker too far
ahead, or the walk finding its leaf not ready).  **Compaction filter:**
`apply_sorted_filtered(ops, &dyn LeafFilter)` lets the layer above drop
entries of the leaves the batch rewrites anyway — `drops(entries, drop)`
sees a leaf's keys and short values after the batch went in, one leaf at
a time on whichever thread prepares it, so it must be a pure function of
them (the MVCC engine drops the versions no snapshot sees, §5); a dropped
entry's overflow pages are freed with the rest.

### 4.5 Write-ahead log (`wal::Wal`)

Segments named by start LSN (20 digits, `.wal`); an LSN is a byte position
in the log.  Record: `len u32, crc u32 (CRC-32C of LSN ‖ payload), lsn u64,
payload`.  `append` → LSN past the record; `sync_to(lsn)` is group commit
(one `fsync` for everything appended; waiters for earlier LSNs return at
once); `replay(from, f)`; `truncate_before(lsn)`; `size`, `start`, `end`,
`durable`.

**Preallocated and recycled segments** (where `writes_in_place`; PLAN.md
§M14.28 L0).  A segment is grown ahead of its records by zeros, a
mebibyte at a time up to the segment size, so a commit's sync writes its
block and no length; the segments a checkpoint drops are renamed to
spares (`<20 digits>.spare`, kept up to as many as that checkpoint
dropped, the rest deleted) and renamed into the next segments
(PostgreSQL's recycled segments).  A spare's old records carry older
LSNs, which the checksum folds in, so none passes for a record at its new
place.  Elsewhere (APFS) segments grow as written and dropped ones are
deleted, as before; spares found there are deleted.

**Crash rules.**  `open` cuts a torn tail of the last segment — in a
preallocated one, writes zeros over the rest of it if what follows the
records is not zeros or a record older than the segment (a torn record of
its own, whose bytes a later record must not leave behind); a segment
before the last must end where the next begins, or it is corruption.  Before a new segment is created, the previous
one is synced — an ordering sync (`sync_order`): the sync that makes a
record of the new segment durable flushes it too, and a barrier orders it
before the new segment's directory entry, so a non-last segment is never
torn — and so is its directory entry.  (Kept, not dropped as §M14.20 W4
first planned: it is what lets `open` tell a torn tail from corruption;
PostgreSQL also syncs a finished segment.)  Segments wholly before a
checkpoint are deleted together with one directory sync (W4; they were
one sync each): a crash may keep any of them, and `open_from(…, from)` —
the store opens from its checkpoint's LSN — deletes again those before
the segment holding it; a log left with no segment starts at `from` (a
segment created there whose entry a crash lost held nothing durable).
Before dropping any, `truncate_before` syncs the directory: the segment
holding the checkpoint may be new, its entry not yet durable, and a crash
that kept an older segment but lost it would leave a log ending before
the checkpoint (found by the simulation with recycling, 2026-11-06; the
deletions before had the same window).  A failed `fsync` poisons the log.

### 4.6 Store (`store::Store`)

The B+tree made durable by the WAL.  `Store::open(platform, dir, opts)`
takes an exclusive lock on `dir/LOCK` (`FileSystem::lock`: `flock` /
`LockFileEx` on the OS, a lock table in the simulation that a crash
clears) and holds it as long as the store — a second process (or store)
fails with `Error::Locked` ("in use by another process") instead of
corrupting the database — then creates `dir/data` and `dir/wal/` if
needed and recovers; returns `Recovery { meta, replayed }`.  `write(&Batch)` appends the batch to the
WAL and applies it under the tree's write lock (visible at once);
`wait_durable(lsn)`; `commit` = both.  `write_filtered(&Batch, filter)`
applies it through a `LeafFilter` (§4.4) — not logged: replay applies the
batch without it, so what a filter drops must be garbage its layer can
find again after a crash.  A batch whose keys are all
distinct (every batch the engine writes) is applied in one sorted pass
(`apply_sorted`); otherwise op by op, in order.  `write_unlogged(&Batch)`
applies a batch without logging it (D16): durable only once a checkpoint
after it is, lost by a crash before — the transaction layer marks what it
writes so first, in a logged record, and checkpoints before the commit
that needs it (§5.2).  `read()` gives the tree's read
guard; `get`.  `checkpoint()` is `checkpoint_begin()` then `finish()`
(§M14.20 W4): `checkpoint_begin`, under the writer lock, freezes the tree
as it is — root, the LSN applied, the last batch's `meta` —
(`Pager::freeze`) and returns; `finish`, writes going on, syncs the WAL
up to that LSN, writes back the frozen pages (`flush_frozen`), commits
the pager (`commit_frozen`) and truncates the WAL.  Writers wait only for
the freeze; one checkpoint at a time; `checkpoint_past(bytes)` runs one
only if the WAL since the last (after any running ends) is past
`bytes`.  `wal_since_checkpoint()` tells callers when to
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
  piling up across segments and checkpoints; the same with batches written
  between a checkpoint's halves (W4), and writers with a checkpointer
  thread on real files.

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

**Intents.**  A large transaction's spilled writes are versions too —
*write intents* — stamped not with a commit timestamp but with the
transaction's id from the top of the range (`intent_ts(id) = u64::MAX -
id`, at or above `INTENT_FLOOR` = 2⁶³), so a key's intents come before its
versions.

**Reserved keys.**  No encoded key starts with `00` and a byte other than
`00` or `FF`, so store keys from `00 01` to `00 FF` are the engine's own:
*status records* `00 02 ‖ id → commit ts` (a large transaction that
committed), the next transaction id `00 03 → id` (written before a
transaction's first intents, so no later one is given an id an intent
has), the *marks* of fresh ranges a running transaction writes without
the WAL `00 04 ‖ start → end` (§5.2), and an older engine's *side area*
`00 01 ‖ id (u64 BE) ‖ user key → tag ‖ value`, moved into versions when
such a database opens.  Every walk over versions skips them.

### 5.2 Engine and transactions

`Engine::open(platform, dir, EngineOptions)` (cheap to clone).
`begin()` / `begin_with(TxnOptions { isolation, read_only, at })`.
`Transaction`: `get`, `put`, `delete`, `scan(start, end)` (ascending;
merges the transaction's own writes), `commit() -> Timestamp`,
`rollback()`; dropping it rolls back.  `Timestamp` is opaque outside the
crate (principle 3): compared, printed and parsed (`Display`, `FromStr`),
never computed with.  `Engine`: `visible`, `checkpoint`,
`vacuum`, `intents_pending`, `version_count`, `intent_count`,
`side_count`, `store`.

**Snapshot isolation.**  A transaction reads at its start timestamp — the
engine's *visible* timestamp, which advances only once a commit is durable,
so nobody reads what a crash could undo.  Writes are buffered — the
newest of each key, in key order: a sorted vector while they come in
order (a statement writes the rows a scan found; a write past the last
key is a push, the question whether a key was written already one
comparison), a write a little out of order moved into place, and a
B-tree map from the first far out of order on (`writes.rs`).  Commit,
under the commit lock: for each written key, if its newest version (durable
or not) is newer than the snapshot → `Error::Conflict` (retryable; first
committer wins); else one WAL batch with the next timestamp, applied at
once (its ops built in key order, as the write set is: not sorted
again); then the group-commit wait, then *visible* moves.  `put_new` (an
`INSERT`'s write: set the key unless it has a value) looks at the key's
versions — and, finding no entry of the key at all, notes the run of
keys up to the tree's next entry: the next `put_new` inside it needs no
look (whatever committed before the snapshot was in the tree then, and a
later commit of such a key conflicts at commit as it would have at the
look).  A bulk insert of new keys in key order looks at the tree once,
not once a row; the run ends at a spill, where its own intents enter the
tree.  A checkpoint is due
when the WAL since the last one exceeds `checkpoint_wal_bytes` (1 GiB, as
PostgreSQL's `max_wal_size`; 64 MiB on 32-bit targets) — asked in one
place after every write to the store (a commit's, a spill's; §M14.20
W4: it was two, both under the commit lock).  Where the platform has
threads to spare (`parallelism() > 1`) a checkpointer thread is woken
and runs it — its slow half while commits go on (§4.6) —; in the
simulation, or when the WAL passes twice the limit (the thread fell
behind), it runs where it fell due, waiting for one running.  The
thread stops with the engine, after the checkpoint it may be running.
Write skew is possible under snapshot isolation (tested and documented).

**Large transactions** (§M14.11, §M14.20 V11).  A transaction keeps its
writes in memory up to `txn_memory` (64 MiB); past it they *spill* into the
store as its *intents*, under the commit lock, in 1 MiB batches: each key
is checked as a commit checks it (a newer version, or another running
transaction's intent of it → `Conflict`; told by the recent commits'
keys where they tell, else by the tree), and from then on any commit —
any write, even — of that key conflicts.  A reader resolves an intent by
its transaction's status: its own (above every version; writes still in
memory above that), committed at a timestamp (a version at that
timestamp, ranked with the others), or neither — running or aborted — and
passed over.  The commit is one batch as always — the writes in memory as
versions (the intents of those inside the spilled span deleted) and the
status record (`id → ts`) — so
it is atomic however large the transaction, and **each write is written
once**: nothing moves after the commit.  `vacuum()` later rewrites
committed intents as versions at their timestamps and deletes aborted
ones; once a whole sweep is done (durable), the statuses of the
transactions committed before it began are dropped.  Meanwhile reads pay
a little for them (a read of a key with intents walks them before its
versions; a scan merges them in one streaming pass), and vacuum leaves
alone the keys that have one (a commit's collection need not: an intent
ranks above every version).  Only the reads its intents can reach pay
(§M14.20 W5): a spilled transaction keeps the ranges of its intents' keys
— up to eight, disjoint; a key past them starts one of its own, and
beyond eight the two neighbours whose facing keys share the longest
prefix become one, so a load's catalog key, its rows and its index stay
apart and the tables between them stay out — running and, once
committed, until vacuum (after a restart they are not known: every read
counts them).  The statuses and the running ranges are read under a
reader-writer lock, so the workers sharing a transaction's reads look
at them at once (a mutex queued them: TPC-H Q19's lookups on a just
loaded `lineitem`, §M14.20 W6), and a read flushes pending unlogged
writes (D16) only in a transaction that has a fresh range.  A scan or a
key's read that no range reaches takes the plain path, as with no
intents at all; a write's conflict check looks at
the tree only for keys another's running ranges cover (or a large
commit's span).  The merging pass decodes each key into the batch once,
its value replaced by a better ranked version's (TPC-H Q6 over a just
loaded `lineitem`: 385 → 330 ms, 301 ms vacuumed; a query of `orders`
loaded before it, 67 → 51 ms, as fast as vacuumed).  A rollback (or a drop)
aborts the transaction — its intents nobody sees from then — and deletes
them in the key span it spilled; on opening, the status records say which
transactions committed, every other's intents are aborted (vacuum sweeps
them).  Serializable reads validate against the key span a large commit
wrote (lowest to highest key) beside the recent keys.  Store reads let a
waiting writer go first (a few hundred yields at most), so a stream of
scans over a large transaction's intents does not keep it from spilling.

**Fresh ranges without the WAL** (D16, §M14.20 W4; PostgreSQL's
`wal_level = minimal` and `COPY FREEZE`, CockroachDB's `AddSSTable`).
`fresh(start, end)` declares a range no version is in, no other
transaction writes before this one ends and none reads before it commits
(a new index's keys): its writes skip the conflict checks.  With
`EngineOptions::wal_minimal` (the default; `--wal full` turns it off, as
anything replaying the whole WAL — a replica, a point-in-time restore —
will) its values skip the WAL too.  The first write registers the range,
under the commit lock: declined — the writes logged as any — when another
running transaction writes an overlapping range so, or the store holds
any entry in it (a commit after this snapshot's, anyone's intent: what a
rollback would delete); else its mark `00 04 ‖ start → end` is logged.
Then each value above the highest key the transaction wrote in the range
(by either path: a key above it has no entry anywhere) goes into the
store as a **final version at the transaction's snapshot** — not an
intent a vacuum rewrites — gathered into 1 MiB batches
(`Store::write_unlogged`); a key at or below it (out of order, or written
again) takes the regular path, whose write wins.  The transaction's reads
write the gathered batch first; `put_new`'s run of keys found empty and
`first`'s empty ranges forget where such a write enters them.  The commit
writes the rest, **checkpoints** (commits go on meanwhile, §4.6) and then
logs its commit record, which deletes the marks: what the commit record
says committed is in the database file before it.  A spill or a commit
of anyone else with a key inside a registered range conflicts.  A
rollback deletes every entry of the range (logged deletes, in batches)
and then its mark; on opening, every mark left — a transaction a crash
ended — has its range deleted the same way.  Before its commit the range
is readable at snapshots from the transaction's on — no one reads it,
which the caller answers for (a new index nobody else's catalog shows);
after it, it is an ordinary one.  A rollback writes a delete per entry to
the WAL (rollbacks of index builds are rare; a range delete would make
it one record).

**Serializable** (`Isolation::Serializable`, §M14).  The transaction also
records the keys it read and the key ranges its scans consumed (a scan
counts up to the last row taken).  The engine keeps the keys of recent
commits with their timestamps (`recent.rs`): a small commit's in a map, a
large one's (256 keys or more) as the sorted run its write set already
is, moved — no key inserted one by one —, a spilled one's as its span;
the commits before a snapshot are skipped as a whole when a question is
asked for it.  Pruned below the oldest running transaction — and the
visible snapshot, which a transaction may still begin at while a commit
waits to be durable — once the map holds 4096 keys, or there are 64
spans or 32 runs; runs a long snapshot keeps past that go into the map.
At commit, if any recorded key or
range was written by a commit newer than the snapshot, the commit fails
with a retryable `Conflict`.  Read-only transactions never conflict.  This
is FoundationDB's read-conflict check rather than Cahill's SSI (D12): more
aborts in some patterns, much less state, and it distributes.  A write
whose key already has a newer version fails at the write, under both
levels — told by the recent commits' keys (every commit after a running
snapshot is among them), without a look at the tree, unless another
running transaction's intents' ranges or a large commit's span cover the
key;
the commit's own check — told the same way under the commit lock, the
tree read, with one cursor, only for the keys the recent ones cannot
answer — stays the guarantee.

**Validated reads** (§M14.5).  `get_validated` / `scan_validated` record
the key or consumed range in the same read set at *any* isolation level, so
a snapshot-isolation transaction can make one decision safe without
becoming serializable: its commit fails if a newer commit wrote what it
read.  Foreign key checks use them (§8.10).

**Garbage collection.**  Watermark = the oldest running snapshot (or
*visible*) minus `history` commits.  Garbage is every version but the
newest at or below the watermark (and that one if it is a tombstone).  A
commit drops it from the leaves it rewrites anyway — a compaction filter
on its batch (`Collect`, §4.4), for every key in those leaves, with no read
or write of its own: a leaf may hold only part of a key's versions, which
is enough (newer ones in an earlier leaf only make one more garbage), but a
tombstone a leaf ends with stays (older versions of its key may go on in
the next leaf).  What it drops is not logged; after a crash, replay brings
it back, invisible still.  `vacuum()` sweeps all keys, in chunks.
Snapshots register under the same lock the watermark is read under, so GC
never removes a version a snapshot can see.  `begin_with(at)` reads an older
snapshot within the history window (read-only), else `SnapshotTooOld`.

**Scans** read the store in batches — 8 visible keys at first, twice as
many each time up to 128, and no more than 4096 entries passed over
(intents nobody sees, versions too new) — re-seeking each time, so no
store lock is held between calls; MVCC makes the re-seek consistent.  Reverse scans are not implemented yet.

**Watched ranges** (§M14.20 W8; SQLite's schema cookie, made
snapshot-aware).  `Engine::watch(start, end)` adds a key range to the
watched ones; a commit that wrote into one — in memory, spilled or into
a fresh range — raises one mark, `watched_at`, to its timestamp under the
commit lock, before the commit is visible, and a range begins to be
watched with the mark at the last commit.  `watched_as_at(at)` answers
whether a transaction reads the watched keys as a snapshot at `at` did:
the mark at or below both its snapshot and `at`, and none of its own
writes in a watched range; never for a serializable transaction that
writes (its reads must be recorded).  A snapshot at or past a commit
sees its mark, so what a reader derived from the watched keys at `at`
(the SQL layer's catalog, §8.2) holds for every transaction that answers
yes.

Errors: `Storage`, `Conflict { key }`, `SnapshotTooOld`, `ReadOnly`,
`KeyTooLarge`, `Corrupt`; `is_retryable()`.

### 5.3 Tests (`tests/mvcc.rs`)

Own writes and rollback; stable snapshots (no dirty or non-repeatable
reads, no phantoms); uncommitted writes invisible; first committer wins;
write skew possible; no lost updates (6 threads incrementing with retries);
transfers between 8 accounts from 4 threads keep the total in every
snapshot a concurrent reader takes; GC with pinned snapshots, tombstones
and vacuum; commits collecting over many leaves, batches of every size,
snapshots of every age reading what they began with, through a crash and
replay, about a version a key left (the filter's own cases — a tombstone a
leaf ends with, a leaf beginning inside a key's versions, intents,
reserved keys — in `engine.rs`); a large commit (its keys one run)
conflicting as a small one does, at the write, at the commit and for
validated key and range reads (the write set's and the recent keys' own
cases in `writes.rs`, `recent.rs`); history; zero bytes and prefixes in keys; model-based scans
inside transactions; recovery of data and timestamps; a crash at every I/O
point (both modes).  Mutations caught: no conflict check; a GC watermark
that ignores running snapshots; a commit's collection dropping a tombstone
a leaf ends with, or a version at the watermark plus one; the recent
keys' runs left out of a key's or a range's question.  `tests/large.rs`: transactions past their
budget read, overwrite and delete their spilled writes and commit them
(their intents read as versions, then vacuumed into them, the same after
reopening); rollbacks and drops leave no intent, crashes leave aborted
ones vacuum removes; conflicts with intents both ways; committed intents
ranked with newer and older versions for readers at three snapshots,
through vacuum; an older engine's side area moved into versions on
opening (its uncommitted one dropped, new ids past its ids); serializable
reads against a large commit's span; a concurrent reader never sees part
of a large commit, and does not keep the writer from spilling; a crash at
every third I/O point (both modes) with spilling transactions, each state
a commit's, vacuumed to no intent.  `tests/unlogged.rs` (D16): a fresh
range's load writes almost no WAL (and the same load with `wal_minimal`
off writes it all), reads back pending and stored, commits versions — no
intent, no mark left — and survives a crash after its commit; a rollback,
a drop, and a crash before the commit (with a checkpoint midway, half of
the range in the database file) delete the range, which is free again
after; declined for a snapshot older than a commit into the range (whose
entry its rollback keeps) and for a range overlapping another's; others'
commits and spills into a range being written so conflict; `put_new`
sees an unlogged write inside a run it found empty; a model test (8
seeds) of in-order, out-of-order and repeated writes, deletes, `put_new`,
`get`, `first` and scans, with spills, through commit, crash and vacuum;
a crash at every I/O point (both modes) of loads past the cache with a
rollback among them, each state a commit's, no mark left.  Mutations
caught (14): no checkpoint at the commit, a rollback keeping the range,
recovery ignoring marks, no check of the store at registration, others'
commits or spills into the range let through, reads not writing the
pending batch first (`get`, `scan`), `first`'s empty ranges or
`put_new`'s run kept across an unlogged write, marks left by the commit,
the highest key not raised by logged writes, overlapping ranges both
registered, `put_new` looking at the tree before the pending batch is in
it.

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
  `random_bytes(buf)` — unpredictable bytes from the platform;
  `parallelism()` — the threads a statement may split its work across
  (default 1; the local implementation's platform's);
  `instance()` — which open database this is, unique in the process
  (`KvTxn::instance()` is its transaction's): what a layer above keys
  state it keeps in memory for one database by (an HNSW index's upkeep,
  §8.18); 0 by default (not told apart), one per `Database::open`;
  `scratch()` — scratch space for work too large for memory (a
  `Scratch`: `write_at`, `read_at`), private, gone when dropped; by
  default in memory (`MemoryScratch`), for the local implementation files
  in `<dir>/scratch/` (left-overs removed on opening).
- `Kv::vacuum()` / `Kv::vacuum_wanted()`: the store's maintenance and
  whether it has work (committed intents to make versions); by default
  none.  `Kv::vacuum_range(start, end)`: the versions no snapshot sees
  removed from those keys alone (keys with an intent left whole) — for a
  range used as a queue, keys put then deleted and never written again,
  whose tombstones a scan would otherwise step over until the whole
  vacuum runs (RocksDB's queue pattern; an HNSW index's delta, §8.18);
  `Engine::vacuum_range`, a chunk of the vacuum's sweep (`sweep_chunk`)
  bounded by the range.  `Kv::vacuum_background(go_on)`: the same in small batches (2 048
  entries), `go_on` asked between them — `false` stops the sweep there,
  and the next call goes on from it (with the transactions it began
  with: their statuses go when a sweep has passed every key).
  `Kv::activity()`: transactions begun so far; `Kv::running()`: running
  now.  The server's maintenance thread (`nucleus_server::maintain`)
  vacuums when no transaction began in the last 10 s and none is running
  (a long `COPY` is), stopping at the first one that begins; after 10 minutes
  without such a pause, paced instead (each batch followed by a pause
  four times as long).  Reads resolve intents by their status meanwhile,
  at about the cost of versions; vacuuming during a load's next
  statements (an index build, `ANALYZE`) or queries slowed them more.
- `trait KvTxn`: `read_ts`, `get(&self)`, `scan(&self, start, end) ->
  KvIter` (ascending), `get_validated` / `scan_validated` (reads checked
  at commit at any isolation level, §5.2), `scan_chunks` /
  `scan_chunks_validated` (the same entries a `Chunk` at a time — up to
  `CHUNK_ENTRIES` keys and values packed into one buffer the caller
  reuses, so a large read allocates nothing per entry; provided over
  `scan`, the local implementation fills chunks straight from the
  engine; a chunk scan may also `seek` forward to a key — `false` where
  it cannot — and fill a chunk with only the keys a `want` predicate
  takes, passing the others by their keys, their values not read, up to
  `most` of them: `next_chunk_where`), `shared()` (the transaction's
  reads as a `KvRead` — the same read methods, `Sync` — for worker threads to share while it is borrowed,
  `first` among them without the empty ranges remembered, which the
  workers would contend for; default `None`), `split_points(start, end, parts)` (up to `parts - 1`
  keys cutting a range into parts of about equal size, from the B+tree's
  internal nodes — each node's children in the range found by binary
  search —, none smaller than 64 leaves; default none), `first(start,
  end)` (the range's first entry, without a scan's read-ahead; the
  engine's transaction, outside `SERIALIZABLE`, remembers up to 8 ranges
  it found the store empty in — where none of its own writes lay — until
  its intents next enter the store, and answers a look inside one from
  its write set: a load's look past the stored rows reads the store once,
  not once a row),
  `put(&mut self)`,
  `delete(&mut self)`, `put_new(key, value) -> bool` (sets a key that has
  no value, else `false`; by default `get` then `put`, locally one look at
  the key's versions decides both the value seen and the write's
  conflict), `fresh(start, end)` (declares a range no version and no other
  transaction's write can be in until this one ends, and no one else reads
  before it commits — a new index's keys: locally, their writes skip the
  conflict checks, and their values the WAL, §5.2; by default ignored),
  `watched_as_at(at)` (whether it reads the keys `Kv::watch(start, end)`
  watches as a snapshot at `at` did — no commit between the two, nor
  this transaction, wrote one; §5.2; by default `false`, and `Kv::watch`
  does nothing),
  `commit(self: Box<Self>) -> Timestamp`, `rollback`.  Reads take `&self`
  so a scan can stay open while the same transaction reads keys.  A scan
  reads ahead 8 entries at first, twice as many at each refill up to 128,
  so one that wants a single entry reads few.
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
`checkpoint`, `vacuum`, `latest`, `scratch_files_made`, `close`.  `Options`: `cache_bytes` (128 MiB — 16 MiB on 32-bit targets —, a frame allocated when first used),
`checkpoint_wal_bytes` (1 GiB — 64 MiB on 32-bit targets), `wal_segment_bytes` (16 MiB), `history`
(0), `txn_memory_bytes` (64 MiB: a transaction's writes held in memory
before they spill, §5.2).  `tests/contract.rs` holds the contract as a suite any implementation
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
`nucleus`; the password also from `NCPASSWORD`).  An error prints its hint
on a `HINT:` line.  Statements end with `;` (a script on stdin,
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
| `\lint <statement>` | the linter's findings (`ANALYZE QUERY`, §8.13), or "No findings." |
| `\statements` | the 20 statements that took the most time (`nc_catalog.statements`) |
| `\copy <table> [(<columns>)] FROM '<file>' [WITH (…)]` | loads the file (text, or CSV with `FORMAT csv`) through `COPY … FROM STDIN` (§8.7), locally or over the network |
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

### 6.5 `nucleus mcp` (`modules/mcp`)

`nucleus mcp [<dir>] [nucleus sql's connection options] [--allow-writes]
[--max-rows <n>] [--timeout <ms>]` serves the Model Context Protocol on
standard input and output (JSON-RPC 2.0, one message per line; protocol
versions 2025-06-18, 2025-03-26, 2024-11-05), for a local database
directory or a server.  An MCP client — an AI assistant, an IDE — starts
it as a command (`{"command": "nucleus", "args": ["mcp", "/path/to/db"]}`)
and gets the tools: `list_tables` (name, comment, analyzed rows),
`describe_table` (columns with type, nullability, default and comment;
primary key, indexes, foreign keys, referencing tables), `sample_rows`,
`summarize_table` (`SUMMARIZE`), `query` (`$1`… from `params`, bound as literals), `explain` (and
`analyze`), `lint` (`ANALYZE QUERY`), and with `--allow-writes`
`execute`.  The initialize answer's instructions tell the model where to
start.

Reads cannot write: the edition parses a read tool's SQL with nucleus's
parser and refuses it (`25006`, with a hint pointing at `execute`) unless
every statement is a read (a query, `EXPLAIN`, `SHOW`, `ANALYZE QUERY`),
and the module runs it in `BEGIN READ ONLY` … `ROLLBACK`, after `SET
statement_timeout` (default 5000 ms).  At most `--max-rows` rows (default
100) are returned, `row_count` and `truncated` saying how many there were.
Results are JSON (`structuredContent` and the same as text): booleans and
integers as JSON's, `real` and `double precision` as numbers, the rest —
`numeric` too — as strings, NULL as null.  A failure is the tool's
(`isError`), its text `ERROR: … (SQLSTATE)` and `HINT: …`; an unknown tool
or method is a JSON-RPC error.  The module is the protocol over a
`Database` trait (SQL text in, answers with column types out); the
edition implements it for `nucleus sql`'s backends.

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

A value in memory (`types::Value`) is 32 bytes on 64-bit targets — a
tag and up to 24: a string, a decimal (its 128-bit mantissa aligned to 8
bytes, `repr(C, packed(8))`), an address; an enum's value behind an `Arc` —
checked when the crate compiles (§M14.20 W5: it was 64, which every row
and batch paid).

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
| vector | `VECTOR`, `VECTOR(n)` | 1–16000 `float4` components, a registered type (§8.12) |
| sparsevec | `SPARSEVEC`, `SPARSEVEC(n)` | up to 10⁹ dimensions, 16000 of them non-zero, a registered type (§8.12) |
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

**`vector`** (`vectors.rs`, §M14.12) is built the same way, on every
capability a type can have.  Its canonical bytes are its components as
little-endian `f32`s (`-0` stored as `0`, so equal vectors are equal
bytes); text `'[1,2.5,-3]'`, each component printed in the shorter of its
shortest decimal and exponent forms (`1e-7`); NaN, infinities, an empty
vector and more than 16000 components refused.  `vector(n)` checks the
dimensions on every write and cast (`22000`, "expected 3 dimensions, not
2"; `nc_catalog.columns` shows `vector(3)`).  Casts to and from `real[]`
(and from any numeric array; a NULL element is `23502`) and `text`.
Equality and hashing on the bytes (`DISTINCT`, `GROUP BY`, `UNIQUE`,
hash joins); a total order — component by component, then the shorter
first — with an order-preserving key encoding (`01`, then per component
its sortable big-endian bits, `00` at the end), so a vector can be a
primary key, sorted and B-tree indexed.  Distances: `<->` Euclidean (the
type's `Distance`), and as operators `<=>` cosine (NaN for a zero vector,
as pgvector), `<#>` the negated inner product, `<+>` taxicab; as functions
`l2_distance`, `cosine_distance`, `negative_inner_product`,
`inner_product`, `l1_distance`.  Each is a `nucleus-kernels` kernel over
the stored bytes — sixteen running `f32` sums added in a fixed order, so
the result is the same bits at every SIMD level (NEON, AVX2, AVX-512) —
and `ORDER BY v <-> $1 LIMIT k` is an exact nearest-neighbour search (a
top-k over the scan).  Vectors of different dimensions are `22000`.
Arithmetic component by component (`+`, `-`, `*`) and `||`
(concatenation), the type's `Arithmetic`; functions `vector_dims` /
`dims`, `vector_norm` / `l2_norm` / `norm`, `l2_normalize` / `normalize`
(a zero vector stays zero), `subvector(v, from, count)`.  Components by
position (the type's `Elements`, the same mechanism as arrays'): `v[i]`
a `real`, 1-based, NULL out of range; `v[lo:hi]` a vector, bounds
clamped, an empty slice `22023`; `UPDATE … SET v[i] = x` inside the
vector's dimension (`2202E` outside it, `22000` for a NULL component).
Aggregates `sum`
and `avg` (centroids), each component summed exactly (`fsum::ExactSum`,
§M14.27) and rounded once.  `crates/testkit/slt/45_vector.slt` follows
pgvector's definitions.

**`sparsevec`** (`sparse.rs`, §M14.12 V6b): a vector of many dimensions
of which few are not zero — a learned sparse model's term weights
(SPLADE), a bag of words — kept as its non-zero components.  Its
canonical bytes are its dimensions and its non-zero count (little-endian
`u32`s), their positions (0-based, ascending, `u32`s) and their values
(`f32`s); text pgvector's `'{1:0.5,4:2}/10'` (positions from 1, in any
order; zeros, `-0` too, dropped).  At most 1,000,000,000 dimensions and
16,000 non-zero components; a position outside the dimensions or twice,
NaN, infinities `22000` / `22003`.  `sparsevec(n)` checks the dimensions
on every write.  Bytes from a driver (`ncp` tag 11, pgvector's binary form
on pg-compat) are checked as text is and kept canonical (§10).  Equality and hashing on the bytes (no order).  Distances
over the positions merged, read in place from the stored bytes (`View`:
nothing allocated): `<->` Euclidean (the type's), `<=>` cosine (NaN for a
zero vector), `<#>` the negated inner product, `<+>` taxicab — the same
operators and functions as `vector`'s (`l2_distance`, `cosine_distance`,
`inner_product`, `negative_inner_product`, `l1_distance`, `l2_norm`,
`l2_normalize` take either: `vectors::VectorFn` resolves by its
arguments' type).  Casts to and from `vector`.  `ORDER BY v <#> q LIMIT
k` is an exact search (a scan and a top-k per morsel on the workers,
merged — `vector::sorted`, as every `ORDER BY … LIMIT` over a scan:
100,000 SPLADE-like rows in `docs/benchmarks.md`); an HNSW index refuses the type (`0A000`) — sparse
vectors' index is an inverted one, `USING sparse` (§8.21), which ranks
`<#>` exactly.
`crates/testkit/slt/54_sparsevec.slt` follows pgvector's definitions; the
law kit generates them.

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
`ARRAY(subquery)`, `a[i]` (1-based, NULL out of range), slices
`a[lo:hi]` / `a[:hi]` / `a[lo:]` (bounds clamped, PostgreSQL's), `UPDATE …
SET a[i] = x` (the array grown with NULLs, a NULL array taken as empty,
`2202E` below 1 or for a NULL position), `||`.

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
(`dml::rewrite_keys`; the decoder reads both).  Row encoding: version `03`, column count, the
offsets of columns 16, 32, … (`u16` from the row's start; `FFFF` for one
past 64 KiB, reached by reading on from the mark before it), tagged values
(missing trailing columns read as NULL, so `ADD COLUMN` needs no rewrite
unless it has a default); an integer is tag `12` (2 bytes), `13` (4) or
`2` (8), the fewest that hold it (version `01`, still read, had only `2`);
a real is tag `14`, a bytea `15`; a registered type's value is tag `11`,
its id, its encoding version and its canonical bytes; tag `16` (a
`u32` length) is a value stored out of line; text, `char(n)`, bytea and
an enum shorter than 256 bytes are tags `22`–`25` with a `u8` length
rather than a `u32` (`encoding::RowWriter`, `encode_row_value`; the wire
keeps `encode_value`'s).  A scan reading a few of a row's columns jumps
to the mark before each and skips the values between (`RowHeader::seek`,
for the row-at-a-time and the batch decoders alike).  Version `02` (no
offsets, no tags `22`–`25`) still reads; a row is written in `03` when
next written (§M14.20 W7, D18).

**Large values out of line** (`large`).  A value whose encoding is longer
than 2 000 bytes (`large::THRESHOLD`) — a file in a `bytea`, a document
in a `text`, any type but one whose values are read with their row
(`Type::stored_with_row`: `vector`, `sparsevec` — kept in the row, on
the KV's overflow pages past 2,048 bytes, one read with its key;
`large::out_of_line_value`, §M14.12 V7.2) — is stored in chunks of up to 32 KiB under the
table's reserved index id `FFFFFFFF`, keyed by the row's primary key, the
column and the chunk number; the row keeps tag 16 and the length, and
reads (`large::read_row`) put the value back.  The chunks are written and
removed with their row, in its transaction: a snapshot sees the chunks of
the row version it sees, a rollback or a crash leaves none behind (the
crash test writes large values), a changed primary key moves them, and
dropping the table removes them.  An update that leaves a large value as
it was writes none of its chunks.  A batch scan of the rows
(`RowBatches`) reads the values it needs through `large::Chunks`: one
scan of the chunks from the first row holding one to the scan's end,
walked beside the rows' (the chunks' keys follow the rows'), each
value's chunks read until they hold its length and a one-chunk value
decoded from the chunk as read — a step a value where a seek was
(§M14.12 V2: a scan of 768-float vectors 2.3× faster on its own); the
row-at-a-time executor looks each value up (`large::fetch`).  The
consistency check finds a value missing chunks and chunks without their
value.  A registered type's id is
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
| `00 04 <table id> 01` | the rows `ANALYZE` keeps for estimates (§8.8) |
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
| `00 14 <name>` | a secret: its owner and its sealed value, or the environment variable it names (§9) |
| `00 15 <name>` | a model: its provider, options, secret, prefixes, dimensions, batch and files (§8.23) |
| `00 16 <SHA-256> <chunk>` | a model's file, by content: 32 KiB chunks, shared by the models made from it (§8.23) |
| `00 17 <table id> <column u16> <pk>` | an embedding column's job: the row's vector to make; column bit `0x8000`: its next model's (§8.24) |
| `00 18 <table id> <column u16> <pk>` | a moving embedding column's next model's vector, until the swap (§8.24) |
| `01 <table id> 00000000 <pk>` | the row |
| `01 <table id> <index id> <index values> [<pk>]` | index entry → encoded pk |
| `01 <table id> FFFFFFFF <pk> <column u16> <chunk u32>` | a chunk of a value stored out of line |

A unique index entry is keyed by the index values alone (unless one is
NULL), so two transactions inserting the same value write the same key and
the second conflicts; other entries append the primary key.  The catalog is
read through the statement's own transaction, so DDL is transactional.

**A session's catalog cache** (`catalog_cache`, §M14.20 W8) keeps what
its statements derived from the catalog — tables' definitions, database
settings, objects' ACLs and owners, an identity's effective roles — for
the statements after.  A session asks the KV store to watch the catalog's
keys (`00 00`–`00 01` and `00 04`–`00 11`: not the counters, which
inserts change, nor the audit log) and lends its cache to the thread
running its statement; a lookup (`memo`) uses the cache when the
transaction reads the catalog as the snapshot the cache was filled at
(`KvTxn::watched_as_at`, §5.2), else empties it and fills it at the
transaction's snapshot if that reads the current catalog, else reads the
catalog itself: an older snapshot (a transaction begun before another's
DDL, `AS OF`), a transaction that wrote the catalog, a serializable one
that writes.  Work on other threads, and any other implementation of the
KV contract, read the catalog as before.  A parameterized point read on
one connection: 52 → 42 µs.

The same cache keeps **a query's plan** (bound, checked, optimized,
pruned: `session::Planned`) and **a statement's description**
(`Session::describe(stmt, text)`) by the statement's text — the text a
front end notes for a statement it runs (`note_text`), and gives when it
describes one.  An entry holds its statement and
serves an equal one only; a kept plan replays the tables its privilege
checks looked at (`audit_tables`).  A text is kept the second time it
comes (its hash remembered the first, up to 4 096), at most 512 of them,
all dropped past that; the cache empties when the session's identity
changes (`SET ROLE`, a login).  Only the statement a session runs is
kept by its text (`catalog_cache::with_text`), not those a function runs
inside it.

A session keeps two more things by a statement's text, with the same
admission and bound (`by_text::ByText`, the one type every entry by a
text is kept in): what the text parses to (`Session::parse`, which
`execute` and both front ends use) and its key in the statement
statistics (`statements::normalize`).  Neither reads the catalog, so
neither empties with it, nor with the session's identity.

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
name = value`, `SHOW name`, `EXPLAIN`, `ANALYZE [table]`, `CREATE
STATISTICS [IF NOT EXISTS] name [(ndistinct, dependencies, mcv)] ON a, b
[, …] FROM t` (2–8 columns counted together by `ANALYZE`; the kinds are
accepted, every group gets the same; names the database's, the owner's),
`DROP STATISTICS [IF EXISTS] name`, `COMMENT ON
TABLE t | COLUMN t.c IS 'text' | NULL` (the owner's; `''` removes it too;
kept in the table's record, shown in `nc_catalog.tables` and `.columns`,
`ncsql \d` and pg-compat's `obj_description` / `col_description` /
`pg_description`), `COPY t [(columns)] FROM STDIN [[WITH] (FORMAT text |
csv, HEADER [bool], DELIMITER 'c', NULL 'n', QUOTE 'q', ESCAPE 'e')]` (or
the older `WITH CSV HEADER DELIMITER ';' …`; §8.7).  Table names may
be qualified with `public.` (and, to read, `nc_catalog.`, `pg_catalog.`,
`information_schema.`).

Queries: `WITH [RECURSIVE] name [(cols)] AS (…)`, `SELECT [DISTINCT]`,
`FROM` with comma joins, `[INNER|LEFT|RIGHT|FULL|CROSS] JOIN … ON | USING`,
subqueries and functions in `FROM` (`generate_series`, `unnest`, any
scalar function; `WITH ORDINALITY`), `LATERAL` before a subquery or a
function (functions are lateral without it, as PostgreSQL's), alias
column lists (`AS t (a, b)`),
`WHERE`, `GROUP BY` (expressions, positions, aliases), `HAVING`, `WINDOW
name AS (…)`, `UNION |
EXCEPT | INTERSECT [ALL]` (INTERSECT binding tighter), `VALUES`, `ORDER BY`
(`ASC|DESC`, `NULLS FIRST|LAST`, positions, aliases, expressions), `LIMIT`,
`OFFSET`.  Expressions: arithmetic, `||`, comparisons, `AND/OR/NOT`, `IS
[NOT] NULL|TRUE|FALSE|DISTINCT FROM`, `[NOT] BETWEEN`, `[NOT] IN (list |
subquery)`, `[NOT] EXISTS`, `op ANY|SOME|ALL (array | subquery)`, scalar
subqueries, `[NOT] LIKE / ILIKE`, `~ ~* !~ !~*` (regular expressions,
linear-time), `CASE`, `CAST` / `::`, typed literals (`INTERVAL '90' DAY`,
`INTERVAL '5' DAY TO HOUR`: a bare number counts the last field, the
value cut below it, as in PostgreSQL), `E'…'` strings,
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
`DISTINCT`, `count(*)`, `ORDER BY` inside the call); window calls `f(…)
OVER name | ([name] [PARTITION BY …] [ORDER BY …] [ROWS | RANGE | GROUPS
{start | BETWEEN start AND end} [EXCLUDE CURRENT ROW | GROUP | TIES | NO
OTHERS]])`, a bound `UNBOUNDED PRECEDING | n PRECEDING | CURRENT ROW | n
FOLLOWING | UNBOUNDED FOLLOWING` (§8.5).

Not yet: `COPY TO` and `COPY FROM 'file'`, `TIMETZ`, time zones other than
UTC, multi-dimensional arrays, data-modifying CTEs, user functions.

### 8.4 Binder (`binder`)

Resolves names through nested scopes — a subquery sees enclosing rows as
`Outer { depth, index }`, which makes it *correlated* — reports ambiguous
(`42702`) and unknown (`42703`, `42P01`) names, infers types (an untyped
string literal or `NULL` takes the other side's type in comparisons and
arithmetic, never in `||`; a `$n` takes its context's type — including
`INSERT`/`UPDATE` target columns, for `describe`), lifts aggregates into an
`Aggregate` node (grouped expressions matched structurally; other column
references are `42803`), and places `ORDER BY` keys (output name or
position, else a hidden extra column, refused under `DISTINCT`).  A
lateral `FROM` item (`LATERAL (subquery)`, or a function) is bound with
the items before it — a join's left side, or the comma-separated items
before it — visible as the row it runs for (`Outer` at depth 1, the
enclosing queries' rows a level further); reading them, its join runs it
for each left row (`binder::lateral_join`: an index nested loop's
`JoinAlgo::Lookup`, below), else it is an item like any other.  A lateral
item cannot be the side a `RIGHT` or `FULL` join keeps whole (`42P10`,
PostgreSQL's rule); a subquery not marked `LATERAL` that names an item
before it is `42P01` with a hint to mark it.

### 8.5 Plans and the executor (`plan`, `exec`, `expr`)

Plan nodes: `Scan` (table, access path, filter), `Values`, `Filter`,
`Project`, `Join` (inner, left, full; nested loop or hash), `Aggregate`,
`Sort`, `Limit`, `Distinct`, `SetOp` (union, except, intersect),
`Cte` (computed once per execution, shared by its references),
`Recursive` / `WorkTable` (`WITH RECURSIVE`: the step re-runs over the
previous round until it yields nothing; `UNION` drops repeats, so cycles
end), `Function` (functions in `FROM`), `Virtual` (catalog tables),
`Window` (below).  The executor runs them as iterators; a join
stashes its right input, sorts and aggregates take their input — each
within `work_mem`, below.

**Window functions** (`window`, §M14.27 S4).  `f(…) OVER (PARTITION BY
… ORDER BY … frame)`, a `WINDOW` clause's named windows (one refining
another as PostgreSQL allows), every aggregate over its frame and
`row_number`, `rank`, `dense_rank`, `percent_rank`, `cume_dist`,
`ntile`, `lag`, `lead`, `first_value`, `last_value`, `nth_value`.  The
binder puts the windows above the grouping and `HAVING`, below the
projection, `DISTINCT` and `ORDER BY`: for each window (the calls with
the same partition and order, in the order they first appear) a `Sort`
by its partition's keys, then its order's — the one sort every `ORDER
BY` uses, in parallel over a pipeline and past `work_mem` by runs — and
a `Window` node, whose output is its input's columns followed by one per
call.  The node takes a partition at a time — held in memory — and finds
its peer groups once, comparing the keys in the rows (a key that is not
a column computed into the row while it waits).  Frames are
PostgreSQL's: `ROWS`, `RANGE` (an offset by the one ordering value:
numbers, dates and times with an interval; its bounds found by binary
search) and `GROUPS`, `EXCLUDE CURRENT ROW | GROUP | TIES`, offsets
evaluated once, the default frame from the partition's start to the
current row's last peer (the whole partition without `ORDER BY`).  An
aggregate's frames are evaluated by the cheapest way that fits them
(Leis et al., VLDB 2015): the whole partition — one state; frames from
the partition's start — one state grown row by row and finished at each
distinct frame end, in place where the aggregate's `finish` keeps its
state (`GroupAccumulator::finish_keeps_state`: every built-in but
`string_agg` and `array_agg`), else a copy (its partial combined into a
fresh group); a frame of up to 64 rows — its rows added to a fresh
group; larger frames — a segment tree of partial states over the
partition's rows, built when first needed, a frame the in-order
combination of O(log n) nodes, for every aggregate whose states combine
(the same `partial` / `combine` parallel aggregation uses, so the exact
sums stay exact, and `array_agg`'s order is kept); otherwise
(`string_agg`, an extension's aggregate without partial states) each
distinct frame's rows added to a fresh state.  Consecutive equal frames
reuse the last result; frames' groups are taken from an accumulator
renewed every 1024, so a long partition does not hold a state a row.
Errors are PostgreSQL's (42P20 for a window where none is allowed, a
nested call or a conflicting window; 42803 for a window inside an
aggregate; 22013, 22014, 22016 for a negative frame offset, `ntile` or
`nth_value` below one).

**Ordered sets** (`quantile`, `buckets`, §M14.27 S5).
`percentile_cont(f)`, `percentile_disc(f)` and `mode()` `WITHIN GROUP
(ORDER BY x [DESC])` — PostgreSQL's ordered-set aggregates — and
`median(x)` (DuckDB's: `percentile_cont(0.5)` of numbers and intervals,
`percentile_disc(0.5)` of any other ordered type).  The binder gives an
ordered-set aggregate (`Aggregate::ordered_set`) the arguments `[x, f…,
descending]` and shows the call as written in `EXPLAIN`; `WITHIN GROUP`
on another aggregate, an ordered-set one without it, or `OVER` one are
errors (42809, 42809, 0A000).  A fraction may be an array (an array
results); a constant one is checked when the call is bound
(`Aggregate::check_direct`), a computed one from the group's first row;
a fraction outside `[0, 1]` is 22003.  Exact, by selection rather than
by a sort: a group's values are kept as they come — integers, booleans,
dates, times and doubles as order-preserving 8-byte keys (a double's
bits, its sign flipped or all bits if negative, one NaN), other types as
values — and its result is the values at a few ranks, each found by
`select_nth_unstable` (introselect, O(n)), the next above the last;
`mode` sorts its group and counts runs (ties to the first in the call's
order).  States combine: a partial state is the fraction and the
group's values (keys as bytes), so values are collected on every worker
and gathered for the selection.  Past `work_mem` an accumulator moves
its values to scratch space and adds to them there.  Keys go as (group,
key) records into a run, read back by partitions of half of `work_mem`:
groups that fit are packed whole, a larger group is split by a count of
its keys over 2¹⁶ slices of their range into ranges that fit — a slice
of one key kept as a count, not a value — so each rank is selected in
the one partition that holds it: each key written twice and read three
times (to count, to partition, to select) whatever the run's size —
radix selection, not a merge sort's log-many passes.  Other values go into a `Sorter` of (group,
value), read once in group order.  `percentile_cont` interpolates as
PostgreSQL does — the values at `floor` and `ceil` of `f·(n − 1)` in the
call's order, `lo + t·(hi − lo)` — with `t` and the interpolation each a
fused multiply-add (PostgreSQL's results as its arm64 build computes
them; an x86-64 build rounds twice).  `width_bucket(x, low,
high, count)` is PostgreSQL's over doubles (its steps, its errors:
2201G) and over decimals (exact: `floor(count·(x − low) / (high − low))
+ 1` in wide integers), integers taking the doubles'; `width_bucket(x,
bounds)` counts the array's lower bounds at or below `x`, any ordered
type.  `histogram(x, low, high, count)` is the `count + 2` counts of
`width_bucket`'s buckets (below the range first, past it last),
`histogram(x, bounds)` the `len + 1`; counts combine, so a histogram
aggregates in parallel.

**Sketches** (`stablehash`, `hll`, `ddsketch`, `topk`, §M14.27 S6).
Approximate only by name, each within a stated bound and fixed in size.
A sketch keeps hashes from `stablehash` — xxHash64 (seed 0) of a value's
key encoding, so equal values hash alike in any form (`1.0`, `1.00`) and
the hash is the same on every machine and version: a format, unlike
`fasthash`'s per-process seed.  `approx_count_distinct(x)` is
HyperLogLog++: sparse (the set of 25-bit hash prefixes with their ranks,
near exact) up to 4096 prefixes, then 2¹⁴ registers (16 KiB, a standard
error of 0.81 %), read by Ertl's improved estimator — `+`, `*`, `/`, `√`
only, no bias tables, so an estimate is the same bits everywhere.
`approx_percentile(x, f)` (`approx_quantile`, `f` a fraction or an
array) is DDSketch: buckets of a factor `(1 + α) / (1 − α)` (α = 1 %),
found by DDSketch's cubically interpolated logarithm (exponent plus a
cubic of the significand) — a quantile within `α` of the value at rank
`⌊f · (n − 1)⌋`; at most 2048 buckets a sign (about 17 decades at 1 %),
the smallest magnitudes folded into the least kept past that; zero, the
infinities and NaN counted apart and ordered as SQL orders them.  Its
error is relative to the value, so on data far from zero (timestamps,
`1e9 + …`) a bucket spans millions and `percentile_cont` is the one to
use.  Both merge by maximum or by adding counts, and a sketch is a
function of the values it saw — their order, the workers, row or column
storage do not change its bytes (tested as a law in every executor).  As
values: the registered types `hll` and `ddsketch` (text form their
canonical bytes in hex, casts to and from `bytea`), made by `hll_agg(x)`
and `ddsketch_agg(x [, α])`, merged by `hll_merge` and `ddsketch_merge`,
read by `hll_count` (`hll_cardinality`), `ddsketch_quantile(s, f)` and
`ddsketch_count` — a rollup keeps a day's sketch and merges a month from
them.  `approx_top_k(x, k)` and `approx_top_k_counts(x, k)` are the
most frequent values and their counts, ties to the least value:
Misra–Gries counts in the mergeable form (exact until more than `2C`
distinct values, `C = max(1024, 8k)`, then the `(C + 1)`-th greatest
count taken from all), each count low by at most `(n − Σ counts) / (C +
1)`; exact and order-free while `x` has at most `2C` distinct values,
otherwise the same in every run of a plan (its states merge in the
morsels' order: it is not `order_free`).

**Sampling** (`sample`, §M14.27 S7).  `FROM t TABLESAMPLE BERNOULLI (p)`
or `SYSTEM (p)`, `p` a percentage, optionally `REPEATABLE (seed)` —
PostgreSQL's grammar and error codes (`2202H` a percentage outside 0–100
or NULL, `2202G` a NULL seed, `42704` another method, `42809` on a CTE
or a view).  The binder puts a filter over the table's scan, so the
sample composes with everything a scan does (columns, pushed-down
predicates, partitions).  With `REPEATABLE`, a row is in the sample when
`stablehash` of its primary key (all its columns without one), seeded
by the seed's bits, falls below `p / 100` of the hash space: the same
rows on every run, worker count, executor, version and machine, in row
and column storage alike, and a smaller `p`'s sample within a larger
one's.  Without it, each row is taken with probability `p / 100` from a
per-thread SplitMix64 seeded from the database's random source — a new
sample each statement.  `SYSTEM` samples rows as `BERNOULLI` does (no
block of storage is skipped yet).  `hash_unit(key [, seed])` is the same
hash as a `double precision` in `[0, 1)` — `WHERE hash_unit(id) < 0.8`
a training set and `>= 0.8` its test set, stable everywhere.
`random()` is PostgreSQL's, a double in `[0, 1)` from the same
per-thread generator.  Both draws are volatile but parallel-safe
(`ScalarFunction::parallel_safe`, PostgreSQL's `PARALLEL SAFE`; by
default a function is unless it is volatile): their calls do not depend
on one another, so a scan sampled without `REPEATABLE` runs on every
CPU.
`reservoir_sample(x, n [, seed])` returns `n` of the non-NULL values
(all of them when fewer) as an array, every set of `n` equally likely:
Li's Algorithm L, the reservoir `ANALYZE` samples with (`sample::Reservoir`),
seeded by `seed` (0 by default); reservoirs from the workers merge by
drawing without replacement how many values each gives (from the rows
each saw), so the result is uniform and the same every run of a plan —
not `order_free`, its gaps computed with `ln` and `exp`.  `n` is 1 to
10⁶ (`22023` otherwise).

**Drift and tests** (`drift`, §M14.27 S8).  Two samples of one column
told apart by a boolean `side` — `false` the reference, `true` the
sample compared (an integer 0 / 1 converts); a row with either `NULL`
is left out, and a side without rows gives `NULL`.  Named as scipy and
scikit-learn name their counterparts, with their answers.
`ks_test(x, side)` is the two-sample Kolmogorov–Smirnov test
(`ks_2samp`): `{statistic, p-value}`, the statistic — the largest gap
between the two empirical distribution functions, ties stepping
together — found in integers (`max |a·n − b·m|` over `m·n`, rounded
once); the p-value exact while neither side has more than 10,000 values
(the share of random interleavings that leave the band, by the
lattice-path recurrence, the outside counted directly so a small p
keeps its digits), past that the Kolmogorov distribution at
`(√e + 0.12 + 0.11/√e)·D`, `e = m·n/(m + n)` (Stephens; scipy's
`asymp` takes the finite-sample distribution — at 15,000 a side 0.4134
where this gives 0.4144).  `auc(score, label)` is the area under the ROC curve,
`label` true for the positives (`roc_auc_score`): the Mann–Whitney rank
sum, ties counted half, in integers.  `psi(x, side [, bins])` is the
population stability index over `bins` (10; 1 to 10,000) bins of equal
frequency in the reference — the edges the reference's values at ranks
`⌊k·m/bins⌋`, a value in the bin of the edges at or below it, ties in
one bin — `Σ (q − p)·ln(q/p)`, an empty bin's share counted as 10⁻⁴
when the other side's is not.  These three keep each side's values as
the ordered sets do (`quantile::Doubles`, `work_mem`, spilling,
combined from the workers) and walk both in ascending order once, the
compared side's distinct values held in memory up to half of `work_mem`
and read back from scratch space past it.  Over categories (any type,
equal by the key encoding): `js_divergence(x, side)`, the
Jensen–Shannon divergence of the two sides' distributions in base 2 (0
to 1; scipy's `jensenshannon(p, q, base=2)` squared), and
`chisq_test(x, side)`, Pearson's χ² test of homogeneity on the 2 × k
table (`chi2_contingency`, Yates' correction at one degree of freedom):
`{statistic, p-value, degrees of freedom}`, the p-value the regularized
upper incomplete gamma function (series below `a + 1`, Lentz's continued
fraction above, `ln Γ` by Lanczos).  Their counts are a map per group,
at most `work_mem` of distinct values (`54000` past it: bin first with
`width_bucket`), summed in the values' order, so neither result depends
on the rows' order.  The ordered sets' 8-byte keys are sorted by an LSD
radix sort (`quantile::sort_keys`): a byte a pass, the bytes every key
shares skipped, the standard library's sort for fewer than 4096 keys or
more than six differing bytes.

**`SUMMARIZE`** (`summarize`, §M14.27 S8b).  `SUMMARIZE t` or
`SUMMARIZE <query>` (DuckDB's statement) returns one row a column of
the source: `column_name`, `column_type`, `min` and `max` (as text, for
numbers, text, enums, dates and times), `approx_unique`
(`approx_count_distinct`), `avg` and `std` (numbers), `approx_q25`,
`approx_q50`, `approx_q75` (`approx_percentile`, numbers), `count` (the
rows) and `null_percentage` (two decimals) — DuckDB's columns, except
that its approximate quartiles `q25` … are named `approx_` here, as
nucleus names everything approximate.  It is one aggregate query over
the source (`SELECT count(*), count(c0), approx_count_distinct(c0),
min(c0)::text, … FROM (<source>) AS summarized(c0, …)`), so one scan
of every column, in parallel, its result turned into rows; a read (the
MCP tool `summarize_table` runs it).  Not yet a `FROM` item.

**Columns** (`prune`, §M14.11).  Before a query runs, its plan is walked
from the top with the output columns wanted, each node adding what its
expressions read; a scan then decodes only the columns needed — the rest
read as NULL, skipped in the encoding without being built (an out-of-line
value not even fetched), and a row's encoding not read at all past the
last column wanted (`count(*)` reads none of it).  An expression with a subquery needs every
column (a correlated subquery reads its row), as do `DISTINCT`, set
operations, CTEs and recursion.  Subqueries' own plans decode only
their columns too (a correlated index lookup into a column table, say,
decodes the two columns it reads, not the segment's every one).  In a
query, a correlated subquery's result is kept by the values it reads of
the rows around it (each level's): a row with the same ones reuses it —
`WHERE l_quantity < (SELECT 0.2 * avg(…) WHERE l_partkey = p_partkey)` runs
once per part, not per line — unless it calls a volatile function or reads
a table function (results of up to 1 024 rows, 65 536 per subquery).  A grand total (an aggregate without `GROUP BY`) keeps one state,
hashing no key.

**Batches** (`vector`, §M14.20).  A table scan by the primary key (or of
every row) and the filters and projections above it run as a *pipeline*,
a batch at a time: a vector per column — integers, doubles and booleans
typed, with a `NULL` mask; decimals of one scale their mantissas
(`Vector::Dec`: a column store's decimal segment as stored, a row table's
numeric column when its rows share a scale — 64-bit while each fits,
`Mantissas::Narrow`, so a cached decimal column takes half the memory,
else 128-bit; the kernels read either, widening as they go, §M14.20 W5);
other types a
value per row; a column nothing
reads a `NULL` constant — and a selection of the rows still in play.  The
first batch is 64 rows, each next one twice the last, up to `batch_size`
(default 1024; `SET batch_size = 0` runs everything a row at a time).
Rows are decoded from the stored encoding straight into the vectors.
Expressions are evaluated a batch per call: kernels for integer and
double arithmetic and comparisons (an integer widened against a double,
as a row widens it), decimal `+`, `-`, `*` and comparisons on the
mantissas (an integer a decimal of scale 0; `+`, `-` and comparisons at
the larger scale, `*` at the sum — `Decimal`'s rules, a result past 38
digits failing on a row in play; a comparison whose raising would
overflow compared value by value), text and `char(n)` comparisons (a `char(n)` side
without its trailing blanks, as a row compares it) and `LIKE` whose only
wildcard is `%` (at the ends: a prefix, suffix or substring; within too:
its parts found in order, each after the last — `'%special%requests%'`;
substrings searched for by their first and last bytes, 32 places at a
time), `IN` lists of constants over text, `char(n)` and integers (in a
hash set past eight; other lists value by value, over the operand and the
items only), comparisons of dates, times and timestamps of one kind,
`AND` / `OR` / `NOT`, `IS NULL`; an uncorrelated scalar, `EXISTS` or
`ARRAY` subquery once (its rows are cached for the statement), an
uncorrelated `IN (subquery)` as the list of its rows; function calls
through the function's batch kernel when it has one
(`ScalarFunction::call_batch`, §8.12: `substr` over text and `char(n)`
with constant positions), else casts and calls a value at a time over
the batch's vectors; `CASE` a branch at a time — each `WHEN` over the
rows no earlier one took, its `THEN` over the rows it takes, `ELSE` over
the rest, so each row evaluates what it would alone; an expression of
constants (an operator, cast or immutable call of constant operands, an
`ARRAY[0.5, 0.95]` of literals) once per batch, as a constant vector —
not a value per row; anything else
(correlated subqueries, decimal `/`, `ILIKE`, …) a row at a time, each
row holding only the columns the expression reads (a long text it does
not read is not copied).  A row at a time, `Expr::eval` takes
its common nodes (columns, literals, parameters, operators, `IS NULL`)
itself and the others in a function of their own (`eval_other`), so each
node of a row's expression takes a small frame (480 bytes, not 1.7 kB).
Text is a column of offsets and bytes
(Arrow's layout), `char(n)` the same as stored (blank-padded), a vector
of its own from row and column storage alike; dates and times their
integers.  The kernels
(`nucleus-kernels`) are dense loops over every position of a batch,
written so the compiler vectorizes them and compiled once per CPU level —
AVX2 and AVX-512 on x86-64, chosen at run time by feature detection;
NEON, the baseline, on AArch64 — what can fail (an overflow, a division
by zero) coming back as a mask the executor checks on the rows in play.
Each has a scalar reference; `crates/sql/tests/kernels.rs` runs every
kernel at every level the CPU has against it, on edge values, and the
references against the SQL layer's own definitions.  Grand totals over a
whole batch sum and compare integers in a kernel (exact, in two 32-bit
halves); group keys hash through a seeded multiplicative hash.  `AND` and `OR` evaluate their right side
only on the rows the left one leaves undecided.  Aggregates over a
pipeline are vectorized when none has `DISTINCT` or `ORDER BY` (most
`DISTINCT` calls the optimizer makes two groupings, §8.6): group
numbers by key — one integer key by a direct index while the keys lie in
a range of up to 65 536 values (the range found as they come, grown
twofold), hashed as an integer past it; any other key by a salted table
(`keytable`, DuckDB's: slots of a group's number and the upper half of
its key's hash, so a slot of another salt is passed over unread), the
row's key hashed and compared from the batch's vectors as they are — no
key built to look a group up, only a new group's —, and each
aggregate fed a batch at a time through `Aggregate::grouped()` — the
built-ins' states a column each, of the kind the aggregate and its
argument need — a value for `min`, `max`, `bool_and`, `bool_or`, an
exact sum and a row count for a `sum` or `avg` of integers or of
decimals at one scale (their mantissas added), else a general state a
group, which such a sum becomes when it meets another scale or kind —,
updated in place from typed vectors — or row by row
through its accumulators; past `work_mem`, rows of new groups go to
partitions as below.  A pipeline whose output becomes rows (for a sort,
a join, the client) runs vectorized only when it filters.  The result is
the row-at-a-time one, errors included: a batch whose evaluation fails is
run again a row at a time — the rows before the first failing one, then
its error, so a `LIMIT` that ends first still ends without it.  Under a
limit on the rows read (a token's ceiling) a statement runs a row at a
time, since batches read ahead.  `EXPLAIN ANALYZE` shows each node of a
pipeline with its rows (times include the inputs').  The test kit's
scripts run with batches of the default size, of 3 rows and with none
(`crates/sql/tests/slt.rs`); `crates/sql/tests/vector.rs` compares the
three on chosen, failing and random queries.  Integer `sum` and `avg`
accumulate exactly (128 bits), so a sum overflows only when its result
does, whatever the order.

**Parallelism** (`vector`, `parallel`, §M14.20).  A pipeline's scan
(not one of a single key of a unique index: a row at most) splits into
morsels at the KV's split points (`split_points`), run by
worker threads — up to `parallel_workers` (default 0: as many as the
database's CPUs, `Kv::parallelism`; 1 turns it off; the simulation
offers one) — each with its own execution context over the
transaction's shared reads (`KvTxn::shared`) and the statement's
ambient state (who runs it, its time, its budget and deadline).  An
aggregate's morsels are aggregated apart and their partial states
combined in key order (`GroupAccumulator::partial` / `combine`, a
capability: an aggregate without it — `string_agg`, whose separators
belong to rows — runs in one pass).  Many groups (16 384 partial ones or
more) in no order of their keys are merged on the workers too: each
morsel splits its groups into 64 parts by their keys' hashes, and each
part is merged on a worker (ClickHouse's two-level tables) — the rows
then put in the order a single pass first saw them, the first error by
that order (a combining one before any finishing one).  All of it within
`work_mem` (DuckDB's radix-partitioned aggregation): the running morsels
share half of it, a share each — past its share a morsel writes its
groups' partial states to scratch space, part by part, and goes on with
an empty table —, a morsel done keeps its groups in memory only while
those kept fit in the other half, and a part reads a morsel's spilled
groups back before those it kept, so each group's place and the errors
are those of a merge in memory.  Merged rows past half of `work_mem` in
all come a part at a time, each part's in the order first seen, those
that did not fit beside the others from scratch space.  When every
aggregate is order-free (not `array_agg`, not `string_agg`), a worker
keeps one table across the morsels it takes, unless its second morsel
shows its groups do not recur (then each morsel's table is handed over
as before); a group's rank is its morsel and its place in it, a merge
takes a group's least, and the merged groups are finished in rank
order — a single pass's order whichever worker took which morsel.
Groups in key order are merged as §8.6 says.  The builtin states pass between workers compactly: `count` an
integer (its own accumulator, a count a group), `min`, `max`,
`bool_and`, `bool_or` their value, a `sum` of no doubles its value (or
its integer sum's bytes), an `avg` of no doubles the pair of its rows
and that; what a group's states take counts against `work_mem` as each
accumulator says (`GroupAccumulator::group_bytes`: 24 bytes for such a
sum, 32 for a value, 8 for a count); a group's accumulators grow by half again, not
a group at a time, and a single integer key is found and placed in one
probe.  A pipeline whose output becomes rows
runs in rounds of a morsel per worker, each round's rows given in morsel
order — one that only projects, too, unless a `LIMIT` stops it within
65 536 rows (`vector::SERIAL_ROWS`: then it streams on the statement's
thread, a round's morsels would read more; the limit marks the chain
below it).  So the result is a single pass's — groups in the order first
seen, rows in key order, the first error the one a single pass meets, a
`LIMIT` stopping after the round it needs.  `UPDATE` and `DELETE` find
their rows the same way (vectorized, in parallel when they can be) when
the table is keyed by a primary key — each row's key computed from its
values; a policy, or a table keyed by hidden row ids, scans row by row.  Sums of doubles and reals are
exact (`fsum::ExactSum`, §M14.27 S1: Neal's superaccumulator — the sum
as an integer in units of 2⁻¹⁰⁷⁵, in 32-bit chunks held in 64-bit
words, only the chunks touched kept; an addition or subtraction is three
integer additions, `add_product` adds x·y exactly — the significands'
106-bit product from one 128-bit multiplication, added as five chunks;
the result rounded once to nearest even from the integer;
a worker's state passed as bytes — the chunks carried, all but the top
in four bytes, without the zeros at either end — and added from them
without building a state), so they do not depend on the order either;
`avg` of doubles is the exact sum divided by the count and rounded once
(`ExactSum::mean`), not the rounded sum divided (PostgreSQL's, which
rounds twice).  So do the moments (`moments`, §M14.27 S2): `var_samp` / `variance`,
`var_pop`, `stddev_samp` / `stddev`, `stddev_pop`, `skewness`,
`kurtosis`, one `GroupAccumulator` for all, combinable, its state the
row count and the power sums.  Over doubles Σx … Σx⁴ are `ExactSum`s,
each power added exactly (x² one `add_product`; x² = h + l by a fused
multiply-add, then x³ as h·x + l·x and x⁴ as h² + 2hl + l²), so
the central sums are exact integers — C₂ = nΣx² − (Σx)², C₃, C₄ — formed
in `bigint::BigInt` (sign and magnitude, base 2³², only at the end) and
rounded once: the variance as C₂/(n(n−1)) rounded from the integer
quotient and its remainder; the standard deviation the square root of
that; skewness √(n(n−1))/(n−2) · C₃/C₂^1.5 and kurtosis
(n−1)((n+1)C₄ − 3(n−1)C₂²)/((n−2)(n−3)C₂²) from the exact numerators,
NULL below 3 (4) rows or with no spread, as DuckDB's.  Over integers and
decimals the variance and standard deviation are PostgreSQL's
`numeric_stddev_internal`: Σx and Σx² exact in `bigint::Wide<8>`
(512-bit two's complement, no allocation a row), at the inputs' largest
scale; the result at PostgreSQL's scale (`select_div_scale`: 16
significant digits, at least twice the input's), half away from zero,
the root an integer square root — the same digits, up to `numeric`'s 38
(beyond: 22003, where PostgreSQL prints 39).  The two-variable
aggregates (`regression`, §M14.27 S3) — `corr`, `covar_pop`,
`covar_samp`, `regr_slope`, `regr_intercept`, `regr_r2`, `regr_count`,
`regr_avgx`, `regr_avgy`, `regr_sxx`, `regr_syy`, `regr_sxy`,
PostgreSQL's, arguments `(y, x)`, doubles — keep n and only the sums
their result needs of Σx, Σy, Σx², Σy², Σxy (`ExactSum`s; integers read
from their vectors without a `Value` each), so Cxx = nΣx² − (Σx)², Cyy
and Cxy = nΣxy − ΣxΣy are exact integers, and each result is one ratio
of them rounded once (`moments::ratio`, Knuth's long division in
`BigInt::div_big`); `corr` the root of the exact Cxy²/(CxxCyy), rounded
once (`moments::sqrt_ratio`: the floor of the root of the floor is the
floor of the root, exact only if both steps were).  PostgreSQL's rules:
a row with either value `NULL` left out; `NULL` without rows, for
`covar_samp` with one, and where x does not vary (and y, for `corr`);
r² 1 for a horizontal line; `NaN` where an infinite or `NaN` input
enters.  Not in parallel: an enclosing row (a correlated
subquery's own plan), `EXPLAIN ANALYZE`, a volatile function or a
subquery the workers cannot run in the pipeline (below), groups in key
order past `work_mem` in a morsel, or merged on the statement's thread
past it (then the single pass partitions).  The buffer pool reads a
missing page without holding its lock, so workers reading cold pages do
not wait on each other; its shards and frames take a cache line each
(128 bytes), and a hit is counted in its shard, so workers finding pages
do not write one line (a lookup's every descent starts at the same
root).

Subqueries where workers evaluate (`expr::ready_subqueries`): an
uncorrelated one runs once on the statement's thread first, its rows
cached and given to the workers; one that reads a volatile function or
fails keeps the work on one thread, which meets it where a single pass
would.  A correlated one runs on the workers for each row, as it would
on the statement's thread, when the statement is a query (nothing it
runs writes), no rows it inserted into a column table are held unstored
on the thread, and the subquery reads only the row it runs for (not one
further out) and nothing that differs by thread or run — no volatile
function, no table function, no CTE, recursive CTE's work table or
catalog view, its own subqueries included (PostgreSQL's rule for a
correlated `SubPlan` in a parallel plan, the check recursing into it).
A filter or projection whose subquery runs for each row over rows that
are not a pipeline's — a join's (TPC-H Q20: a condition with a
correlated subquery stays above a semi join) — runs on worker threads
the way an index nested loop's lookups do (`exec::OnWorkers`, below):
its input read in blocks, each block's pieces evaluated on the workers,
the rows given in the input's order, its error where a single pass
would stop, and none from rows a `LIMIT` stops short of.

**Memory** (`spill`, §M14.11).  What an operator holds is bounded by
`work_mem` (`SET work_mem = '256MB'`, or the database's setting; default
64 MiB): past it, the rest goes to scratch space — the KV contract's
`scratch()` (§6.1), files in the data directory's `scratch/` — and is read
back.  Sorts sort records in chunks, each kept in memory while the
sort's chunks fit in `work_mem` and written to scratch space past it,
then merge them by key ranges (`sorter`, below; ties keep the input's
order) — under a `LIMIT` (a sort right below it), only the rows
it takes and skips, kept in a heap of that many once twice as many
came (`spill::Top`, PostgreSQL's bounded sort) while it fits, a row turned away by one
comparison, and the full sort past `work_mem`; materialised CTEs, recursive CTEs' rounds and results,
and the rows a write collects before writing (`INSERT … SELECT`,
`UPDATE`, `DELETE`, rewrites) are kept in memory up to the limit, the
rest in a run; writes then apply them 4096 rows at a time.  Index builds
sort their entries within the same limit (§8.7, *Index builds*).
Table functions stream (`generate_series` makes its rows as they are
read).  Hash aggregation, `DISTINCT`, `UNION` and `INTERSECT` / `EXCEPT`
keep their groups, the rows seen, or the right side's counts in memory up
to the limit; past it, rows of new groups (rows not yet seen; both sides
of a set operation) go to 32 partitions by a hash of their key, each
taken in turn after the input — a partition too large splits again, up to
four levels — so no aggregate's state ever leaves memory, and each
partition holds every row of its keys.  Joins stash their right input:
in memory, a hash join probes it by key and a nested loop reads it all
for each left row (a full join marks the right rows matched and gives
the rest after); past the limit, a hash join is a grace hash join (both
sides partitioned by key, each pair joined in turn — a right side still
too large splits again; rows with a NULL key match nothing, coming out
padded for LEFT and FULL) and a nested loop reads the right side once per
`work_mem`'s worth of left rows (a block nested loop, the matched right
rows of a full join in a bitmap).  Still held whole: a recursive `UNION`'s
rows seen, and the validated reads of a transaction (§5.2).
Subqueries run per row when correlated, once per execution otherwise
(cached in the execution context) — unless unnested into a join (§8.6).
A **hash join whose left side is a pipeline** (a scan, its filters and
projections) and whose right side fits in work_mem runs vectorized: the
right rows hashed once (`vector::JoinTable`, its keys found as a
grouping finds its groups — `keytable`: one integer key in `IntKeys`, a
direct index over a narrow range; other keys in `KeyTable`, a probe's
key hashed and compared from the batch's vectors with no key built; two
integers as a pair in the executor's own hash, `fasthash` —; each key's
row numbers side by side in one array, no allocation per key), the left side's keys evaluated a batch at a time
and probed — the batch's lookups all first, then its output, so lookups
that depend only on their keys overlap their cache misses (what
prefetching does in a hash probe; no `unsafe` prefetch instruction), no
key built for a lookup — on worker threads,
morsels of the left scan in key order, when it is large — with the rows,
their order and errors of the row-at-a-time join; a condition that is just
the integer keys' equality is not checked again per row.  When the right
side is a pipeline too, the workers collect it — morsels in key order,
their rows and keys' values joined in order, so the table is a single
pass's — a column at a time (`columns::Columns`: an array a column, typed
while its values are of one kind — an integer 9 bytes, not a `Value`'s
32 —, no allocation per row), unless it passes work_mem (then it is
stashed, and spills).  A right side that is not a pipeline (a join's
output, a group's) is gathered in columns the same way, its keys then
evaluated — after every row, as when stashed —, and probed as above;
once the columns pass work_mem, the rows gathered so far and the rest
are stashed instead.  **Joins inside pipelines**: an inner, left, semi or
anti hash join whose left side is such a pipeline is itself a step of
it (`vector::Step::Probe`), so joins over joins, and the grouping, sort
or projection above them, run as one pipeline, on the workers' morsels
of the lowest scan.  Before it runs, `vector::prepare` makes each join's
table in turn — outermost first, as the row-at-a-time join runs its
right sides —, with its key filter; the workers' contexts get the
tables (`Ctx::join_tables`), which drop once the pipelines that probe
them are made.  A probe step joins a batch into a batch: each left row's
matches in the table's order, its columns gathered from the left batch
and from the table's columns (`Columns::gather`), a left join's
unmatched rows with `NULL`s, a semi or anti join's rows by the
selection alone; an `on` beyond the keys is evaluated over every
candidate pair at once, and an error there runs the batch again a row
at a time, so the rows before it and the error are the row-at-a-time
join's.  A right side past work_mem is stashed, the pipeline above it
does not run as one, and the row-at-a-time join takes the stashed rows
and the tables made above them (`vector::take_built`) — nothing runs
twice.  Not with an enclosing row (a correlated subquery's plan, which
runs again per row); a recursive CTE's term makes its tables each
round.  **Key filters** (`keyfilter`): once an inner or
semi join that hashes its right side has that side in hand — narrowed by
a filter, a join or a group, not a whole table — the integer values of
each key go into a filter (their least and greatest; the values
themselves when 64 or fewer, else a Bloom filter, a word a key, three
bits in it) for the table scans below the probe side that key's column
comes from (through projections, filters, sorts, partitions and the
joins that keep their rows: not a limit, a group, the padded side of an
outer join).  The scan drops a row whose key no right row has, before
its own filter — that row would leave at the join —, and a column store
skips the segments whose least and greatest values miss the keys'
range.  The filter is made in every executor alike (a row at a time,
by batches, on workers, past work_mem), so their rows and errors agree;
an error on a row only the key filter drops does not come.  `EXPLAIN
ANALYZE` marks the scan `(join key filter)`.  A **sort** (`sorter`,
§M14.27 S4·sort) takes each row and its keys as one record of bytes:
the keys *normalized* — each a byte placing a `NULL` first or last, then
its index encoding (`encoding::encode_key_value`), a descending key's
bytes inverted, so the records' bytewise order is the keys' —, then the
keys' values and the row's (the scratch codec).  A record is a few dozen
bytes where a row of values is hundreds, and two compare by their first
eight bytes as an integer, then by `memcmp`.  Where a key column holds
values whose encoding does not order as the comparison does (intervals,
enums, registered types, arrays) or of two kinds (a `UNION`'s integers
and doubles), records compare by their key values.  A sort of a
pipeline runs a sorter a morsel on the workers (keys a batch at a time);
a sorter fills a chunk of records (`work_mem` shared by twice the
workers), sorts it stably and keeps it — in memory while the sort's
chunks fit in `work_mem` (`sorter::Budget`), else in scratch space with
every 128th record's key as an index.  The merge cuts the keys into
ranges by splitters sampled from the chunks (their indexes, on disk),
finds each chunk's share of a range by binary search (or through its
index), and merges the ranges on worker threads, two a worker ahead of
the reader, their rows given in order; ties go to the earlier chunk —
the morsels' order —, so the order is a single pass's stable sort.
Under 32 768 rows, or on one worker, one heap merge streams.  Under a
`LIMIT` each morsel keeps its own heap of the rows taken, a row built
only when its keys enter it, and the heaps are merged the same way.
A sort is **not run** when its input is already in its order
(`optimizer::sorted`): a table is read in primary-key order — a row
table's tree, a column table's segments and delta merged, a parallel
scan's morsels in key order —, through filters and projections, so an
`ORDER BY` of a prefix of the key, ascending, is that order (the key is
never `NULL`; ties, in a stable sort, keep the input's order — the
key's — too; past the whole key, further keys change nothing), for keys
of kinds whose encoding orders as `ORDER BY` does.  `ORDER BY id LIMIT
10` then reads ten rows.  A projection left giving its input's rows as
they are is dropped (`optimizer::same_rows`).
An **index nested loop** (`JoinAlgo::Lookup`, `EXPLAIN`: `Index Nested
Loop`) runs its right side — a table's scan by an index on the join keys,
the left row's values as `Outer` references — once per left row (inner,
left, semi and anti joins).  A lateral item's join is one too (`EXPLAIN`:
`Nested Loop (per row)`), its right side any plan; the optimizer's
passes keep it whole (not reordered, no semi join moved into its right
side) and the column pruning keeps the left columns it reads.  It runs on worker threads when it can — a
lateral item's right side too, where it is a scan (by a key range or an
index method's ranking, `Access::Method`) under filters, projections, a
sort and a limit, each worker running it whole for its left rows (a
worker's statement work is not shared out again: `budget::workers` is 1
on a worker, whose transaction gives the index methods the shared reads
and the database's instance, `parallel::SharedTxn`): the
left rows read in blocks (256 at first, each next twice the last, up to
16 384), a block cut into pieces the workers look up over the shared
reads, the pieces' rows given in the left rows' order — a single pass's
rows, and its error where it would stop (`exec::OnWorkers`, which a
filter's or projection's subqueries share, §8.5 "Subqueries where
workers evaluate").  Not under `EXPLAIN ANALYZE`, for an enclosing row,
for a lookup or condition with a volatile call or a subquery the workers
cannot run (`vector::parallel_safe_in`), or while a statement on the
thread holds rows it inserted into a column table unstored (only that
thread's reads see them).
A **semi join** gives each left row some right row matches, once; an
**anti join** each left row none matches (a NULL key matches nothing, so
its row comes out of an anti join); both give only the left's columns,
by hash, grace hash or block nested loop like the others.  Where the left
side is the smaller by estimate (half the right's or less) a hash join
hashes it instead (`EXPLAIN`: `Hash Join (left hashed)`): the right
pipeline probes the left rows' table a batch at a time, on worker threads
when large, marking the rows it matches; then the left rows in order,
those marked (semi) or not (anti).  Expressions use three-valued logic,
checked integer arithmetic (`22003`), division by zero (`22012`).

### 8.6 Optimizer (`optimizer`)

Predicate pushdown (through projections, into join sides, outer-join safe;
a condition with a correlated subquery stays above a semi or anti join,
which only drops rows — an uncorrelated one runs once and goes below; an
`OR` across both sides of an inner join pushes down to each the OR of its
branches' conjuncts on it, when every branch has one, the `OR` itself
kept; a scan's filter holds each conjunct once), WHERE conjuncts into join conditions — the conjuncts every
branch of an `OR` has taken out of it first (`(a AND b) OR (a AND c)` is
`a AND (b OR c)`; not volatile calls or subqueries), so a join condition
repeated in each branch joins —, hash joins on equality conjuncts,
access paths: primary key or index with an equality prefix and a range on
the next column; the filter always stays, so an access path only narrows.
A condition whose correlated subquery reads this row's columns goes only
where they all are, and moving it (a join side, a reordered join, through
a projection) rewrites the subquery's references with it.  A scan
filter's comparisons with constant expressions compare with their values,
computed when the plan is made (`o_orderdate < DATE '1993-07-01' +
INTERVAL '3' MONTH` with `1993-10-01 00:00:00`; not volatile calls,
parameters or subqueries): estimated from the histogram, checked against
a segment's least and greatest, and not computed per row.

**Unnesting** (`unnest`): a `WHERE` or scan-filter conjunct `EXISTS (…)`,
`NOT EXISTS (…)` or `x IN (…)` becomes a semi or anti join with the
subquery's rows — one pass over each, hashed on the correlation's
equalities — when the subquery's rows do not depend on the row
(uncorrelated `IN`), or when it reads one table (under projections,
`DISTINCT`, sorts) and refers to the row only in its filter's top-level
conjuncts, which become the join condition.  The right side keeps only
the columns the condition reads.  Once the inner joins are ordered, each
semi and anti join moves down among them, as far as the columns it reads
allow and while the rows it meets there are no more (a subquery filtering
one table filters it before the joins that multiply it, after those that
narrow it; without statistics, as deep as it goes).

**Narrow rows** (`narrow`, last): the columns nothing above a join reads
are cut from its inputs by projections, every reference above moved with
them, so a join copies only what is used.

**The output after the limit** (`postponed`, last; §M14.21 T8b): under
`LIMIT`, a projection below the sort has its expressions the sort does
not order by — not columns or constants — computed above the limit
instead, the sort carrying the columns they read (PostgreSQL's
`make_sort_input_target`, which does it for expensive or volatile ones
only): `SELECT id, ts_headline(…) … ORDER BY ts_rank(…) DESC LIMIT 10`
computes ten headlines, not one a match, and a value of a row the limit
drops is never computed (nor its error raised).  A projection right
above the limit takes the expressions in.

**Index nested loops** (`lookups`): a hash join whose right side is an
analyzed table's scan becomes an index nested loop when an index (the
primary key or a secondary one) leads with the join keys and the lookups
cost less than hashing the table — each lookup counted as 40 rows of a
scan, each row a secondary index finds 60 more (its fetch; measured on
TPC-H, §M14.12 V3), against the table's rows read once; a column table's
lookups count 1.2 times as much and its rows scanned a quarter (its
scans read batches of the columns wanted, its lookups segments' values);
the rows a lookup reads are all of its key value's, whatever the filter
keeps — a semi or anti join's stop at the first match.  An inner join
may look up its left side instead (the columns put back in order above).
A lookup join's rows are estimated as the left side's times a lookup's.

**Adaptive joins** (`adaptive`, last, after narrowing): an inner hash
join whose left side such an index could look up keeps that plan beside
the hash join's (`plan::Adaptive`; `EXPLAIN`: `Hash Join (…) or lookups
up to N rows`), N the right rows below which the lookups cost less.
The join makes its right side first, as a hash join does (on the workers,
gathered, or stashed); with at most N rows they look the left side's
table up, each right row outside (an index nested loop, its rows put
back left columns first), the left side never scanned; with more, the
hash join runs.  A right side estimated larger than it is — TPC-H Q18's
`HAVING sum(l_quantity) > 300`, a third of the orders by estimate, 57 in
fact — then costs what the right estimate would have (Oracle 12c's
adaptive plans, SQL Server 2017's adaptive joins).  The rows are the hash
join's, in the right rows' order; the choice is the same in every
executor (`vector::prepare` makes it for a pipeline, `exec`'s join
otherwise).  Not where the left side reads an enclosing row.

**Parallel aggregates in key order**: grouped by (at least) the leading
column of the primary key a scan reads in order, the morsels' groups meet
only at their edges — the groups of a morsel's first and last prefix —
so each morsel finishes the others itself, on its worker, and hands over
only its edges' partial states, merged in the morsel order (TPC-H Q18's
`GROUP BY l_orderkey`, 1.5 million groups: 600 → 240 ms).  A million
groups do not have to fit in `work_mem` at once (they would, merging into
one table, and the run fell back to one thread partitioning); a group
that fails to finish is the one a single pass finishes first.

**Eager aggregation** (`eager`; Yan & Larson): a grouping by a join's
left key — the key of the left side's single table, its primary key —
whose aggregates read only right columns and cannot fail
(`Aggregate::infallible`: `count`, `min`, `max`, `bool_and`, `bool_or`)
is computed on the right side first, by its join key, and then joined:
`c LEFT JOIN o … GROUP BY c.id` becomes `c LEFT JOIN (SELECT cid,
count(…) FROM o GROUP BY cid)`, a group a customer through the join
rather than a row an order (TPC-H Q13, 367 → 111 ms on column tables).
A left row no right row matches gets what its aggregate gives for the
padded row (`count(x)` 0, `count(*)` 1, `min(x)` `NULL`).  It holds when
the join is inner or left on key equalities only, the right side has at
most half as many groups as rows, and the join keeps at least half of
them (else its key filter narrows the right side more cheaply than a
grouping of all of it); an inner join is tried both ways round, and
through a projection of columns.  Not for a unique index (its `NULL`s
repeat) or an aggregate that can fail (the right rows no left row
matches are aggregated too, and their error would come where the join
gave none).

**`DISTINCT` inside aggregates as groupings** (`distinct`, §M14.20 W6):
`SELECT g, count(DISTINCT x), sum(y) … GROUP BY g` is aggregated twice —
by `g, x`, the other aggregates giving their partial states
(`partial sum(y)` in `EXPLAIN`), then by `g`, the `DISTINCT` one fed
each `x` once and the others combining the partial states (`combined
sum`) — so both are the vectorized, parallel grouping within
`work_mem`, where a `DISTINCT` call kept a set of values a group, a row
at a time on one thread (ClickBench Q4, `count(DISTINCT UserID)`, 558 →
15 ms).  It holds when the `DISTINCT` calls share their arguments, no
call has an `ORDER BY`, every aggregate's result is the same in any
order of its rows (`Aggregate::order_free`: not `array_agg` or
`string_agg`), the others have partial states and the input is a
pipeline (a scan through filters and projections — over a join's rows,
a row at a time, the two groupings would only hash each row twice:
TPC-H Q16 127 → 192 ms); the result is the set's, `NULL`s and errors
included.  `NOT IN` stays a subquery (its NULL
semantics are not an anti join's); so do correlated scalar subqueries.
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

**Index builds** (`dml::IndexBuild`, §M14.20 W2) — a new index's
entries, and every index's when `ALTER … TYPE` or a key change rewrites
a table's rows — are gathered, sorted and written in key order.
`CREATE INDEX` reads only the index's columns, in up to 1024 parts of the
table (`split_points`) scanned a round at a time on the statement's
workers, and computes only that index's keys.  The entries lie in one
buffer — keys and primary keys side by side, no allocation per entry —
with, per entry, its place and its key's first eight bytes as a number,
on which most comparisons end (PostgreSQL's abbreviated keys, DuckDB's
normalized-key prefix).  Up to `work_mem` (shared by the indexes built
at once) they are sorted in memory, in parts on threads of their own and
the parts merged; past it each `work_mem`'s worth becomes one sorted run
of byte records in scratch space (`spill::Run::push_record`), and the
runs are merged by a heap of run positions, the keys left where they
are.  A unique index's duplicates are neighbours in that order — found
without a look at the tree (`23505`); the writes go to the index's
`fresh` range in key order: into the store as they come, without the WAL
(D16, §5.2), `apply_sorted` packing them into full pages — or, with
`--wal full`, through the write set, which keeps them as appends.

**Bulk load** (`copy`, §M14.11).  `COPY t [(columns)] FROM STDIN` loads
rows the client sends, in one statement: PostgreSQL's text format
(fields split by a tab, `\N` for NULL, backslash escapes `\t \n \\
\x41 \101 …`, `\.` ending the data) or CSV (`,`, `"` quoting — newlines
inside quotes are the field's —, an unquoted empty field NULL, `HEADER`
skipping the first line), parsed as the data arrives; or typed rows the
front end decoded (`ncp`'s batches, §10).  Each row is checked and stored
as an `INSERT`'s: defaults for the columns not named, assignment from
text, NOT NULL, lengths, CHECK, unique keys and indexes, row-level
security, the `INSERT` privilege on the columns; foreign keys a chunk of
4096 rows at a time.  A failing row fails the statement — nothing is
stored — with the data's line in the message (`… (COPY data, line 3)`);
`22P04` for a record with too few or too many fields.  It runs in the
session's transaction (or its own) and is never retried, its data having
been read; a load larger than the transaction's memory spills (§5.2), so
its size is not bound by memory.

`COPY` and `INSERT` build their rows through one builder
(`dml::RowBuilder`, §M14.20 W3): the target columns bound once, a text
field parsed straight to its column's type (`Value::from_text` — no
string per field for numbers, dates and timestamps; anything else as a
cast from text), the record's fields in one buffer, and when the fields
come in the table's column order the row built as they are read; then
the defaults, the checks and the policies.  `COPY` collects up to 4 MiB
of records and — when every default, check and policy is safe off the
statement's thread and the transaction can share its reads — parses and
builds them in parts of 2048 on the statement's workers (DuckDB's
parallel CSV reader, Instant Loading), a row table's rows encoded there
too (`TableStorage::encode_new`); the rows are stored in order on the
statement's thread, so a failure names the first failing line as a
serial load would.  Parsing and storing overlap (§M14.20 W3b): a parser
thread parses on the workers, reading through the writer's transaction
behind a lock (`nucleus_kv::Locked`), while the statement's thread stores
what was parsed; the chunks waiting to be parsed are bounded (4), the
parsed batches drained before each read.  `ncp`'s binary batches (§10)
are built into rows on the same parser thread.

Every insert — `INSERT`, `COPY`, an `UPDATE` moving rows between
partitions — stores through one batch path (`dml::insert_batch`): rows
handed over owned, routed to their partitions by bounds, their row ids
reserved per batch (`catalog::next_rowids`), their index entries and
foreign-key keys computed first, then handed to the storage
(`TableStorage::insert_batch`; by default row by row).  A foreign key's
keys are deduplicated by their encoding and checked once each.  The
first error in row order wins, and a statement failing on its own data
skips the foreign-key checks, so a bad record is reported before a
missing parent (PostgreSQL's order).  A column table (§8.15) takes an
ascending batch into its gathered rows at once when nothing stored lies
between its first and last key.  A front end without a client to read from refuses it
(`0A000`); `Session::run_copy(stmt, params, reader)` reads it from any
`Read`.  `COPY … TO` and from a server-side file are not supported (a
query reads; the client sends files).

Sessions: outside `BEGIN` every statement is its own transaction, retried
up to 10 times on `40001` (with the KV contract's `back_off`); inside, the KV transaction starts with the
first statement (so `SET TRANSACTION` may follow `BEGIN`), an error moves
the block to *failed* (`25P02` until `ROLLBACK`; `COMMIT` then rolls
back).  `REPEATABLE READ`, `READ COMMITTED` and `READ UNCOMMITTED` all run
as snapshot isolation; `SERIALIZABLE` as §5.2.  `SET
default_transaction_isolation`; `SHOW transaction_isolation |
server_version | …`.

### 8.8 Statistics and costs (`stats`)

`VACUUM` (PostgreSQL's options and tables accepted; the whole database
either way; not in a transaction block, `25001`) runs the KV's maintenance
(`Kv::vacuum`: versions no snapshot sees removed, committed intents made
versions, §5.2).

`ANALYZE [table]` stores rows, and per column NULLs, distinct values and a
16-bucket equi-depth histogram — from every row of a table up to 30 000
rows, else from a uniform sample of 30 000 (reservoir sampling by Li's
Algorithm L — the gap to the next row taken drawn at once, about
k·(1 + ln(N/k)) draws for N rows rather than one each, from SplitMix64
seeded once from `Kv::random_bytes`: lineitem's `ANALYZE` 11.5 → 5.5 s;
NULLs scaled, distinct values by PostgreSQL's Duj1 estimator); the row
count is exact.  The storage samples (`TableStorage::sample`, offered
rows through `stats::Sample`, which builds a row only when it takes it):
a row table reads every entry but decodes only the rows taken, a column
table decodes a segment's columns only when the sample takes one of its
rows (§M14.20 W3b: `lineitem` 5.5 → 2.7 s in a row table, the column
load's `ANALYZE` 5.7 → 2.9 s).  Selectivities: `=` 1/ndv, ranges from the
histogram (a column's lower and upper bound in one AND together: the
histogram between them, not each side's product), `IS NULL` the null
fraction, `IN` n/ndv, combined by AND/OR/NOT.  What the statistics cannot
estimate — `LIKE`, a distance below a bound, `id % 7 = 0`, any function
of a column — is counted on a sample `ANALYZE` keeps (§M14.12 V3; Oracle's
dynamic sampling, stored rather than read at planning): up to 1,000 of
its sampled rows, at most 1 MiB, under `00 04 <table id> 01`, read once
per planning and evaluated by the predicate itself (matches / rows, at
least half a row); a predicate with a subquery or a volatile call, one
that fails on a sampled row, or a table whose columns changed since
falls back to the defaults (`LIKE` 0.1, `NOT LIKE` 0.9, other
comparisons 0.33, the rest 0.5).  A condition an index method serves is
asked of the method first (`index_method::selectivity`, §8.12): the text
index tells `@@`'s share from its words' document counts (§8.19), where
the sample would analyze each kept row's text at every planning.  For analyzed tables the access path is the cheapest of a
full scan and every index (a secondary index pays a row fetch per row),
`EXPLAIN` shows `rows=`, and chains of up to 8 inner joins are reordered by
dynamic programming (no cross products unless nothing connects; the
smaller input becomes the hash build side; a projection restores the
column order).  A join's equalities multiply their selectivities, except
several between the same two inputs whose columns on one side cover a key
of its table (the primary key, a unique index): those select 1 / its rows
(each row of the other side meets at most one), where the product would
take a join on a two-column key for nearly empty.

Columns are also counted together (§M14.20 V12), per group: the columns
of the primary key and of each index with two or more (an index's first
8), and each `CREATE STATISTICS`.  For every set of two or more of a
group's columns `ANALYZE` stores its distinct combinations (NULL a value;
Duj1 on a sample), and for each two its functional dependency's degree —
of the sampled rows whose value of one repeats, the fraction whose group
agrees on the other (a value seen once tells nothing of a sample; in a
whole table it determines).  Equalities of columns to values in one AND
are then estimated together: one row if they cover a key; else by the
chain rule, from a set counted together (1 / its combinations), each other
column given those before it — `combinations(T) / combinations(T and it)`
for the largest T counted with it, else the strongest dependency on one
of them (`degree + (1 − degree) · its own`, PostgreSQL's), else its own;
of the sets to start from, the likeliest outcome.  A join's equalities
between two inputs whose columns on a side were counted together select
1 / the combinations of the side with more; a `GROUP BY` of columns (and
a `DISTINCT`) makes, per table, its columns' combinations (counted, or
their distinct values multiplied), at most its input's rows.
`nc_catalog.statistics_objects`, `.column_groups` and `.dependencies`
show them.  Without statistics the rule-based plan is kept.

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
| `nc_catalog.tables` | `table_id`, `name`, `columns`, `primary_key` (names), `has_row_id`, `indexes`, `checks`, `foreign_keys`, `referenced_by`, `key_prefix` (hex), `analyzed_rows`, `owner`, `row_security`, `force_row_security`, `comment`, `storage` (§8.15) |
| `nc_catalog.storages` | `name` (the table storages registered, §8.15) |
| `nc_catalog.text_analyzers` | `name` (the text analyzers registered, §8.19) |
| `nc_catalog.partitions` | `table_name`, `partition`, `strategy` (`range`, `list`, `hash`), `key` (names), `bound` (`FOR VALUES …` or `DEFAULT`), `storage` (§8.16) |
| `nc_catalog.segments` | `table_name`, `segment` (id, hex), `rows`, `deleted`, `bytes` (encoded) — each segment of each `column` table (§8.15) |
| `nc_catalog.columns` | `table_id`, `table_name`, `position`, `name`, `type`, `nullable`, `default_expr`, `in_primary_key`, `comment`, `generated` (`stored`, `embedding`, or NULL), `generation_expr`, `model`, `model_version`, `next_model` (§8.24) |
| `nc_catalog.indexes` | `table_id`, `table_name`, `index_id` (0 = primary), `name`, `columns`, `is_unique`, `is_primary`, `key_prefix`, `orderings` (per column, the taught ordering it is indexed in, or NULL), `method` (`btree` or a registered method's, §8.17), `opclasses` (per column, its operator class, or NULL), `options` (`name=value`) |
| `nc_catalog.constraints` | `table_id`, `table_name`, `name`, `kind`, `definition` |
| `nc_catalog.foreign_keys` | `table_id`, `table_name`, `name`, `columns`, `referenced_table`, `referenced_columns`, `on_delete`, `on_update` |
| `nc_catalog.statistics` | `table_id`, `table_name`, `column_name`, `rows`, `nulls`, `distinct_values`, `histogram` |
| `nc_catalog.statistics_objects` | `table_id`, `table_name`, `name`, `columns` — each `CREATE STATISTICS` |
| `nc_catalog.column_groups` | `table_id`, `table_name`, `columns`, `distinct_values` — each set of columns `ANALYZE` counted together |
| `nc_catalog.dependencies` | `table_id`, `table_name`, `from_column`, `to_column`, `degree` (0–1) |
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
| `nc_catalog.statements` | `user_name`, `query` (constants as `$n`), `calls`, `errors`, `total_ms`, `mean_ms`, `min_ms`, `max_ms`, `rows_read`, `rows_returned`, `model_calls`, `model_texts`, `model_cached`, `model_tokens`, `model_errors`, `model_ms`, `last_call` (statement statistics, §8.13; an administrator sees all, others their own; the model columns §8.27) |
| `nc_catalog.index_advice` | `table_name`, `columns`, `create_statement`, `statements`, `calls`, `rows_read`, `rows_read_with_index`, `example_query`, `plan` (the index advisor, §8.13) |
| `nc_catalog.sessions` | `id`, `user_name`, `session_user_name`, `client_addr`, `front_end`, `auth_method`, `tls`, `access_rule`, `read_only`, `as_role`, `roles` (active at login), `started` (the sessions a front end admitted; an administrator sees all, others their own) |
| `nc_catalog.policies` | `table_name`, `name`, `permissive`, `command`, `roles`, `using`, `with_check` |
| `nc_catalog.tokens` | `name`, `login`, `role`, `read_only`, `tables`, `valid_until`, `created`, `created_by`, `max_rows`, `max_scan`, `timeout`, `rate_per_minute` (the tokens the current role may manage; never a secret) |
| `nc_catalog.models` | `name`, `owner`, `provider`, `dimensions`, `max_batch`, `secret`, `query_prefix`, `document_prefix`, `options`, `files`, `bytes`, `created`, `calls`, `texts`, `cached`, `errors`, `total_ms`, `tokens`, `throttled_ms`, `batch_tokens`, `requests_per_minute`, `tokens_per_minute`, `parallel` (the models the current role may call; the calls since the process started, §8.23, §8.27) |
| `nc_catalog.model_providers` | `name`, `remote` (whether its calls reach the network) |
| `nc_catalog.chunk_tables` | `table_name`, `source`, `column_name`, `by`, `size`, `overlap`, `model` (the chunk tables and what they chunk, §8.26) |
| `nc_catalog.embedding_jobs` | `table_name`, `column_name`, `model`, `kind` (`current`, `next`), `key`, `state` (`pending`, `retrying`, `failed`), `attempts`, `enqueued`, `next_attempt`, `error` (the embedding columns' queue, §8.24; the tables the current role may read) |
| `nc_catalog.secrets` | `name`, `owner`, `source` (`value`, `env`), `env`, `created`, `changed` (the secrets the current role may use; never a value) |
| `nc_catalog.grants` | `kind`, `object`, `column`, `grantee`, `privilege`, `grantable` (the ACLs written out) |
| `nc_catalog.types` | `name`, `aliases`, `category`, `module` (which registered it), `equality`, `hashing`, `ordering`, `key_encoding`, `distance` (its operator), `encoding_version` |

**PostgreSQL compatibility: the `pg-compat` module** (`modules/pg-compat`,
`nucleus_pg_compat::PgCompat`, §8.12).  Only sessions whose registry has it see any of what follows —
the PostgreSQL front end's (§9); `ncp` sessions and the embedded
`Session::new` do not (principle 11).  PostgreSQL clients hard-code
PostgreSQL's catalog names, so the same catalog is also shown as
`pg_catalog` (`pg_namespace`, `pg_class`, `pg_attribute`, `pg_attrdef`,
`pg_type`, `pg_index`, `pg_constraint`, `pg_am`, `pg_collation`,
`pg_roles`, `pg_database`, `pg_tablespace`, `pg_tables`, `pg_indexes`,
`pg_statistic_ext` (each `CREATE STATISTICS`, its columns by
`pg_get_statisticsobjdef_columns`), and empty `pg_policy`, `pg_trigger`,
`pg_inherits`, `pg_publication*`, `pg_description`, `pg_auth_members`) and
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
that need functions, sequences or permissions do not.  `pg_typeof(x)` and
`to_regtype(name)` (a type's oid by its `pg_type` or SQL name, NULL when
there is none) are among its functions too.  PostgreSQL's text search —
`tsvector`, `tsquery`, `to_tsvector`, `to_tsquery`, `@@` — is the
module's too, served by the text index where it can be (§8.22).

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
  an ordering), `parallel_safe()` (default: unless volatile; a volatile
  function whose calls do not depend on one another, such as `random()`,
  returns true so workers call it), `found(args)` (the condition whose
  rows a ranking function ranks: `text_score`'s are `@@`'s; §8.20),
  `rewrite(args)` (a call giving the same value on every row, bound in
  this one's place — pg-compat's `to_tsvector(cfg, col) @@
  to_tsquery(cfg, q)` as the text index's `col @@ q'`, §8.22),
  `of_condition()` and
  `bind_condition(operands, rest)` (a function of an operator's
  condition, `highlight(body @@ q)`: the operator, and the call bound in
  its place from the operator's operands — typed as its signature
  converted them — and the other arguments), `implied_range()` (for a function of two arguments true
  only when the first lies in a range computed from the second: two
  functions giving the bounds; the binder adds the comparisons beside the
  call, for index ranges — `inet <<= cidr` uses it), `call(&CallContext,
  &[Value])`, and optionally `call_batch(&CallContext, &[&Vector], len,
  &Sel) -> Option<Result<Vector>>`: a batch kernel for the vectorized
  executor (§8.5), the values for the rows in play — `NULL`s and errors
  as `call` gives them; `None` (the default) for arguments it has no
  kernel for, and `call` is made a row at a time.
  `CallContext` gives the statement's transaction, the registry,
  `now()`, `clock()` and `random(buf)` (all through the database's
  platform).
- `Aggregate` — `name()`, `arity()`, `accepts_star()` (`count(*)`),
  `keeps_nulls()` (default false: rows whose first argument is `NULL` are
  skipped), `resolve` (as a function's), `start() -> Box<dyn
  Accumulator>` (`add(&[Value])`, `finish()`), one per group.  The engine
  does the grouping, `DISTINCT` (over the arguments) and `ORDER BY` inside
  the call.  Optionally `grouped() -> Option<Box<dyn GroupAccumulator>>`
  (`resize(groups)`, `update(rows, groups, &[&Vector])`, `finish(group)`,
  and `group_bytes()`, about the bytes a group's state takes — by
  default 96 —, what a grouping counts against `work_mem`):
  the states of many groups fed a batch at a time by the vectorized
  executor (§8.5) — a capability; without it the accumulators are fed row
  by row, with the same result.  With `combinable()`, `partial(group)` and
  `combine(group, partial)` too, its groups are aggregated in parallel:
  each worker's state as a value, combined in key order (`resize` may be
  asked for more groups than are used: room for them); a window frame
  over any rows of its partition then combines a segment tree's partial
  states (§8.5, *Window functions*).  `finish_keeps_state()` (default
  false): `finish(group)` leaves the state as it was, so a window frame
  growing from its partition's start reads each row's result from one
  state.  `infallible()`
  (default false): no input makes it fail, so the optimizer may compute
  it below a join, over rows the join would drop (eager aggregation,
  §8.6).  `order_free()` (default false): its result is the same in any
  order of its rows, so a `DISTINCT` call of it may be fed its values
  grouped first (§8.6).
- `TableFunction` — `name()`, `arity()`, `resolve(&[ArgInfo]) ->
  TableSignature` (per argument, and the output columns: one is named
  after the function or its alias, several by their own names, qualified
  by the alias), `rows(&CallContext, &[Value])`.  `ext::common_type` is the binder's rule for `Common`
  arguments, for a `resolve` that needs it.
- `CatalogView` — `schema()`, `name()`, `columns()`, `rows(&CallContext)`.
- `Type` — `name()`, `aliases()`, `category()`, `input(text)`,
  `output(&Value)`, and its abilities, each its own interface it has or
  not: `equality()` (`Equality`), `hashing()` (`Hashing`), `ordering()`
  (`TotalOrder`), `key_encoding()` (`KeyEncoding`: order-preserving,
  self-delimiting), `distance()` (`Distance`: an operator and a
  distance), `arithmetic()` (`Arithmetic`: the binary operators it
  defines — `+`, `-`, `*`, `||` … — and `apply(op, a, b)`; the binder
  calls it through `ArithmeticFn`), `elements()` (`Elements`:
  `element_type()`, `count`, `get`, `slice`, `with` — 0-based and in
  range, the binder and evaluator doing SQL's 1-based positions and
  clamping — for `x[i]`, `x[lo:hi]` and `SET x[i] = v`; a type without
  it cannot be subscripted, `42804`); a modifier — `modifier(n)` checks
  `type(n)` when a column or cast names it (by default: the type takes
  none, `42601`), `fit(v, n)` checks or fits a value to it on every write
  and on a cast (`FitFn`); casts — `cast_from(v)` from another type,
  `cast_to(v, t)` to one (`None`: not this type's cast, the built-in
  rules apply); `aggregate(name)` (its own `sum`, `avg`, … — chosen by
  the first argument's type before the built-in one); `stored_with_row()`
  (a value read with its row nearly always — `vector`, `sparsevec` —
  stays in the row however long instead of out of line, §8.1's large
  values; by default false); `encoding_version()`; `examples()` (on which registration
  checks the laws: a type that breaks one is not registered).  A
  registered type's values are
  `Value::Ext(TypeRef, bytes)` — `Value::ext(t, bytes)`, `v.ext_bytes()`
  — whose bytes are canonical: equality and hashing are on them
  (`CanonicalBytes` implements both), so they agree by construction; its
  type is `DataType::Ext(TypeRef)`.  `TypeRef` is the type interned once
  per name (small, `Copy`): `name()`, `id()`, `imp()`,
  `TypeRef::named(name)`, `TypeRef::by_id(id)` (each thread keeps the
  real type it last looked up either way, so decoding a column of a
  registered type takes the interning table's lock once, not a value —
  §M14.12 V2, where every worker of a parallel scan waited on it).  The catalog stores a registered type by name;
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
  `add_view(v)`, `add_type(t)`, `add_storage(s)` (a table storage, §8.15),
  `add_index_method(m)` (an index access method, §8.17),
  `add_alias(name, ty)` (pg-compat's
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
arrays, `uuid`, `vector` and `sparsevec`).  `crates/sql/tests/laws.rs` runs the kit on every
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

### 8.13 The SQL analyzer (`profile`, `statements`, `lint`, `advisor`)

**`EXPLAIN ANALYZE <query>`** (also `EXPLAIN (ANALYZE [bool], TIMING
[bool]) …`; other options are refused with `0A000`) runs the query and
prints the plan with, on every node, the optimizer's estimate beside what
the node did: `(rows=40) (actual rows=38 loops=1 time=0.120 ms)` — rows
over all its starts, the starts (a nested loop's inner side, a correlated
subquery: once per outer row), and the time spent in it, its inputs
included; a scan a hash join's key filter narrowed adds `(join key
filter)` (§8.5).  A node the run never reached says `(never executed)`; a
query that called embedding models has a line `Models: n calls, n texts
sent, n cached, n tokens, n errors, t ms` (§8.27); the last
line is `Execution: n rows in t ms`.  `TIMING OFF` leaves the times out
(repeatable output, for tests).  Time is the database's monotonic clock
(`Kv::elapsed`).  The query runs with the session's privileges, budget and
row-level security, like the query itself; only queries for now (`UPDATE`
and `DELETE` have plain `EXPLAIN`); it reads the columns the query
reads, as it would run.  A large gap between `rows=` and
`actual rows=` points at stale or missing statistics (`ANALYZE`).

**Statement statistics** (`nc_catalog.statements`): every statement a
front end (or `Session::execute`) gave the text of, per role
(`current_user`) and normalized text — constants replaced by `$n`
(numbered after the statement's own parameters), white space as one
space; case and spacing do not split a statement, and the text shown is
the first run's.  Per statement: calls, failures, total, mean, shortest
and longest time (ms), rows read (scanned) and rows returned or changed,
its calls to embedding models (calls, texts sent, cache hits, tokens,
errors, time: §8.27), the last call.  An administrator sees every role's, anyone else their
own; `nc_statements_reset()` forgets every role's (an administrator) or
the caller's own.  They live in the process, by database (a restart
starts over), at most 5000 statements (the least called makes room);
statements of the database's own probe sessions are not counted.

**The linter: `ANALYZE QUERY <statement>`** (a query, `UPDATE` or
`DELETE`; `ANALYZE query` alone still analyzes a table named `query`)
plans the statement without running it and returns one row per finding:
`rule`, `place` (the table and the condition, with column names; the
join; the subquery), `finding` and `fix`.  Rules, over the optimized plan
and its subqueries:

| Rule | Fires on | Fix |
|------|----------|-----|
| `full_scan` | a table read in full for a filter that keeps a tenth of it or less (by the statistics, else the default selectivities) and that an index could serve; not for an analyzed table under 1000 rows | `CREATE INDEX ON t (…)`: the equality columns, then one range column |
| `index_defeated` | a comparison with a value where the first column of an index (or the primary key) is under a function, arithmetic or a cast (the binder's too), and the scan does not use that index | compare the column itself; move the arithmetic; cast the other side |
| `foreign_key_without_index` | a foreign key whose columns lead no index, of a table the statement reads — or, for `UPDATE` / `DELETE`, of a table referencing the one changed | `CREATE INDEX ON t (…)` |
| `cartesian_product` | a join with no condition (or `ON true`) between inputs estimated at more than one row each | a join condition (ignore when every pair is meant) |
| `not_in_nullable` | `NOT IN (subquery)` whose column can be NULL (nullability followed through scans, filters that reject NULLs, projections, joins, aggregates) | `NOT EXISTS`, or `IS NOT NULL` in the subquery |

`SELECT *` in views comes with views.

**The index advisor** (`nc_catalog.index_advice`): from the statement
statistics (the caller's own, or every role's for an administrator) and
`ANALYZE` data, the indexes that would have paid off.  Each recorded
query, `UPDATE` and `DELETE` is planned again from its last run's text;
its full scans of analyzed tables of 1000 rows or more give candidates
(the filter's equality columns, then one range column).  Each candidate
is put on its table as a hypothetical index — the catalog reads a
definition with it in place of the stored one
(`catalog::with_hypothetical`), so the binder and optimizer plan as if it
existed — and every statement is planned again: where the optimizer
picks it and the estimated rows read fall by a tenth or more, it serves
that statement.  Each statement counts, by its calls, for the candidate
that serves it best (a tie goes to the one of more columns), so one index
on `(region, score)` stands for statements on `region` alone too.
Columns: `table_name`, `columns`, `create_statement` (`CREATE INDEX ON
t (…)`), `statements`, `calls`, `rows_read` and `rows_read_with_index`
(estimated, over the calls), `example_query` (the one it helps most,
normalized) and `plan` (that statement's plan with the index, with its
constants).  Most rows saved first.  Joins do not give candidates yet
(the executor has no index nested loop).

### 8.15 Table storage (`storage`, §M14.20)

How a table's rows are kept is a module (principle 12): an implementation
of `TableStorage` among those registered — `row`, `column` and `row,column` built in, others added
through `Registry::add_storage` (process-wide, so every database's tables
may name them; `nc_catalog.storages` lists them).  A table names its
storage: `CREATE TABLE … WITH (storage = name)` (`row` by default; an
unknown storage is `42704`, an unknown option `22023`), kept in its
record (table record version 10) and shown in `nc_catalog.tables.storage`.

The SQL layer reads and writes rows only through the trait, by *row key* —
the table's primary index prefix and the row's primary key or hidden row
id — and by ranges of row keys: `write(txn, t, key, row, old)`,
`remove`, `exists(…, validated)` (primary key and foreign key checks),
`get(…, wanted, labels)` (a secondary index's rows, which the index finds
by row key), `rows(Scan)` (a range ascending, only the wanted columns
decoded), `batches(Scan)` (the same a batch at a time for the vectorized
executor — by default from `rows`), `check` (the integrity check's part:
rows as stored, problems noted), `index_ids` (the key spaces it uses
under the table's data prefix).  Secondary indexes, constraints,
foreign keys, policies, parallel splits and the executor stay above it,
the same for every storage; a storage keeps all it stores under the
table's data prefix, so dropping or truncating a table removes it.

`row` encodes a row's values together under its key (§8.1), a long value
in chunks under the table's index `u32::MAX` (§M14.26); its batches decode
the encoding straight into vectors (§8.5).

`column` (`column_store`, `segment`) keeps rows as column segments: up to
32 768 rows with consecutive keys (fewer when wide), one KV value per
column and one for the row keys, segments covering disjoint key ranges in
key order, listed in a directory keyed by each one's last key (index
`u32::MAX - 3`; with its first key, rows, and each column's least and
greatest values and `NULL`s).  Columns are encoded by what they hold:
integers, dates, times and timestamps by frame of reference and bit
packing, or runs; doubles as they are, booleans a bit each; text (and
`char(n)`, as kinds of its own) as a dictionary and bit-packed codes when
values repeat, else Arrow's offsets and bytes; decimals of one scale whose unscaled values fit 64 bits as
those integers, by frame of reference and bit packing; anything else value
by value; `NULL`s a bitmap (a segment
lists the table columns it holds: a partial column copy holds some).  A
segment's id is a hash of its contents, so it never changes and its decoded columns
are cached by id (in 16 shards by key, a hit locking its shard only to
read; the size bound, 256 MB, is all shards' together, and past it the
columns used longest ago in any shard go, an eighth of the bound at a
time) — its directory entry decoded too, once (a lookup's), and its row
keys with every 32nd key's first 8 bytes as a number beside them (a
search narrows to 32 keys in that small array first, ClickHouse's marks).
A read of one row (by its key: an index lookup, a
correlated subquery's) takes each column's value from the column decoded
if it is cached, else from its encoding as stored — cached instead of the
column decoded, smaller — read where the value lies (`segment::value_at`:
every layout but value by value's).  A scan reads only the wanted columns of the segments
its range overlaps, skips a segment the filters right above it rule out
(column-constant comparisons against the segment's least and greatest
values), and hands each run of a segment's rows to the vectorized executor
as vectors.  A dictionary column is decoded — and cached — as it is
stored, its entries and each row's code (`Vector::Dict`, a fraction of
its text's memory); a scan gives it as text, or as codes to a filter right
above that asked for them (`Scan::dicts`): the filter's leading conjuncts
that test the column against constants by what cannot fail (comparisons,
`IN`, `LIKE`, `IS NULL`, `NOT`, `AND`, `OR`) are tested on the entries,
once a segment, and a row whose entry one holds FALSE for is dropped
before the filter runs — the rows an `AND` would drop reading no further,
so no later conjunct's error is skipped.  The rows left are made text
(gathered into a batch of their own when at most half are left), and
when every such conjunct held TRUE for each, only the filter's other
conjuncts are tested (lineitem's 6 million rows: `l_shipmode = 'MAIL'`
16 → 12 ms, `IN` 20 → 14 ms, `LIKE '%PERSON'` 29 → 8 ms).  *Writes:* rows a statement inserts — and the new versions of
segment rows it updates — are gathered until it ends (4 segments' worth
at most, then written at once); then, 4096 or more go to segments
directly, fewer to a *delta store* in the row storage's format (index
`u32::MAX - 4`, the rows' key suffixes).  An update or delete of a
segment row marks it deleted: a marker keyed by the row (index `u32::MAX -
2`), not by its place in a segment, so it stays right when a merge
rewrites the segment — deletes do not conflict with merges or with each
other.  A merge rewrites the segments the incoming rows' key range overlaps —
deleted rows dropped, delta rows moved — so appends make new segments
and updates rewrite the segments they touch.  Scans merge the segments'
rows with the delta store's in key order; a key lookup checks the delta
store, then the segment whose range holds the key.  MVCC and crash safety
are the KV's: segments, markers and delta rows are versioned keys written
in their transactions; a delete and a merge of one segment, in either
order, both commit and the delete holds (`crates/sql/tests/column.rs`).
*Concurrent writes* conflict as a row table's do: the directory is keyed
by a segment's last key, so two segments over the same keys need not
share a key to conflict on — instead a merge reads the directory
*validated* from its first incoming key through the first entry past its
last (to the directory's end without one) and the delta store's keys in
that range, and a statement that stores rows in the delta store reads the
directory the same way for its keys.  A segment another transaction
commits over those keys meanwhile, or a delta row among them, fails the
later commit with a retryable `40001`, and no committed row is lost;
small writes to different keys never conflict.  A write past the last
segment claims the directory to its end, so two writers appending there
at once may conflict even with different keys (one retries); tested for
every pair of large and small writes, overlapping or not, in either order
(`crates/sql/tests/column.rs`).  A statement that fails part way leaves
nothing gathered: a table is registered for the statement's clean-up
when its first row is gathered, so the rows of a statement that fails —
say on a unique index while updating — are dropped with it, never
stored by the next statement on the thread.  What a statement keeps per
table it writes — the rows written and gathered — is one statement
context (`storage::TableWrites`, behind `with_writes` / `peek_writes` /
`take_writes`; §M14.20 W3), cleared when the statement ends, whatever
its outcome.  An existence check (a key's uniqueness on insert) looks at
the gathered rows — past the last one, a load's every key, one
comparison — then at the directory's first segment ending at or after
the key and, when none does, at the delta store's first row at or after
it: both `first` reads, which the transaction answers from memory inside
a range it found empty (§6.1), so a load's ascending keys read the store
once each time rows are stored, not once a row.  A batch (§8.7) that is
strictly ascending and meets no stored row — none gathered between its
first and last key, no segment there, no delta row — is gathered whole
without a check per row; any other batch goes row by row.  A segment is
built from typed vectors (§M14.20 W3b): the rows moved, not copied, into
columns transposed in parts on threads, each column joined and encoded on
threads of its own (`Vector::typed`, the one row-to-column builder;
integer least and greatest values from the kernels; a text column's
dictionary chosen by a distinct count that stops past a quarter of the
rows).  Values read from a segment are stored values; an enum's labels
come from the reading transaction's catalog (`Labels::fill`), so a
renamed label reads renamed.  The same SQL runs on both storages with
the same results: the tests run loads, updates, key changes and deletes on
a `row` and a `column` table and compare every query, a row and a batch
at a time and in parallel, and the integrity check.  On a million rows
(`docs/benchmarks.md`): a `sum` 22 → 2 ms (one thread 81 → 13 ms), a
filtered count 23 → 2.4 ms, a 1000-group `GROUP BY` 35 → 19 ms, the file
of its columns 32 MB against 61; key lookups 0.1 → 0.3–1.7 ms.  The delta
store's deleted rows stay as versions until `vacuum`.

`row,column` keeps both copies (HTAP): the rows whole in the row storage —
which key lookups, row scans and constraint checks read — and a column
copy of every column or of some, which scans whose columns it holds read.
Writes go to the row copy and note their keys among the *changed* (index
`u32::MAX - 5`; a segment row they replace marked), and the column copy
reads a changed key's row from the row copy; merges take changed keys
into segments as the `column` storage's merges its delta store.

**Changing a table's storage** (`convert`): `ALTER TABLE t SET STORAGE
row | column | row, column | column (a, b, …)` (the last a column copy of
those columns beside the rows) converts the table online — in transactions
of its own, a part at a time, while other sessions read and write it; not
inside a transaction block (`25001`).  Building a column copy, the table is
`row,column` with the copy being built (writes note their keys) and
segments are written from the rows a segment at a time; building a row
copy, the table is `row,column` with reads from the column copy and the
segments' rows not written since copied a thousand at a time; dropping a
copy, the table switches at once and the copy's keys go afterwards, a
part at a time.  Between `column` and `row,column` the delta moves over a
part at a time after the switch (the table reads both meanwhile; writes go
to the new one), so no step has to catch up with writes.  Each switch is a
small transaction that rewrites the table's record, and every write reads
the record validated, so a write begun under the old state fails (`40001`)
and runs again; a conversion cut short leaves a state that reads and
writes correctly, and running it again finishes it (the table's record
keeps the state, `nc_catalog.tables` its storage).
`crates/sql/tests/convert.rs` converts through every pair with writes
between, converts while another thread writes (each write retried on
`40001`), and crashes conversions at I/O points, comparing a row table's
results and running the integrity check each time.  On a million rows, a
row table gets its column copy in 0.7 s.  `crates/sql/tests/storage.rs`
registers a module's storage and runs writes, unique and foreign key
checks, index lookups, scans a row and a batch at a time, in parallel,
and the integrity check through it, against `row` tables.

### 8.16 Partitioning (`partition`, §M14.20)

`CREATE TABLE t (…) PARTITION BY RANGE | LIST | HASH (columns)` makes a
partitioned table: it holds no rows; its partitions do —
`CREATE TABLE p PARTITION OF t FOR VALUES FROM (…) TO (…)` (the lower
bound included, the upper excluded; `MINVALUE` / `MAXVALUE` open; several
key columns compare in order), `FOR VALUES IN (…)` (list: one column),
`FOR VALUES WITH (MODULUS m, REMAINDER r)` (hash: `partition_hash(key,
m)` = `r`, a hash of the key's encoding, the same in every build), or
`DEFAULT` (range or list: the keys no other partition takes).  A partition
gets the table's definition — columns, keys, checks, indexes, foreign keys
— and its own storage (`WITH (storage = …)`, §8.15); overlapping bounds
are refused, and so is a new bound a default partition already holds rows
of.  A partitioned table's primary key and unique keys must include its
partition key (so each lives in one partition); `CREATE INDEX` on it makes
the index on every partition (`name_partition`); `ADD COLUMN` adds the
column to every partition; dropping it drops its partitions.  Other schema
changes of a partitioned table or a partition, and foreign keys
referencing a partitioned table, are refused for now (`0A000`).

`ALTER TABLE t ATTACH PARTITION p FOR VALUES …` makes an existing table a
partition: it must have the table's columns (names, types, nullability)
and primary key (`22023` otherwise); its rows must fall in the bound
(`23514`) and pass the table's checks; its own indexes and foreign keys
give way to the table's, built over its rows.  `ALTER TABLE t DETACH
PARTITION p` makes a partition a table of its own again (`42809` when it
is not one of `t`'s); `ALTER TABLE t DROP PARTITION p` detaches and drops
it.

A table partitioned by range of a date or timestamp may have a **storage
policy**: `ALTER TABLE t SET STORAGE POLICY (column AFTER interval '30
days')` (any storage `SET STORAGE` names, without columns; `NONE` takes
it away; not in a transaction block, `25001`).  A partition whose range
ends at least the interval before now is converted online to that
storage (§8.15) — when the policy is set, and every minute by the
server's maintenance thread (`nucleus_server::maintain`;
`nucleus_sql::partition::apply_policies` for embedders) — so recent data
stays in rows, for writes, and old data goes to columns, for scans.

A query of the partitioned table is an `Append` of its partitions' scans
(under the table's policies), each partition read when reached.  The
optimizer pushes the predicates into each partition and, from those
comparing the key's first column (range) or the key (list, hash `=`) with
a value of no row, derives what a partition must satisfy to hold a match:
evaluated now when it is a constant (a partition ruled out is not in the
plan), else kept as a guard evaluated when the plan runs — parameters,
enclosing rows (`EXPLAIN` shows `Append (pruned when run: …)`).  A row
inserted into it goes to the partition its key falls in (`23514` when
none does); one written to a partition directly must fall in its bound
(`23514`).  `UPDATE` and `DELETE` of it change its partitions' rows — all
chosen before any is written — and an update that changes a row's key to
another partition's moves the row (deleted there, inserted in the other).
`crates/sql/tests/partition.rs` runs random inserts, key-moving updates
and deletes on a partitioned table (of `row`, `column` and `row,column`
partitions) and a plain one, compares queries and parameter-pruned ones,
and runs the integrity check, and applies a storage policy at chosen
times; `42_partitions.slt` covers the statements.

### 8.17 Index access methods (`index_method`, §M14.12 V4b)

An index is the built-in B-tree (an entry a row: the indexed values, then
the primary key, holding the primary key) or an `IndexMethod` registered
through `Registry::add_index_method` (process-wide, as storages):
`CREATE INDEX [name] ON t [USING method] (col [opclass], …) [WITH (name =
value, …)]` names it (`USING btree`, or none, the B-tree; an unknown
method `42704`; the B-tree takes no operator class, `42704`, and no
option, `22023`; a method's index is never `UNIQUE` nor in a taught
ordering, `0A000`).  The index record (table record version 14) keeps
the method, the operator classes and the options as the method's
`define` left them — it checks the columns' types, refuses what it does
not know and fills in defaults — and `ALTER … TYPE` of an indexed column
asks it again.

A method keeps everything under its index's prefix, so `DROP INDEX`,
`DROP TABLE` and deleting the table's data need nothing of it.
`dml::IndexKeys` tells it every change in the writing statement's
transaction (`write(txn, t, i, old, new)`: the row as it was and as it
is, primary key bytes and values; `None` for an insert's old and a
delete's new) — after the storage took the row, so a refused row never
reaches it; a rollback undoes its writes with the rest.  `build` makes
it over the rows a table has (`CREATE INDEX`, and after a rewrite of the
table's rows: `ALTER … TYPE`, a key encoding upgrade; `index_method::rows`
reads them), and `check` checks it for the integrity check against the
table's rows.  The optimizer's key ranges take only plain B-trees
(`Index::ordered`); a method's own search is an access path of its own
(`Access::Method`: the condition it serves, the order it ranks by, or
both).

Two hooks let a method serve a filter's condition and decide what an
operator means on its column (§M14.21 T3, the text index's, §8.19).
`operand(ix, op)`: a value the binder appends to operator `op`'s call
when its left side is a table column the index is on (`ScopeCol::origin`,
`Binder::index_operand`) — the text index's analyzer for `@@`, so the
operator means the same through the index and on the rows; the first
index that gives one wins.  `serves(ix, function)` and
`rows_where(txn, t, i, function, args)`: a conjunct of a scan's filter
`f(col, args…)` with `col` the index's first column and the arguments
fixed for the scan (`optimizer::fixed`) — and the operand the index
gives, so a statement bound before the index existed is not served —
becomes `Access::Method` with `served: Served { function, args, cond }`
(`optimizer::method_filters`: a full scan, or a key range estimated past
`METHOD_OVER_RANGE` rows, 1,000), the conjunct out of the filter —
under row-level security a statement's conjunct above the barrier too,
as it may narrow a key range, staying there (`Served::above`: where the
index cannot tell the rows they are read without it, so it is never
evaluated below the policy);
`exec::method_rows` asks for the primary keys and reads their rows (none
when the statement reads no column and nothing else is checked: the
method is exact) — in key order, a block at a time (8, then twice the
last up to 256, four times past it, up to 16,384: a limit above stops it
early), by a merge with the table's scan (`exec::rows_at`,
`TableStorage::rows_at`; the row storage's `exec::entries_at`, PLAN.md
§M14.21 T5b): where the keys lie far apart each is looked up, where they
lie close the scan reads on through the entries between, passing the
unwanted by their keys — chosen by the share wanted of the entries
passed, measured as it goes (a probe of 16 entries where it is not yet
known and every 1,024th lookup, each chunk read on) and kept across the
statement's blocks, reading on above one in `SEEK_ENTRIES` (16, what a
lookup costs in entries read); past 256 keys a run each on the
statement's workers over the transaction's shared reads.  Where
`rows_where` says `None`
every row is read with `cond` checked.  `EXPLAIN`: `Index Scan using ix on t (col f arg)`.
`selectivity(txn, t, i, function, args)`: the share of the table's rows
such a conjunct holds for, where the index tells it at less cost than
the statistics' kept sample (§8.8), which it is asked before; by default
`None`.  The text index's `@@`: each word's documents over the index's,
the words as independent — all of them the product, any of them one
less the product of their misses, a phrase as all of its words, an
excluded one its miss (`query::share`, PostgreSQL's `tsquery_opr_selec`
over exact counts where PostgreSQL has a sample's most common words); a
prefix or a fuzzy word leaves it to the sample.

Two more let a method compute a function of such a condition and rank
the rows it finds (§M14.21 T5, BM25's).  `function(t, i, name, arg)`:
`name(arg)` with `arg` an operator's call whose left side is a column
the index is on (`Binder::operator_origin`, `Binder::index_function`)
bound as the method says — an expression and its type in place of the
call (`score(body @@ q)`: §8.19); none, and the function is bound as
any other.

One more ranks a table's rows for `ORDER BY f(col, args…) LIMIT k
[OFFSET o]` — HNSW's nearest rows (§8.18) and the text index's first by
BM25 (§8.19) through one hook, one access path and one optimizer pass
(PLAN.md §M14.21 T5a).  `ranks(ix, function, desc, nulls_first)`: the
orders it ranks by; `rank_cost(ix, RankCost { function, served, rows,
k, share })`: what ranking costs in rows read (HNSW's `ef · (3 + 2m /
10)`; the text index's `k` over the share passing, never more than the
rows its condition finds), `None` where it cannot; `rows(txn, t, i)`:
the rows it holds, for a table not analyzed.  The optimizer's `ranked`
(its last pass) finds `f` the first sort key of a `Limit` over a `Sort`
(through a projection), the column `f`'s first argument (or either of a
distance's two) and the rest fixed; under the sort, a scan — of each
partition under an `Append`, each choosing for itself — under row-level
security perhaps, whose policy and statement filter above its barrier
become the rank's `checks`, in that order.  The index is the scan's own
where its condition is served by one that ranks (the text index), else
one on the column that ranks by `f`, the one with the most of its other
columns held equal by the conditions (a partitioned HNSW's `partition`).
A scan through a B-tree's range of the filter or another method's
condition keeps it as the rank's `within` and always ranks: its rows are
ranked among, or checked where they turn out many.  Otherwise it ranks
where `rank_cost` is at most the rows the scan reads (the table's, the
range's or the condition's estimate), with `share` the filter's and the
checks' selectivity.  The scan becomes `Access::Method` with `rank:
MethodRank { function, args, desc, count, within, share, checks,
partition, value }` (`count` the limit plus the offset); the sort and
the limit stay above.  Where the method gives each row's value of `f`
with it — `values(ix, f)`, the very number `f` computes of the row (the
text index's BM25; by default none) — and a projection under the sort
computes `f` (of one table's scan, not each partition's), `value` is
the call: the scan's rows carry its value one more column past the
table's (`Plan::width`), and the projection's call, wherever it is in
its expressions, reads that column instead (PLAN.md §M14.21 T5c).
Where the index cannot rank after all, the scan computes the call on
each row it reads (`prune` reads the columns it takes).

`exec::method_rows` evaluates them and calls `rank(txn, t, i, RankAsk {
rank, served, share, part, among, values }, wanted, keep)`: `keep` the join's
key filters, then `within`'s condition (unless the row came from
`among`), the filter and the checks — `None` where nothing checks; and
`among` (`exec::among`) a callback the method calls with the most rows
it takes, which reads the B-tree range's primary keys or asks the other
method's `rows_where`, `None` past that many.  The method gives at least
`k` rows that pass, in any order — an exact one every row tied with the
`k`-th passing — or `None` where it cannot tell them; the scan then reads
`within`'s rows (or every row) with the filter, the sort and the limit
above taking the first.  `EXPLAIN`: `Index Scan using ix on t (col f
arg) first k by f(col) [DESC] [with values]` for a condition ranked, `(k nearest col
to q [in c = …] [among ix: …])` for HNSW's — `among docs_body: body @@
'fox'` where the text index's matches are what it ranks.

`crates/sql/tests/index_method.rs` registers a method that
mirrors a column and runs writes, rollbacks, a rewrite, the integrity
check (a stray entry found), the refusals and a drop through it;
`06_indexes.slt` the syntax.

### 8.18 HNSW indexes (`hnsw`, §M14.12 V4b, V5, V6)

`CREATE INDEX [name] ON t USING hnsw (col [opclass]) [PARTITION BY (c,
…)] [WITH (m = 16, ef_construction = 64, quantize = 'none', rescore =
n)]` (`PARTITION BY` and `quantize` below) on a `vector(n)` column (one without dimensions
`22023`; another type, or more than one column, `0A000`): the operator
classes are pgvector's — `vector_l2_ops` (the default, `<->`),
`vector_cosine_ops` (`<=>`), `vector_ip_ops` (`<#>`), `vector_l1_ops`
(`<+>`) — and so are the options' bounds (`m` 2–100, `ef_construction`
4–1000 and at least `2 · m`); the record keeps both options.  A `NULL`
vector is not indexed, nor, for `vector_cosine_ops`, one of zero length.
The graph is `hnsw::graph` (`bench/ann`'s core: the paper's search and
inserts with hnswlib's heuristic, ParlayANN's batch inserts with
NN-descent's local join, FreshDiskANN's delete consolidation, a node's
level from a hash of it); its distances keep the SQL operator's order
(squared Euclidean; one minus the dot product over vectors stored
normalized for the cosine).

Storage (`hnsw::store`, PLAN.md §M14.12 V4's model (C)): under the
index's prefix, `r` the root, `o` + id the objects, `d` + primary key +
nonce a delta row (its vector), `x` + primary key + vector hash + nonce a
dead row.  The graph is immutable objects keyed by a hash of their
contents: vector pages and adjacency pages of 64 nodes (decoded into
chunks of 4,096 in memory), patches of the lists a fold changed (in
memory a chain of levels kept short by the logarithmic method, Bentley
and Saxe: a level takes in the one before while that holds no more
patches, so a lookup walks O(log patches) levels), a directory of the
pages in parts of 256, the dead nodes;
decoded, they are shared by every snapshot (`object_cache`, shared with the text index: up to 1 GiB,
the least recently used dropped past it).  A write puts the row's
vector into the delta and, for a delete or an update of the vector, a
dead key, in the statement's transaction (an update of other columns
writes nothing): every key is new, so writers never conflict.  The
upkeep — each step a transaction of its own, run again on a conflict
(with another upkeep only) — *seals*: the delta and dead keys it sees
moved into a sealed object the root lists (an LSM's memtable flush),
then the delta's range vacuumed (`Kv::vacuum_range`: the moved keys'
tombstones, which every search's scan of the delta would otherwise step
over — 2 µs a row growing to 20 as the seals went on, before); *folds*:
every sealed object's rows the table still has as they are inserted into
the graph in batches on the machine's threads, the nodes of the rows
updated or deleted marked dead (passed over by searches), and once the
dead are 2 % of the graph cut out (each list through a dead node
rewritten from its live neighbours', the entry point moved off); and
*compacts* once a fold leaves the patches past a tenth of the graph's
edges — apart from the folds, as an LSM compacts beside its flushes:
the pages the patches touch rewritten from a snapshot on the machine's
threads (`Index::compaction`), put in transactions of about 8 MiB of
new objects each (no conflicts, none on the KV's path for large
transactions), then swapped in between two folds in a short transaction
(`Index::install`: the patches it took dropped, later folds' kept; a
graph rewritten meanwhile by a consolidation refuses it, its pages
deleted), and the new pages read into memory (`Index::warm`, RocksDB's
`prepopulate_block_cache` for a compaction's output) — a fold no longer
rewrites the whole adjacency (2.7 s at 1 M rows, the delta piling up
meanwhile).  It runs after each statement outside a transaction block
and each block's commit, by that session
(`index_method::after_statement(kv, wrote)`; PostgreSQL's GIN
`fastupdate` cleans its pending list so), for each index of that
database with rows waiting (the process notes the delta's keys written
and the rows left sealed): a seal once the delta passes the seal's size,
a fold once the sealed rows pass the fold's — while rows come in (a row
a second or more), only by a session whose statement may have written,
so a search never waits on it.  The sizes are economic order quantities
(Harris) of what the index measures as it runs — writes and searches a
second, a fold's fixed cost (a decayed line fitted to folds' times
against their rows), a seal's, and what a waiting row costs a search,
sealed and in the delta (averaged from the searches' own scans) — at
most 10,000 rows, a fold's nudged a fifth either way in turn; as the
writes stop their rate falls and so do the sizes, so the rows left
waiting are folded by the searches that follow.  `hnsw::upkeep_now`
runs it whatever the sizes (tests).

`hnsw::search(txn, t, i, q, ask, wanted, keep)` (`Ask`: `k`, `ef`,
`ef_most`, the share of the rows expected to pass, the rows to search
among): the graph searched (its dead nodes passed over) and the sealed
rows (from memory) and the delta (from the KV) scanned, the candidates
checked against the table's rows in the snapshot — a row gone passed
over, one found twice once, one `keep` refuses (a filter) left out, each
ranked by its distance as the SQL operator takes it.  `ef` counts the
candidates that *pass* (as a filtered HNSW search's result list holds
only those — hnswlib's, ACORN's): the graph is asked for `ef` over the
share expected to pass, and again — sized by the share that passed among
the rows it read (a filter that goes with the vectors passes fewer near
the query than overall) — where fewer than `k` passed or that share is
under half the expected, up to `ef_most` or until the index has no more
(pgvector 0.8's iterative scans, sized by what was seen; a row looked at
once is not read again); `hnsw::nearest` is it without a filter.  The
candidates are read nearest first by the graph's distance and the
search stops once `k` rows that pass are nearer than the next, so it
reads about `k` over the share passing, not `ef`; a row whose vector is
no longer the one found (its distance in the graph's terms differs, to
the bit) is passed over — its vector now is in the index too, under its
own distance.

**Among given rows** (PLAN.md §M14.12 V5): the search is exact over
their vectors in the index, read from memory — no graph walked
(`Index::candidates_among`).  A row's nodes are found by a hash of its
primary key (`ByRow`, kept in memory per graph and grown as nodes are
added — what a fold marks dead by, too — behind a read-write lock: the
fold takes it only to mark), all the rows' nodes first and then their
vectors in the nodes' order (lookups that do not wait on each other,
vectors read along their chunks), on up to `parallel_workers` threads of
2,048 rows each; the sealed and delta rows among them come from the
scans the search makes anyway.  A node of another row (two hashes met)
or of a vector the row no longer has is checked against the row as every
candidate is.  A snapshot older than the map's graph (rebuilt since) is
searched through the graph instead.

**Partitioned: `PARTITION BY (c, …)`** (PLAN.md §M14.12 V5): `CREATE
INDEX ix ON t USING hnsw (v) PARTITION BY (tenant_id) [WITH (…)]` keeps a
graph per value of the columns — for many tenants each too large for the
exact search above, a graph of its own size each.  The columns (each with
a key encoding and equality, none the vector's) follow the vector among
the index's columns, and the option `partition_by` holds their number —
so `USING hnsw (v, tenant_id) WITH (partition_by = 1)` is the same index,
as `pg_indexes.indexdef` shows it, and a column dropped or retyped takes
the index with it as any other's.  A graph's keys are under the index's
prefix, `p` and the values key-encoded (dropping the index drops them
all); each has its own upkeep, delta, seals, folds and compactions, and
its own map of rows to nodes.  A row with a `NULL` among the values is not
indexed (no `col = value` finds it).  A write puts a row into its values'
graph; a row whose values change gets a dead key in the old graph and a
delta row in the new — the fold of the old graph finds the row no longer
there (`current` reads its values too).  A search goes through one graph:
the optimizer takes an index whose `PARTITION BY` columns the conditions
— the filter's, or under row-level security the policy's (a tenant's
`tenant_id = current_setting('app.tenant')::int`) — each hold equal to a
value fixed for the scan (`MethodRank`'s `partition`; `EXPLAIN` `…
in tenant_id = …`), the one with most such columns where several serve;
its costs on the partition's rows (the table's times the equalities'
selectivity; a table not analyzed is searched, the search finding out).
Without such conditions a partitioned index is not used.  The integrity
check checks each graph against the rows with its values.

**A partitioned table** (§M14.20): `CREATE INDEX … USING hnsw` on it
makes one on each partition, a graph each; `ORDER BY col <op> q LIMIT k`
over it searches each partition's index for `k` rows (each partition its
own choice, as below) and the sort and the limit above take the nearest
— partition pruning leaving the one a condition on the key fixes.

**Quantized: `WITH (quantize = …, rescore = n)`** (PLAN.md §M14.12 V6):
the graph holds codes instead of the vectors — in memory, in its pages
and its delta — and takes its distances between them; the rows, which a
search reads anyway, give the exact distances the answer is ordered by.
`quantize` is `none` (the default), `half` (IEEE halves, the nearest;
magnitudes past 65504 clamped, under 2⁻¹⁴ zero: half the bytes),
`int8` (a byte a component, each `m + s · byte` with the vector's own
least component `m` and step `s = range / 255`, after a header of `m`,
`s`, the bytes' sum and the norm² they stand for — so a distance is one
integer dot product and a few multiplications: `graph::Codec`, the
kernels `dot_u8`, `l1_u8`; a quarter of the bytes and 16) or `binary` (a
bit a component, set where it is above the vector's mean; distances the
bits that differ, the kernel `hamming`; a thirty-second of the bytes).
The codes of the cosine class are of the vectors normalized.  `rescore`
(1–100; only with `quantize`) is the rows a search reads for each it
returns — the graph asked for at least `k · rescore` candidates, the
rows read until that many have passed (`hnsw::search`'s `reads`), their
distances the rows' own; its default is the codec's (`half` 1, `int8` 2,
`binary` 5: Qdrant's and Elasticsearch's oversampling, measured here),
and the record keeps it.  The check compares a node's code with the
row's vector coded.  Binary codes say little of a vector of few
components: they are for embeddings of hundreds and thousands.

**Searches in SQL.**  `ORDER BY col <op> q LIMIT k [OFFSET o]` (the
operator or its function — `l2_distance`, `cosine_distance`,
`negative_inner_product`, `l1_distance` — either way round), over a
table with an HNSW index on `col` whose operator class is that
distance's, `q` the same for every row (a constant, a parameter, an
enclosing row's value — a lateral item's left row's, so `FROM items a,
LATERAL (SELECT … ORDER BY b.v <=> a.v LIMIT k)` searches once per row
—, or a subquery that does not read the row: `(SELECT v FROM items
WHERE id = $1)`, "more like this", run once for the statement as
PostgreSQL's InitPlan; `optimizer::fixed`), becomes a scan through the
index's ranking (`Access::Method`'s `rank`, the optimizer's `ranked`,
§8.17; `EXPLAIN`: `Index Scan using ix on t (k nearest col to q)`) where
it costs less than the scan it had: about
`ef · (3 + 2m / 10)` rows' reads, `ef` the session's `hnsw.ef_search`
(default 40, 1–1000, `SET` / `RESET`) over the share of the rows the
filter is estimated to pass, against the rows the scan reads; and not
where `ef` passes the rows over `2m`, where a search costs what reading
every row does.  A scan through a key range of a B-tree index the filter
chose always becomes one, the range kept (`within`; `EXPLAIN`: `… among
ix: col = …`) — so does a scan through a text index's condition
(`WHERE body @@ q ORDER BY emb <-> v LIMIT k`; `among ix: body @@ …`,
its matches from the index's `rows_where`, `@@` checked on the rows the
graph finds where they are many): its rows are searched among exactly, reading the range's
entries and about `k` rows where the range's scan reads every row in it
— unless they are more than √(rows · (22 · ef + 20 · k))
(`hnsw::among_most`: where reading their entries, ~70 ns each and
~35–115 ns to find each one's vector, costs what the graph's search
does — asked for `ef` over the share passing, ~3.7 µs a candidate, and
reading ~`k` over it, ~3.4 µs a row; 33,000 of a million for `k` 10 at
`ef` 40), or estimated to be twice that (the range not read): the graph
then, as without one.  Under
row-level security the policy and the statement's filter above its
barrier become the search's `checks`, evaluated in that order on each
row it reads — the search goes on until `k` rows the session may see
pass, and no predicate of the statement is evaluated on a row the policy
hides.  A table not analyzed has the rows the index holds
(`hnsw::rows`), and keeps a primary-key range its filter chose.  The
executor (`exec::method_rows`) gives the scan's filter, the checks and
the hash joins' key filters to the search as `keep` and reads only the
columns the plan needs (the method's `rank`); the sort and the limit
above stay.  When fewer than `k + o` rows pass — the index holds fewer
(a `NULL` vector is not in it, and sorts last), or the filter is
narrower than the estimate and `ef` reached the rows over `2m` — the
scan reads the B-tree range's rows instead, or every row without one, so
the answer is the exact one's.  A descending order, the
distance not the first key, an expression of it, a query that reads the
row, or no limit: read and sorted.  `hnsw::status`: the nodes, dead
nodes, patches, sealed rows and delta keys.  The integrity check reads
the root and every object it names, checks the edges' ends, and that
every row with an indexed vector is in the graph (live), a sealed object
or the delta with that vector.  `crates/sql/tests/hnsw.rs`: recall
against the exact search after the build, with a delta, after folds,
marks and consolidations and a table rewrite; a snapshot from before an
upkeep; each operator class; the definitions refused; the upkeep after
commits; `ORDER BY … LIMIT` through the planner — the plans chosen and
not, recall against the exact search, `hnsw.ef_search`, a filter, an
offset, the fallback to every row, an enclosing row's vector, writes;
the rows left waiting folded by searches; searches among a B-tree's
range exactly the exact search's — through rows not yet folded, changed
vectors, deletes, the upkeep, a transaction's own writes — a range with
fewer rows than asked for, and ranges too many to search among (expected
so, or found so) through the graph at the recall unfiltered; a policy's
rows only, a predicate that fails on hidden rows never evaluated on
them; a partitioned index — the definition checked, each value's graph
searched where the conditions or a policy fix it, through inserts, rows
moved between values, changed vectors, deletes, the upkeep and the
column's type changed; a partitioned table searched in each partition
or the one its key fixes; and (unit tests in `store`) a compaction worked out, a fold
meanwhile, swapped in — the later patch kept, the index whole and exact
— and a second one from the same graph refused, its pages gone; a search
among rows in the graph, sealed, in the delta and changed since, on one
thread and on four.  `bench/ann/filtered.py`: filtered searches on
SIFT-1M, nucleus against pgvector (`docs/benchmarks.md`).  `crates/sql/tests/hnsw_crash.rs`:
writes, rollbacks, seals and folds on the simulation, a crash at an I/O
operation (dropped unsynced writes, torn ones), reopen — the rows a
step's, the integrity check clean, a search at a wide `ef` the exact
search's (and each tag's graph of an index `PARTITION BY (tag)`, rows
moving between them), the upkeep going on.  `bench/ann/sql.py`: SIFT-1M through SQL
as `bench/ann/pgvector_ref.py` runs pgvector (`docs/benchmarks.md`).

### 8.19 Text analysis, queries, indexes and BM25 (`text`, §M14.21 T1–T6)

An **analyzer** turns a text into **terms** — what a text index keeps and
a text query looks for (§M14.21's later steps).  `text::Analyzer` has a
`name` and `analyze(text, out)`, which gives `Token { term, position,
start, end }`: the term, its word's position among the text's words (a
dropped stop word leaves a gap, so a phrase does not match across it)
and its bytes in the text.  The same text gives the same terms, always.

Analyzers are **registered** (principle 12) and kept for the process, as
index methods are: `text::add`, a module's through
`Registry::add_analyzer`; `text::named(name)` finds one (`42704`
otherwise); `nc_catalog.text_analyzers` lists them.  A module reaches
them through `ext::text` (`named`, `tokens`, `Analyzer`, `Token`,
`register_builtins`), and the bound expressions `ScalarFunction::rewrite`
and `found` read and give as `ext::Expr` — `nucleus_sql::ext` is all of
the SQL layer a module names (§M14.19).  The built-ins
(`text::register_builtins`, once a process):

| Name | Does |
|------|------|
| `simple` (the default, `text::DEFAULT`) | words, lower case |
| `porter` | Lucene's `EnglishAnalyzer`, which Anserini's BM25 baselines use: words, English possessives (`'s`, `’s`) dropped, lower case, Lucene's 33 stop words, the Porter stemmer of 1980 with its reference implementation's departures (`text/porter.rs`) |
| `danish`, `dutch`, `english`, `finnish`, `french`, `german`, `hungarian`, `italian`, `norwegian`, `portuguese`, `russian`, `spanish`, `swedish` | words, lower case, the language's Snowball stop words, its Snowball stemmer |
| `trigram` | every three characters of the whole text, lower case (SQLite FTS5's `trigram`): words by their parts in any language |

**Words** are UAX #29's (`text/words.rs`): the standard's rules WB1–WB999
over the word-break property and Extended_Pictographic
(`text/word_breaks.rs`, generated from the Unicode 16 database by
`scripts/gen-word-breaks.py`); a segment with a letter or a digit is a
word — `can't`, `3.14`, `e.g`, `a_b` whole, an ideograph alone, a
katakana run together.  ASCII is classified without a table search.

**Stemmers**: Porter's, written here; the others are Snowball's
(`text/snowball/`): the runtime and the algorithms exactly as Snowball
3.0.1's compiler generates them for Rust (only their `use` paths
changed; `#[rustfmt::skip]` keeps them so), BSD licence in the module.
Stop words: Snowball's lists (`text/stop_words.rs`, generated by
`scripts/gen-stop-words.py`), sorted by bytes for a binary search.

`tokenize(text [, analyzer])` returns the terms an analyzer makes of a
text, as an array (`NULL` for `NULL`): what an index on it would keep.

*Tests:* `words.rs` — every case of Unicode's `WordBreakTest.txt`
(`crates/sql/tests/data/word_break_test.txt`, comments removed); `porter.rs`
— Snowball's Porter vocabulary (the paper's algorithm, the departures
off) and the three departures; `snowball/` — each language's vocabulary;
the vocabularies in the repository are every fortieth word, the whole
of each checked where `bench/text/data.py` fetched them into
`target/text-data` (Snowball's data at the commit of 3.0.1); `56_text_analyzers.slt`
— `tokenize` with each kind of analyzer, the catalog view; `tests/text.rs`
— every analyzer over any string (proptest): tokens at character
boundaries, positions in order.

**Text indexes** (§M14.21 T4, `text/index/`): `CREATE INDEX … USING
text (col) WITH (analyzer = 'english')` on one `text` column (another
type `0A000`; an analyzer not registered `42704`; `k1` and `b`, BM25's
parameters (T5: `k1` ≥ 0, `b` from 0 to 1; Lucene's 1.2 and 0.75 by
default), else `22023` — as any other option), `simple` by default; the
options keep `analyzer`, and `k1` and `b` where given.  The index
decides what `col @@ q` means: its analyzer, given by the binder
(`IndexMethod::operand`), on the rows as through the index; a filter's
`col @@ q` with `q` fixed for the scan is found by it (`Access::Method`,
§8.17).  A `NULL` is not indexed.

*Storage* (`text/index/store.rs`), model (C) as HNSW's (§8.18) but exact
in every snapshot.  Keys under the index's prefix: `r` the root (the
segments, oldest first: object id, documents, deleted object, deleted
count); `o` + id + part an object in parts of 1 MiB (ids are nonces,
never reused: an object is written once); `d` + primary key + nonce a
delta row (the text's hash, its terms with positions, its length); `x` +
primary key + text hash + nonce a tombstone (the old text's length and
distinct terms).  A write puts one or both in its statement's
transaction (an update that leaves the text and key alone, none).  A
row is in the index as many times as its (key, hash) was inserted less
the times it was deleted, wherever each is — segment, delta or tombstone
— so a search counts versions: `View::matching` finds the segments'
documents not deleted and the delta's rows that match, and drops those
a tombstone of the same version cancels.  `View::stats(terms)` —
documents, total length, each term's documents — adds the segments'
counts, takes off their deleted objects' (each keeps its documents' bits,
count, total length and per-term counts), adds the delta's inserts and
takes off its tombstones: the snapshot's rows' numbers, which BM25 (T5)
reads.

*Upkeep* (`text/index/mod.rs`): a write notes its index; after a
statement, once `SEAL` (1,024) rows wait, the session seals — in a
transaction of its own (`hnsw::retrying`), one index at a time
(`Pending::busy`) — the delta into a segment: a tombstone of a row in
the delta drops it, one of a row in a segment marks it in a new deleted
object for that segment (`Segment::find_doc` by key and hash); a segment
all deleted goes; the delta's keys are deleted and their range vacuumed.
Then merges: `MERGE` (8) segments of a tier — live documents in powers
of 8 — written as one, the deleted documents left out (Tantivy's
logarithmic policy).  `upkeep_now` (tests, `VACUUM`), `merge_all`,
`shape` and `stats` are public for tests.  `CREATE INDEX` reads the
rows' texts, then analyzes them and makes segments on the statement's
workers (`parallel_workers`) — a batch each, the rows over the workers,
4,096 to 100,000 a segment (Lucene's and Tantivy's indexing threads, a
segment each) — and runs the merges due a round at a time, a round's
groups at once and each group's terms in runs on its share of the
workers (`segment::merge`: cut at terms of the part with the most,
joined in order; the same bytes as one run).  Where the batches'
segments hold 3 GB or less together they are merged into one at once
(`merge_all` on the workers; Lucene's `forceMerge(1)` for an index done
with) — a search looks each term up once a segment, and one merge reads
each document once — else the merges due run.

*Segments* (`text/index/segment.rs`): documents (primary key, text
hash, length), terms sorted, each with its document count and its
postings in blocks of 128 — the documents' deltas and the frequencies
less one, each bit-packed at the block's width (Lucene's and Tantivy's
layout, the bits a block needs, no SIMD yet) — and its positions apart
(varint deltas), read only for phrases.  Version 2: a term in more than
one block has a skip list — per block its last document, its bytes and
its *impacts*, the (frequency, length) pairs of its documents no other
there beats (Lucene's competitive impacts, a Pareto frontier) — so a
search reaches a block without decoding those before and bounds its
scores without decoding it (`Segment::blocks`, `block`); version 1 is
read, its skip lists made as it is decoded.  A segment is decoded once into
memory and shared by every snapshot (`object_cache`, the HNSW index's
cache moved out of it: `Kind::TextSegment`, `TextDeleted`).  A search
decodes each leaf's postings (a prefix's terms by a range of the
dictionary, a fuzzy term's by a pass over it), evaluates the query's
tree over sorted document lists (intersections, unions — of many lists
through a bitmap of the segment's documents —, differences; phrases
checked on their candidates' positions) and the delta's rows by
their stored terms.

*Tests:* `segment.rs` — bit packing at every width, a segment of several
blocks round-tripped, a merge dropping deleted documents, every block of
a skip list reached and bounded by its impacts (version 1's made alike),
a merge in runs the same bytes as one;
`tests/text_index.rs` — through a build, a delta of updates, deletes,
inserts, a rollback and a row changed back and forth, seals, twelve
rounds of writes and merges, a merge of everything, the table emptied:
at every step each of eleven queries through the index against the same
on the rows, and the integrity check (the rows' versions, the documents,
lengths and every term's document count); a snapshot keeps its
statistics through writes and seals; the index decides the analyzer;
a row deleted in a segment stays deleted; `tests/text_index_crash.rs` —
a crash at any I/O point of a workload of writes, seals and merges
(dropped and torn writes), then the rows of some step, the index exact
and its answers the rows', and writes and an upkeep after; and a
transaction's snapshot keeping its answers and statistics while other
sessions write, seal and merge; `58_text_index.slt` — the syntax, the
catalog, `EXPLAIN`, the analyzer with and without the index, writes and
a rollback.

**BM25** (§M14.21 T5, `text/score.rs`): `score(col @@ q)`, on a column
with a text index, is how well each row's text matches `q` — Lucene's
`BM25Similarity` over the index's statistics as the statement's
snapshot sees them: a term's `idf · tf / (tf + k1 · (1 − b + b · dl /
avgdl))`, `idf = ln(1 + (N − n + ½) / (n + ½))`, with the text's exact
length in terms (Lucene keeps a byte); a phrase's, of its frequency
with its terms' IDFs summed; a prefix's or a fuzzy term's 1 where it
matches (Lucene's constant score); a boolean query's, the sum of its
required and matching optional parts' — a word twice in a query counts
twice; 0 where the query does not match, a `NULL` text too.  Without a
text index `score` is refused when bound (`0A000`), as is a query that
varies by row.

The text index binds it (`IndexMethod::function`, §8.17;
`text::score::bind`) as `text_score(col, q, analyzer, k1, b, (SELECT
text_stats(table, index, q)))`: the statistics — documents, their total
length, each exact term's documents (`View::stats`) — in an
uncorrelated subquery, read once a statement; `text_score` scores a
row's text by them (`Scorer`, the query analyzed with the index's
analyzer).  `ORDER BY score(col @@ q) DESC LIMIT k` over a scan the
index finds the rows of (`WHERE col @@ q`, with other conditions, a
join's key filters or row-level security or not) is ranked by the index
(`IndexMethod::rank`, §8.17): `View::ranked(q, sc, n)` scores from the
postings — their frequencies, positions for phrases, the segment's
lengths — through the same `Scorer`, so the numbers are a row's, the
`n` best matches and every one tied with the `n`-th (all of them where
`Ranked::complete`), and the rows are read greatest first in slices
(`Ranked::next`: a selection over the rest, then a sort of the slice) —
`k` over the share the optimizer expects to pass the other conditions
first, then as many as the share passing so far needs, at least as many
again — until `k` passed and every row tied with the `k`-th passing one
has been read (a lookup each, in that order: it stops at the `k`-th
exactly — a merge with the table's scan, as a served condition's rows
are read, measured slower here; PLAN.md §M14.21 T5b); the sort and the
limit above order them — by the scores the ranking gave with the rows
(`RankAsk::values`, §8.17): `score()` does not analyze a returned row's
text again (PLAN.md §M14.21 T5c); `text_score` scores the rows only
where the index cannot rank them (a query of `score` not the
condition's).
`n` is `k` where no other condition is, else four times the rows the
expected share takes; every match is scored where `n` is a 64th of the
matches or more (about: no more than the documents or the leaves'
documents together), and where the first `n` run out before `k` pass,
a second pass takes four times the rows the share seen so far needs (by
the same rule), a third every match — never more than two passes and a
full scoring.

A bag of words (one term, or optional terms only) is scored by
**block-max MaxScore** (Turtle and Flood's; Lucene's
`MaxScoreBulkScorer`, `View::live_matches` → `max_score`): the threshold
is the `n`-th best score of the versions found so far (`Top`); documents
in windows of 4,096 (the first a leaf the threshold leaves essential
over the segment holds); in each, the leaves sorted by their bounds
there — their blocks' impacts' best score, by the snapshot's statistics,
a unit more — and those whose bounds together stay below the threshold
*non-essential*: a document only they hold cannot reach it, a window
none is essential in is skipped.  The essential leaves' postings are
summed term at a time into the window's buffer; the documents found that
may still reach the threshold are the candidates; each non-essential
leaf, best bound first, is matched against them — its postings read
through where they hold four times the candidates or fewer, else each
candidate looked up (`Cursor::advance`: blocks skipped by their last
documents, undecoded) — and candidates that can no longer reach it
dropped.  A term in one block is decoded into its cursor once, its
bound the score of its greatest frequency in its shortest text; with no
threshold (every match wanted) the leaves' postings are summed term at
a time over the segment, no windows (`every_match`).  Scores are summed
in fixed point, units of 2⁻³²
(`score::fixed`), in the index and on rows alike: integer sums, the same
whatever order the terms are added in, so MaxScore adds them in its own
and the numbers are still a row's (as PISA's and JASS's quantized
scores, far finer: 2.3 × 10⁻¹⁰).  A block's bound is its best impact's
score rounded as a part is, plus a unit, so only documents that cannot
reach the threshold are skipped: the ties of the `n`-th stay.  Any other
query is scored document at a time over cursors into each leaf's
postings.  A version is counted only where a tombstone in the delta
names it: a segment's document not marked deleted is otherwise live and
the only copy of its row (`View::live_matches`) — the copies of a named
row are of one text and score alike, so a threshold another reaches
leaves none of them out; only versions not named raise it.

*Tests:* `score.rs` — the formula against a computation by hand, sums
over the tree, phrases, prefixes; `store.rs` — MaxScore's matches over
segments of many windows (windows skipped, documents skipped to, deleted
documents) against every document scored on its text, at `n` from 1 to
all: the best, their ties, the same scores; `59_text_score.slt` — scores against
a computation outside, the refusals, `EXPLAIN`, the first rows with an
offset, ties; `tests/text_index.rs` — at every step of its writes,
seals and merges, eleven queries' first seven rows by score through the
index against the same scored on the rows (scores to the last bit, ties
broken alike); `pruned_as_scored_in_full` — a Zipf vocabulary, texts of
very different lengths, an index built on four workers, deletes in
segments and the delta: seven queries at five `k` with three filters,
through the index and on the rows, alike.  `bench/text/beir.py` checks the rankings against
Anserini's and ParadeDB's on BEIR (docs/benchmarks.md).

**On every kind of table** (§M14.21 T8c): a text or sparse index is a
table's whatever its storage — it is told each row written and builds
over the rows the storage gives (§8.15), `row`, `column` and
`row,column` alike — and `CREATE INDEX` on a partitioned table makes one
on each partition (§8.16).  A partitioned table's query is its
partitions' scans, each served by its own index: `@@`'s rows, and the
first `k` by BM25 or by `<#>` from each, the sort above keeping the first
`k` of them — their scores read from the indexes (`with values`) where
every partition's gives them, else computed again on the rows kept —;
`rrf` and `convex` likewise (§8.20).  BM25's
statistics are the partitioned table's — documents, their length and
each term's documents summed over its partitions' indexes
(`text::index::stats`: each partition's index of the same definition,
looked for past the table's place where the partition has indexes of its
own) —, so a row scores as in one table, whichever partition holds it.
*Tests:* `tests/text_tables.rs` — a `row`, a `column`, a `row,column`
table and one partitioned into partitions of each storage, the same rows
written to each: built over a load, then inserts, updates, deletes, rows
moved between partitions, and sealed; at each step eight queries' rows,
their first seven by score with three filters, highlights and snippets,
the first by `<#>` and `rrf` and `convex` of both alike to the `row`
table's (scores to the bit), served by the indexes (the scores read from
them), and the integrity check.  On BEIR (`bench/text/beir.py --layout`,
docs/benchmarks.md) a `column` table ranks as fast as a `row` one, one
partitioned four ways in about twice the time, the same nDCG@10.

**Text queries** (§M14.21 T2, `text/query.rs`): `text @@ query` is true
when the text matches the query.  The syntax is Lucene's, the subset of
it one column needs, so what people and models already write works:

| Query | Matches |
|-------|---------|
| `word` | the word, analyzed as the text is; a word the analyzer makes several terms of (`e-mail`, `P/E`, any word under `trigram`) is a phrase of them; a stop word is dropped |
| `"some words"` | a phrase: the terms at consecutive positions (a dropped stop word keeps its place) |
| `"some words"~N` | the phrase within `N` moves (Lucene's slop: a word between is one, a transposition two; `N` up to 1,000) |
| `word*` | any term starting so — lower-cased, not stemmed (Lucene's normalization) |
| `word~`, `word~1` | any term within 2 (1) edits — insertion, deletion, substitution, transposition of neighbours; lower-cased, not stemmed |
| `a b` | either (Lucene's, Tantivy's, Elasticsearch's default: ranked search wants any of them) |
| `+a b` | `a` required, `b` then only ranks |
| `-a`, `NOT a` | without `a`; a query of exclusions only matches the texts without them |
| `a AND b`, `a OR b`, `( … )` | `NOT` binds tightest, then `AND`, then `OR`, then words side by side |

`AND`, `OR`, `NOT` are operators in upper case only; `+` and `-` only
before a word (`C++`, `a - b` are words).  Other punctuation breaks words
as the analyzer does, so a question typed or generated as plain text is
a query.  A malformed query — an open quote or parenthesis, `a AND`,
`~3` — is a `42601` error with the character it was found at.

The query is parsed once into its distinct **leaves** (exact, prefix and
fuzzy terms) and a tree of `Leaf`, `Phrase` and Lucene's `Bool { must,
should, not }` over them; a text is analyzed, each term's positions
noted for the leaves it matches, and the tree evaluated.  A phrase's
occurrences come from one function (`query::phrase`) wherever positions
are read — a row's text, a segment's postings, BM25, a highlight:
exactly, each position of its first term the others follow at their
offsets (binary search); within a slop, Lucene's `SloppyPhraseMatcher`
— each term's position less its offset, the least moved on (through the
positions that keep it least, so the tightest occurrence from there
counts), an occurrence where the greatest less the least is at most the
slop, weighing 1 / (1 + that) in BM25's frequency (§M14.21 T6; a term
twice in one phrase may share a position, as Lucene's would without its
repeats' rule).  The analyzer is a third,
optional argument of the operator's function, which the binder gives
from the column's text index — one meaning with or without an index;
without one it is `simple`.  The batch kernel parses a constant
query once a vector; a row at a time, a per-thread cache keeps the last
query parsed.

**Highlights and snippets** (§M14.21 T6, `text/highlight.rs`):
`highlight(col @@ q [, start, stop])` — the text with what the query
found marked, `<b>` and `</b>` by default — and `snippet(col @@ q [,
start, stop, ellipsis, words])` — its best window of `words` terms
(PostgreSQL's `MaxWords`, 35; 1 to 1,000), marked, `...` where the text
goes on.  Functions of the condition, as `score` is: the binder binds
them (`ScalarFunction::of_condition` / `bind_condition`, §8.12) as
`text_highlight(text, q, analyzer, …)` and `text_snippet(…)` of the
`@@` call's operands — its analyzer the column's text index's, `simple`
without one — so a text is analyzed as `@@` analyzes it; of anything
but `@@`, refused (`0A000`).  Marked: each word an exact, prefix or
fuzzy term finds, each occurrence of a phrase (within its slop) from its
first word to its last as one mark (SQLite FTS5's `highlight`), nothing
only an exclusion names; overlapping marks are one; the text between is
kept as it is, not escaped (`ts_headline` neither).  The snippet's
window holds the most distinct leaves, then the most marked terms, the
earliest of equals (FTS5's order), and is moved to put its marks in the
middle where they leave room; a text with nothing found gives its first
`words` terms, an empty one nothing.

*Tests:* `query.rs` — the operators, phrases across stop words, phrases
within a slop and their weights, prefixes
and fuzzy terms under a stemmer, plain text as a query, syntax errors,
the edit distance, and any string as a query (proptest: an error or an
answer, never a panic); `highlight.rs` — marks, phrases as one, slops,
prefixes, stems, overlaps, snippets' windows; `57_text_queries.slt` —
`@@` on rows, `NULL`s, a query that varies row by row, the errors, in
every executor mode; `60_text_highlight.slt` — highlights and snippets
with and without an index, slops ranked by the index against a score
computed by hand; `tests/text_index.rs` — two phrases within a slop
among its queries, the index's rows and scores the rows' at every
step.

### 8.20 Hybrid search (`hybrid`, §M14.21 T7)

Rankings fused in one query: `ORDER BY rrf(r1, r2, …) DESC LIMIT k`
(2 to 8 rankings) and `ORDER BY convex(r1, r2, α) DESC LIMIT k`, over
one table's scan (with its filter and row-level security; a partitioned
table's, its partitions' scans):

```sql
SELECT id, snippet(body @@ $2) FROM docs WHERE tenant = $1
ORDER BY rrf(score(body @@ $2), emb <=> $3) DESC LIMIT 10;
```

A ranking is greatest first, a distance (`<->`, `<=>`, `<#>`, `<+>`,
`…_distance`, `negative_inner_product`) nearest first; a function whose
rows are a condition's (`ScalarFunction::found`: `score(body @@ q)`'s
are `body @@ q`'s) ranks only those.  Each ranking gives its **list**:
its first `window` rows — 100, or the limit and the offset where more —
of the rows the query keeps, ties by the primary key (Elasticsearch's,
OpenSearch's and Weaviate's `rank_window_size`).  `rrf`: Σ 1 / (60 +
rank) over the lists a row is in (`rank()`, ties share one; Cormack,
Clarke and Büttcher's).  `convex`: α·n1 + (1 − α)·n2, each ranking
min-max normalized over its own list — (x − min) / (max − min), a
distance (max − x) / (max − min), 1 where all are equal — and 0 for a
row not in that list or `NULL` there (OpenSearch's `min_max` with an
arithmetic mean, Elasticsearch's linear retriever, Weaviate's
`relativeScoreFusion`).  Elsewhere — no limit, a join, a fusion not the
first sort key — the call is refused (`0A000`).

`hybrid::fused`, an optimizer pass after `narrow`, rewrites `Limit` →
`Sort` (first key the call) → `Project` over a scan, or an `Append` of a
partitioned table's: each list a `Limit` over a `Sort` of a projection of
the scan (the condition of a `found` ranking added to its filter, each
partition's) — which `ranked` (§8.17) then serves from
the index on its column: the text index's first rows by BM25 with their
scores (`with values`), HNSW's nearest — then a window over the list
(`rank()`, or `min` and `max` over all of it) and a projection to the
candidate's columns and the fusion's slots (its rank, or its value and
its list's bounds; the other lists' `NULL`); the lists `UNION ALL`, one
row a candidate by an aggregate grouped by the primary key and the
columns read above (`max` of each slot), and the call replaced by the
fusion of the slots.  No ranking is computed again over the candidates:
a list's values come from its index where it has one.  Without an index
the same plan reads every row.

*Tests:* `tests/hybrid.rs` — `rrf` and `convex(…, 0.3)` against the
fusion computed in Rust from every row's score, match and distance, for
`k` 1, 10 and 50, with a filter and without, with HNSW and without; the
columns read above the fusion; the plans (the text index's first 100
with values, HNSW's 100 nearest on 12,000 rows); the refusals.

### 8.21 Sparse vector index (`text::index::sparse`, §M14.21 T7b)

`CREATE INDEX … USING sparse (sv [sparsevec_ip_ops])` on a `sparsevec`
column ranks `ORDER BY sv <#> q LIMIT k` — the inner product, greatest
first — exactly, from an inverted index: the text index's storage
(§8.19: delta, tombstones, seals, merges, snapshots, the integrity check)
with *weighted* documents.  A dimension is a term (its number in eight
hexadecimal digits, so the dictionary's order is the dimensions'); a
component's weight is the term's frequency in the postings,
`segment::weight_bits` — the `f32`'s bits in the numbers' order (a
negative's flipped, a positive's sign bit set), so a block's impacts give
its greatest weight; no positions (`Doc::weights`; the delta's encoding
marks a weighted term by 0 positions, which a text's never has, so old
deltas read as before; `segment::merge(…, weighted)` copies weights, not
positions).  A query is the bag of its dimensions, each leaf weighing
its component (`Scorer::dot`): block-max MaxScore over the same cursors
as BM25's, a block's bound its greatest weight times the query's.

Exact: the index sums the products in units (2^-32, each rounded once),
`<#>` in `f64` in dimension order — within `MARGIN` (1,025) units a leaf
of each other while each product is under 2^16 (`Scorer::dot` notes one
past it).  `rank` gives every row it scores within that of the `k`-th
passing (filters and row-level security through `keep`, as the text
index's), the projection computes `<#>` of each and the sort keeps the
first `k`: a scan's rows.  It gives up — the rows are read — for a query
weight not positive, fewer than `k` rows sharing a dimension with the
query or a `k`-th product not above the margin (the rest tie at 0), a
product too large, and another condition's rows to rank among.  `NULL`s
are not indexed.  `EXPLAIN`: `Index Scan using ix on t (k nearest sv to
q)`.

*Tests:* `tests/sparse_index.rs` — four queries ranked by the index
against the same order on every row (`(sv <#> q) + 0`), `k` 1, 5 and 20,
with filters passing all, half and a tenth, a tenth of the weights
negative, ties, `NULL`s, built, after inserts, updates and deletes in
the delta, sealed and merged, the integrity check at each step; a
negative query weight and dimensions no row has read the rows; the
refusals.  `segment.rs` — `weight_bits`' order and round trip, a
weighted document's encoding.

### 8.22 PostgreSQL's text search (`modules/pg-compat` `tsearch`, §M14.21 T8)

pg-compat brings PostgreSQL's `tsvector` and `tsquery` — its text forms
both ways (`'cat':2 'mat':6A`, `'cat' & !( 'dog' | 'mice' )`, `<N>`,
`:*`, weights), `regconfig` a name for `text` — the makers
`to_tsvector`, `to_tsquery`, `plainto_tsquery`, `phraseto_tsquery`,
`websearch_to_tsquery` (`[cfg,] text`), and `@@` between a `tsvector` and
a `tsquery` (either way round; `text @@ tsquery` by `simple`).  A
configuration is nucleus's analyzer of the name (`english`, `hungarian`,
…, `pg_catalog.` dropped; none named: `simple`): a `tsvector` is its
terms at their positions plus one.  Queries are built as PostgreSQL
builds them — each operand analyzed, its terms a phrase by their
positions, stop words cleaned out with the phrase distances across them
kept (`cat <-> the <-> mat` is `'cat' <2> 'mat'`; `<N>` up to 16,384,
and the distance after the clean-up added unchecked, as PostgreSQL's:
`cat <16384> the <-> dog` is `<16385>`) — and evaluated as its
`TS_execute`: a phrase's parts' positions merged per operator, `&` under
a phrase where both match, a `NOT` under one where its operand does not,
weights filtering positions, a term without positions a "maybe" (no
phrase matches it); the empty query matches nothing, `!x` the empty
vector.

**The rewrite** (`ScalarFunction::rewrite`, §8.12): `to_tsvector(cfg,
e) @@ maker(cfg, 'literal')` — what a PostgreSQL client writes for a GIN
index on `to_tsvector(cfg, e)` — binds as nucleus's `e @@ q` by the
analyzer `cfg` (§8.19), which a text index on `e` analyzing so serves,
when `q` can say the query exactly: each operand a quoted phrase of its
words, kept only where the analyzer finds the same terms in it; a prefix
as its stem and `*`; `&`, `|`, `!` as `AND`, `OR`, `NOT`; a phrase of
words a step apart as one quoted phrase, kept only where the analyzer
gives them joined the positions PostgreSQL's chain has.  Otherwise —
weights, other distances, a phrase over operators, a query of stop words
only, a parameter, different configurations — `@@` is computed on the
values.  Departures: nucleus's analyzers, not PostgreSQL's parser and
dictionaries; positions not capped at 16,383 nor 256 a term.

**Ranking and headlines** (`tsrank`, `tsheadline`, §M14.21 T8b):
`ts_rank` and `ts_rank_cd` (`[weights,] vector, query
[, normalization]`; weights `{D, C, B, A}`, default `{0.1, 0.2, 0.4,
1.0}`; normalization flags 1, 2, 4, 8, 16, 32) computed as `tsrank.c`
computes them — `ts_rank` in `real` with C's promotions, `ts_rank_cd` in
`double precision` — and the query's operands sorted by PostgreSQL's own
`qsort` (unstable past six items: which of `a` and `a:*` counts follows
it), so the values are PostgreSQL's to the bit.  `ts_headline([cfg,]
document, query [, options])` as the default parser's `prsd_headline`
and `generateHeadline`: the text a list of tokens — `simple`'s words,
the text between two words one token, a number never a headline's end —
each word the operands its terms match (a word matching two repeated);
the query's covers (`hlCover`, by `TS_execute_locations`, each checked
on its words); the best stretched to `MinWords`–`MaxWords` words
(`mark_hl_words`), or the best `MaxFragments` fragments
(`mark_hl_fragments`) joined by `FragmentDelimiter`; the matches between
`StartSel` and `StopSel`; `HighlightAll` the whole text.  Options are
`deserialize_deflist`'s `key = value` list (quoted values; errors as
PostgreSQL's: `MinWords must be less than MaxWords`, `unrecognized
headline parameter`, …).  Departures: UAX #29's words (PostgreSQL's
parser splits `don't` and hyphenated words), HTML tags kept as text.

**On the wire** (§M14.21 T8c): PostgreSQL's front end sends them as
PostgreSQL's types — `tsvector` oid 3614, `tsquery` 3615, in `pg_type`
with their arrays (3643, 3645), named by `format_type` and
`to_regtype`, so a column's type is theirs in `pg_attribute` and
`\d` —: in text format their text forms, in binary format as
`tsvectorsend` and `tsquerysend` give them (the term count, each term's
bytes and a zero, its position count, each position with its weight in
the top two bits; the item count, the items in prefix order — an
operator before its right operand, then its left —, an operand's
weights as bits, whether a prefix, its bytes and a zero, an operator's
code and a phrase's distance); and it takes them as parameters either
way, a binary one checked as `tsvectorrecv` and `tsqueryrecv` check
it (`22P03`; a query nested deeper than 10,000 refused too, its tree read
without recursion).  `tsvectorsend(v)` and `tsquerysend(q)` give the
binary forms in SQL, as in PostgreSQL.  Positions past 16,383, or past
256 of a term, which nucleus's values keep, are sent as PostgreSQL would
have kept them (capped, each once with the greater weight, the first
256).  A `tsquery` is what its text form reads back as: PostgreSQL's
prints `a & (b & c)` as `'a' & 'b' & 'c'`, which reads back as
`(a & b) & c`, and a nucleus value — whose text form must read back as
it (§M14.19's laws) — of either is that one: an `&` of `&`s, an `|` of
`|`s, grouped from the left, which matches, ranks and headlines alike;
its binary form is `'a' & 'b' & 'c'`'s, and the two are equal
(PostgreSQL's are not).  nucleus's own protocol, whose sessions do not
have the module, sends a value as any registered type's (`ncp` tag 11,
§10: the name and the binary form, here its text form's UTF-8): the
drivers give a value of a type they do not know (Rust `Value::Other`,
Python and JavaScript `Ext`, Java `Ext`, C `NC_EXT`), and a text
parameter is taken where a column holds one.

*Tests:* the module's `tests/tsearch.rs` — 5,407 cases PostgreSQL 18
answered (`tests/tsearch_pg.tsv`, by `bench/text/tsearch_pg.py`: random
vectors and queries, phrases at distances 0–3 over every operator,
prefixes, weights, terms without positions, and the makers' queries,
text forms included, the longest distances; `ts_rank` and `ts_rank_cd`
six ways a case, to the bit; headlines of random English with 11 sets of
options, to the byte; 305 values' binary forms, to the byte, the edges
among them) alike; 400 random queries of the four makers by the rewrite
and over stored `tsvector`s alike, without the index and through it, on
a `row` table, a `column` one and one partitioned into partitions of
each storage; the plans served and not.  `tests/wire.rs` — the types
through rust-postgres (their oids, PostgreSQL 18's binary forms as
parameters and results, `a & (b & c)` read as `(a & b) & c`, a malformed
one refused) and through nucleus's protocol (the Rust driver's
`Value::Other`, a text parameter).  `tests/slt/03_text_search.slt`
(PostgreSQL 18's results); unit tests of the printer, the clean-up, the
evaluation, the rewrite's text queries, and the binary forms (round
trips, the capped positions, what `tsvectorrecv` and `tsqueryrecv`
refuse, the depth).

### 8.23 Embedding models (`models`, `modules/embeddings`, §M14.22 E2b)

`CREATE MODEL [IF NOT EXISTS] m USING provider [WITH (option = value,
…)]` defines a model (catalog `00 15 <name>`); `DROP MODEL [IF EXISTS]
m`.  The *provider* is registered for the process
(`models::ModelProvider`, `Registry::add_model_provider`; modules reach
it as `ext::models`): it checks its options — refusing any it does not
know, `22023` — and says the model's dimensions (1 to 16,000) and batch;
it never reaches the network then.  Built in: `hashing` (options
`dimensions`, default 256, and `analyzer`, default `simple`): an
analyzer's terms hashed into buckets (feature hashing without signs, so
words never cancel), counted and scaled to length 1 — no model, no
network.  The `embeddings` module (in the `nucleus` tool) adds `openai`
(any OpenAI-compatible endpoint), `cohere` and `voyage` (its README).
Three options are the core's: `secret` (a secret, §9, whose value the
provider gets when it calls; naming one needs `USAGE` on it, and a
secret is not dropped while a model names it, `2BP01`), `query_prefix`
and `document_prefix`.

**Where a model may call.**  A provider names the addresses a model's
options point it at beside its own (`ModelProvider::urls`: the
`embeddings` module's `url`).  A model made by an administrator may call
any; anyone else's only those under the database's `model_urls`
(`ALTER DATABASE nucleus SET model_urls = 'https://api.mistral.ai/v1,
http://localhost:11434/v1'`, an administrator's): the same scheme and
host (with port), and a path under the prefix's at a `/`
(`models::url_under`; `…/v1` does not admit `…/v10`, nor
`api.example.com` `api.example.com.evil.net`).  Checked when the model
is made and at every call by its owner's standing (an owner without a
role record is the unclaimed database's administrator), so a URL taken
off the list is not called again (`42501`, a hint naming the setting).
A provider's own address (OpenAI's, Cohere's, Voyage's) needs no
listing; plain HTTP still only to loopback (`nucleus-http`).  So a role
that may make models cannot point one at the server's network — an
internal service, a cloud's metadata — unless an administrator lists
it.

`embed(text, m [, 'query' | 'document'])` returns the text's `vector`
(dimensions unconstrained; a `vector(n)` column checks them, `22000`);
a document by default.  The model's prefix for the kind is put before
the text, and a provider whose API takes the kind itself (Cohere,
Voyage) is told.  Calling one needs `USAGE` on the model (`GRANT USAGE
ON MODEL m TO r`; its maker owns it; making one needs `CREATE` on the
database).  `embed` is stable: in a ranking (`ORDER BY v <=> embed($1,
m, 'query')`) the query's vector is computed once.  Vectors are kept in
a cache for the process — by SHA-256 of the model's definition, the kind
and the text; 32 MB, the older half dropped when full — so a text is not
sent twice while kept, nor twice in one call; the vectorized executor
sends a batch's texts in calls of the model's batch (`call_batch`).  A
provider's answer is checked: as many vectors as texts, each of the
model's dimensions, finite.  Failures: `58030` where trying again may
help (no connection, 429, 5xx — tried once more first), `55000` where it
will not (a refused key, an unknown model).  A remote call ends within
the time its statement has left (`models::Request::time_left`, from
`statement_timeout`; `ext::time_left` on the statement's own thread),
at most five minutes, the retry's pause included — `nucleus-http`'s
`Request::total` bounds a whole exchange, so a server dripping a byte
within each read's timeout cannot hold a call.

`nc_catalog.models` (`name`, `owner`, `provider`, `dimensions`,
`max_batch`, `secret`, `query_prefix`, `document_prefix`, `options`, `files`, `bytes`,
`created`, and the process's `calls`, `texts`, `cached`, `errors`,
`total_ms`, `tokens`, `throttled_ms`, and the limits of §8.27; the
models the current role may call) and
`nc_catalog.model_providers` (`name`, `remote`).  Tests:
`tests/models.rs` (`hashing` end to end, privileges, prefixes, the
secret given, one call for a ranking, the cache, a column table's 2,000
rows in batches), `modules/embeddings/tests/providers.rs` (servers on
loopback answering as OpenAI, Cohere and Voyage: the requests, the key,
answers out of order, a 429 tried again, a 401 without the key in the
error, a server down, a dripping server cut off at the statement's
`statement_timeout`).

Local models (`modules/local-models`, §M14.22 E3, in the `nucleus`
tool): `CREATE MODEL m USING local WITH (huggingface = 'org/name' [,
revision = 'main'] | path = '/dir' [, pooling = 'cls' | 'mean'] [,
threads = n] [, batch = n])`.  The provider *fetches* the model's files
when it is made (`ModelProvider::fetch`, the only time a provider reads
files or the network outside a call): `tokenizer.json`, `config.json`,
`model.safetensors` and `1_Pooling/config.json` where there is one —
from Hugging Face (`https://huggingface.co/<repo>/resolve/<revision>/…`,
redirects followed to `https://`, a large file checked against the
SHA-256 the hub announces in `X-Linked-Etag`; another `endpoint`, and a
`path` on the server, only for administrators, `42501`).  A file may be
at most a quarter of the machine's memory, and 8 GB (it arrives in
memory, and its weights run there); the download, all its files, ends
within the statement's time, and an hour.  The core keeps
the files with the model by content — `00 16 <SHA-256> <chunk u32>`, 32
KiB chunks; the record lists their names, digests and lengths — so a
backup or a replica has them, two models from the same files store them
once, and `DROP MODEL` removes them with the last model that keeps
them.  `define` loads the model (a model that cannot run is refused when
made, `55000`) and gives its dimensions; a call reads the files through
`models::Files` (their digest checked) only when the process has not
loaded them (four models kept, by `Files::id`), and runs them on
`threads` (the machine's cores by default).  `nc_catalog.models` shows
`files` and `bytes`; `model_providers` shows `local` as not remote.

Its tokenizers read a Hugging Face `tokenizer.json` — WordPiece behind
BERT's normalizer and pre-tokenizer, Unigram behind SentencePiece's
precompiled character map (by grapheme cluster, UAX #29) and Metaspace —
and give the ids `tokenizers` gives: special tokens in a text are those
tokens, a text is cut with its closing token kept, and anything else a
file may hold is refused by name.  BERT's character classes are Unicode
8's, as `tokenizers`' are (`scripts/gen-tokenizer-tables.py`).
Checked by `bench/embed/tokens.py` (corpora, every code point, random
mixtures: all the same) and `modules/local-models/tests/`.

Its encoder reads `config.json` (BERT, RoBERTa, XLM-RoBERTa; GELU,
absolute positions, a RoBERTa's starting after its padding token's) and
the safetensors weights (F32, F16, BF16; the linear layers packed once
for the matrix product), pools by sentence-transformers' `1_Pooling`
(the first token or the mean) and scales to length 1.  A batch's texts
are one matrix of their tokens, end to end (no padding, no mask), cut to
the model's 512 positions; threads take whole texts, about equal work
each, so a text's vector is the same bits alone, in any batch and on any
number of threads.  The kernels (`ext::kernels`, from `nucleus-kernels`):
`matmul` over a `PackedB` (BLIS's structure: 8 × 12 register tiles on
NEON, 6 × 16 on x86; every output one chain of fused multiply-adds in
order, so the same bits at every CPU level, held to the naive chain by
its test), `gelu` (exact, by a rational `erf`), `softmax_rows`,
`layer_norm`, `add_assign`.  The x86 `Avx2` level now needs FMA too.
Checked by `bench/embed/encoder.py` against ONNX Runtime on the
publishers' f32 exports.

### 8.24 Generated columns and embedding columns (`dml`, `embedding`, §M14.22 E4)

`c type GENERATED ALWAYS AS (expr) STORED`: the expression, over the
row's other columns, is computed on every write of the row — `INSERT`,
`COPY`, `UPDATE` (of any column), a foreign key's cascade, `ALTER
COLUMN … TYPE` of a column it reads — by `Writer::complete` before the
row's constraints are checked, and stored like any value (indexed,
searched, checked).  A value is never given: naming the column in an
`INSERT`'s or `COPY`'s list or an `UPDATE`'s `SET` is `428C9`; without a
list, an `INSERT`'s values and a `COPY`'s fields are the columns that
are not generated.  The expression may not read a generated column, call
a volatile function (`42P17`) or hold a subquery (`0A000`), nor be in
the partitioning; `DEFAULT` with it is `42601`; `VIRTUAL` and identity
columns are refused (`0A000`).  `ALTER TABLE … ADD COLUMN … GENERATED`
computes it for every row.  The table record (version 15 on) keeps a
column's generation after its statistics: `0`, `1 <expr>` or `2 <expr>
<model> <version u32> <next model?>`.  `nc_catalog.columns` shows
`generated` (`stored`, `embedding`), `generation_expr`, `model`,
`model_version` and `next_model`; pg-compat shows
`attgenerated = 's'`, the expression in `pg_attrdef`, and
`information_schema.columns.is_generated` / `generation_expression`.

`v vector GENERATED BY EMBEDDING (expr) USING m`: the vector of the text
`expr` gives, made by model `m` in the background.  When made: the model
exists and the maker has `USAGE` on it; the type is `vector` of the
model's dimensions (`vector` alone takes them, `42804` otherwise); the
column may be NULL, is not in the key, and the table has a primary key
(`42P17`); the expression calls no model (`embed`, `0A000`: the worker
computes the text, not the role that wrote the row, and the column's
model is its `USING`).  A model used by a column or a chunk table is not
dropped (`2BP01`); dropping the table drops its jobs.  `ADD COLUMN` and
`SET MODEL` read the table validated, so a row a writer commits while
they run makes them conflict (`40001`, run again) instead of going
without a job.  `embed(q)` without a model, an operator's
operand beside such a column (`ORDER BY v <=> embed($1)`), is the
binder's `embed(q, 'm', 'query')`: the query embedded by the column's
own model.

The queue (`embedding`): `00 17 <table id u32> <column u16> <primary
key>` → version, enqueued (µs), attempts, not-before (µs), failed, the
last error.  An insert puts a job for each new row; an update puts one
when a column the expression reads changed (compared as values), and
moves a row's job with its key; a delete removes them — all in the
writer's transaction, a job a row and column, so a row written five
times while pending is one job.  A **round** (`embedding::work`) takes up
to 256 due jobs (not-before passed, the model's circuit closed), reads
their texts at one snapshot, calls the models (`embed_unchecked`: the
maker's `USAGE` was checked when the column was made; the models' cache
first; a batch refused as a whole by an error that will not pass is
tried a text at a time), then writes in one transaction a group: for
each job still as read, the row's vector if its text is still the one
embedded (else the job is left for the next round), the job removed; a
NULL text or a row gone just removes it; a text whose expression fails
for a row (a division by zero, say) fails that job, not the round.  A
commit that conflicts leaves the group's jobs for the next round, and a
round that panics is the round's error (the driver freed, the server's
worker alive).  Failures that may pass (`58030`,
`40001`, `57014`, `53300`, `53400`): attempts + 1, not before now + the
provider's `Retry-After` (`models::retry_hint` in the error's hint,
`models::retry_after` reads it) or a full-jitter backoff (a draw in 0 …
min(5 min, 1 s × 2ⁿ⁻¹), from the database's random source).  Others:
failed at once, tried again in an hour or when the row's text changes.
Five failures in a row that may pass open the model's circuit (per
database, in memory): no calls for 30 s, doubling to 5 min while it
reopens.  Rounds run after a session's statements
(`embedding::after_statement`, when a job may be due: a flag set by a
put, or the earliest not-before reached; one round at a time a database)
unless a background worker serves the database (`embedding::serve`,
started by the `nucleus` server: rounds while jobs are due, else asleep
until a job is put or comes due — woken again once the writer that put
it has committed, since a job put is seen only then).

Pending rows, by the session's `embedding.pending`: `include` (the
default: each row's vector as it is — NULL before its first, the old one
while a new one is made), `skip` (the binder puts `NOT
embedding_pending(t)(pk…)` over each scan of a table with embedding
columns: rows with a job, failed ones too, left out), `wait` (before a
query takes its snapshot, rounds until only jobs failed for good are
left, sleeping on the database's clock until a retried job is due; past
the statement's time — `statement_timeout`, else a minute — `57014`; in
a transaction whose snapshot is taken, `25001`).  `Kv::sleep` waits on
the platform's clock, so the simulation passes the time.
`nc_catalog.embedding_jobs` (`table_name`, `column_name`, `model`,
`key`, `state` — `pending`, `retrying`, `failed` —, `attempts`,
`enqueued`, `next_attempt`, `error`; the tables the current role may
read).  Tests: `crates/testkit/slt/61_generated_columns.slt`,
`tests/embedding_columns.rs` (kept current, outages, backoff, the
circuit, Retry-After, the three modes, a refused text, a writer racing a
round, a crash at any I/O point of a workload of writes and rounds).

**A column moved to another model** (§M14.22 E5): `ALTER TABLE t ALTER
COLUMN v SET MODEL m2` (the table's owner; `USAGE` on `m2`; not on a
partitioned table) records `m2` as the column's next model and puts a
job a row under the column's slot with bit `0x8000` set.  Rounds take
the columns' own jobs first and fill the rest of their 256 texts with
next-model jobs, whose vectors go beside the table: `00 18 <table id>
<column u16> <pk>` → the components as f32 little endian (empty: a NULL
text).  While the column moves, a write that changes the text puts both
jobs; a key change moves both and the vector beside; a delete removes
them.  Each round, for a moving column whose next-model jobs are all
done (none left, or this round took the last), `swap` runs one
transaction: no next-model job may be left; every row rewritten
(`dml::replace_rows`) with its vector from beside (a row with none —
written while the move began — gets NULL and a job for the new model;
the scan validated, so a row written meanwhile makes it wait); the
column's dimensions the new model's, its index methods defined again and
built; its model the next, its version + 1; the store beside and the old
model's jobs removed.  A conflict leaves it for the next round.  `SET
MODEL` to the column's current model gives a move up (its jobs and store
removed); to another, starts over.  A model that is a column's next one
is not dropped (`2BP01`).  `wait` does not wait for next-model jobs; a
job failed for good holds the move until it is mended or given up.
`nc_catalog.embedding_jobs.kind` is `current` or `next` (`model` then
the next one).

### 8.25 Chunks (`chunks`, `text::sentences`, §M14.22 E6)

`chunks(text, by, size [, overlap [, model]])`, a table function: a row
a chunk — `position` (from 1), `chunk`, `start` and `length` (characters,
as `substr` takes them: `substr(text, start, length) = chunk`),
`headings` (`text[]`), `size`.  A NULL text has no chunks.  A chunk is a
run of whole units packed greedily up to `size`:

- `sentences` — UAX #29 sentences (`text::sentences`: the default rules
  SB1–SB11 over `text::sentence_breaks`, generated by
  `scripts/gen-sentence-breaks.py` from Unicode 16's
  `SentenceBreakProperty.txt`; all 512 cases of its
  `SentenceBreakTest.txt` pass, kept in `tests/data/`);
- `tokens` — UAX #29 word segments (§8.19's), a window of `size`;
- `markdown` — sections by CommonMark headings (ATX `#`…`######` with up
  to three spaces before, a closing `#` run dropped; setext `===` / `---`
  under a paragraph; none inside a fenced code block), with the path of
  headings above each (a heading closes the ones of its level and
  deeper); a chunk never crosses a section; its units are the section's
  blocks — the heading line, paragraphs between blank lines, a fenced
  code block whole.

A unit larger than `size` is cut finer (a block into sentences, a
sentence into word segments) and its pieces are packed only with each
other, so a chunk is whole units of one kind or the pieces of one; a
word segment over `size` is a chunk alone.  `overlap` (< `size`): the
next chunk begins with the last units of the one before that fit in
`overlap`, when that leaves room for a unit it did not have.  Chunks are
trimmed of white space; one that is only white space is left out.
Sizes are words (segments with a letter or a digit) — or, with a model,
its tokens: `models::count_tokens` asks the provider
(`ModelProvider::count_tokens`, `None` by default: `0A000`; the `local`
provider counts by its tokenizer, without the prefix and the special
tokens), checking `USAGE`; a chunk is counted again whole and given
fewer units while it is over.  Tests: `chunks.rs`'s own,
`slt/62_chunks.slt`, local-models `tests/provider.rs`.

### 8.26 Chunk tables (`chunk_tables`, §M14.22 E7)

`CREATE TABLE c CHUNKS OF docs (body) BY sentences | tokens | markdown
SIZE n [OVERLAP n] [USING model]`: a table of the chunks `chunks()`
(§8.25) cuts from `docs.body`, kept current with `docs`.  Its columns:
`docs`' primary key columns (names and types), `position`, `chunk`,
`start`, `length`, `headings`, `size`, all `NOT NULL`; its primary key
the documents' key and `position`; its lineage a foreign key
`c_document_fkey` to `docs` `ON DELETE CASCADE ON UPDATE CASCADE`.  The
table record (version 16) keeps a chunk table's `ChunkSpec` (the foreign
key's name, the text column's position, by, size, overlap, model) after
its columns' generations.  Made by SQL the module writes and
`session::create_table`, then filled from every document.  Needs: a
primary key on `docs`, a `text` column (`42804`), a table neither
partitioned nor a chunk table (`0A000`); `CREATE` on the schema,
`SELECT` on the column and `REFERENCES` on the key.

Kept current in the writer's transaction (chunking is local; no call
leaves the process): `Writer::new` of a table finds its chunk tables
among the foreign keys referencing it (`chunk_tables::of`); after an
insert (`store_batch`) the new documents' chunks are inserted in one
batch; after an update (`update_rows`, past the foreign keys' actions)
each document whose text changed is chunked again (`rechunk`): its
chunks read by the foreign key, matched with the new ones by text (the
same position first); unmatched old ones deleted, matched ones whose
position moved updated through negative positions (so no two meet on the
primary key), their other derived values set, unmatched new ones
inserted.  A matched chunk keeps its row, so an embedding column over
`chunk` keeps its vector and gets no job.  A key change is the foreign
key's cascade; a delete, its cascade.  `INSERT`, `DELETE`, `COPY` into a
chunk table and `UPDATE` of a derived or key column are refused
(`42809`); columns added to it are the user's.  Its foreign key is not
dropped (`2BP01`); the text column's and the derived columns' types are
not changed (`0A000`); `DROP TABLE docs` is `2BP01` while it has chunk
tables, `… CASCADE` drops them with it.  A chunk table `USING m` counts
tokens without its writers' `USAGE` on `m` (its maker's was checked),
keeps `m` from being dropped, and is filled by a validated scan (a
document committed meanwhile makes the `CREATE` conflict); neither it
nor its documents' table can be attached as a partition (`0A000`).  Tests: `tests/chunk_tables.rs`,
and the crash workload of `tests/embedding_columns.rs` keeps one.

### 8.27 What model calls cost (`models`, `budget`, §M14.22 E8)

Four more core options of `CREATE MODEL`, for any provider (positive
integers, `22023` otherwise; kept in the model record, version 3):
`batch_tokens` (the most tokens a call sends), `requests_per_minute`,
`tokens_per_minute`, `parallel` (calls at once).  A call's texts — those
the cache did not answer, each once — are packed in order into batches
of at most the provider's batch and `batch_tokens` (a text over it goes
alone; the provider says whether it takes it).  A text's tokens are
estimated as its UTF-8 bytes / 3, rounded up, or counted by the provider
(`count_tokens`) for a model with files when a token limit is set.

The rate limits are two token buckets per database and model in the
process, each holding up to a minute's limit and refilled steadily (a
redefined model's start full); a batch takes a call and its tokens (at
most a bucket's size).  Time is the database's clock, so the simulation
passes it.  A statement's call waits for the buckets (`Kv::sleep`)
unless the wait would pass its deadline — then `53400`, its hint `try
again in n ms (the model's rate limit)`.  An embedding column's round
(§8.24) never waits: the jobs get the same `53400`, come back after the
hint's wait, and do not count toward the model's circuit (`models::throttled`).
With `parallel` > 1, a model without files (a remote one) sends its
batches on that many threads, answers kept in order; a local model uses
its `threads`.

Tokens used are the provider's where it reports them
(`Request::report_tokens`: OpenAI's and Voyage's `usage.total_tokens`,
Cohere's `meta.billed_units.input_tokens`, the local model's own token
count), else the estimate.  Each statement is charged as it calls
(`budget::ModelCost`: calls, texts sent, cache hits, tokens, errors, µs
in calls and waiting for limits), its parallel workers too (they share
the statement's counter through `parallel::Ambient`): `EXPLAIN ANALYZE`
prints a `Models:` line, `nc_catalog.statements` sums `model_*` columns,
`nc_catalog.models` the process's `tokens` and `throttled_ms`.  Tests:
`tests/models.rs` (`limits_batches_and_costs`), `tests/embedding_columns.rs`
(`a_rate_limit_spaces_the_rounds`), `modules/embeddings/tests/providers.rs`
(usage reported, calls at once).

### 8.14 Errors and hints (`error`, `hint`)

An `Error` is a SQLSTATE, a message and perhaps a **hint**: what to do
about it, for people and for models.  Sites that know more attach their
own (`Error::with_hint`): an unknown column — the one meant
(`Perhaps you meant column "email".`, by edit distance with swaps, a third
of the letters at most), else the columns in scope when there are 12 or
fewer; an unknown table or `FROM` entry — the one meant, else the tables;
an unknown function — the one meant (aggregates too), a wrong argument
count — the counts it takes; a comparison of two types — the cast.  The
session gives the rest a hint by code (`hint::complete`): `25P02`
ROLLBACK, `40001` run it again, `23502`, `23505`, `42501` GRANT, `22003`
a wider type, `22012` NULLIF, `21000` LIMIT 1, `42P07` IF NOT EXISTS,
`42804` and argument types — a cast.  On the wire: `ncp` 5's `E`
(§10), pg-compat's `H` field (psql's `HINT:`).

## 9. Server (`nucleus-server`)

```
nucleus server <dir> [--listen <addr:port>] [--pg-compat [<addr:port>]]
                     [--front-end <name> [<addr:port>]]
                     [--user <name>] [--password <pw>] [--no-auth] [--log-queries]
                     [--sync ordered|full] [--wal minimal|full] [--cache <size>]
                     [--index-cache <size>]
```

`--cache` sizes the page cache (default `128MB`; units as `work_mem`'s:
`kB`, `MB`, `GB`) — PostgreSQL's `shared_buffers`.  `--index-cache`
sizes the decoded index objects kept across transactions
(`object_cache`: HNSW graphs' parts, text segments) — an index past it
is decoded again each time it is read.  By default it is a quarter of
the memory the process may use (`real::physical_memory`; 1 GB where the
OS does not say) — PostgreSQL's advice for `shared_buffers`, DuckDB's
`memory_limit` sized by the machine too; a bound, not memory taken.  The
server's first line names it (`index cache 8192 MB`).  `--sync` sets how far commits and checkpoints sync (§3.2, D17):
`ordered` (the default) or `full`, which also flushes the drive's cache
(on macOS).  `--wal` (D16): `minimal` (the default) writes a new index's
entries without the WAL and checkpoints at its commit; `full` logs every
write.  Both front ends read and write through 64 KiB buffers.

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

**Secrets** (`secrets.rs`, §M14.22 E2a; catalog `00 14 <name>`):
credentials the database uses on its own behalf (an embedding
provider's API key).  `CREATE SECRET [IF NOT EXISTS] name VALUE '…'`
seals the value with ChaCha20-Poly1305 (`nucleus_crypto::aead`, RFC
8439) under the *keyring's* key — a random nonce, the secret's name
authenticated beside it, the key's id (8 bytes of a hash of it) kept so
another key is refused by name, not by garbage; `… FROM ENV 'VAR'`
names an environment variable of the server's process, read when the
secret is used — only an administrator makes one.  `ALTER SECRET name
VALUE '…' | FROM ENV '…'` replaces it; `DROP SECRET [IF EXISTS] name`
drops it with its grants.  Making one needs `CREATE` on the database;
its maker owns it (changes and drops it; a role that owns one is not
dropped, a renamed one keeps it); `GRANT USAGE ON SECRET name TO role`
lets a role use it — name it in a model (E2b) — and see it in
`nc_catalog.secrets` (`name`, `owner`, `source` `value` or `env`, `env`,
`created`, `changed`; never a value).  The keyring (`secrets::Keyring`,
set for the process by `secrets::set_keyring`) is the server's
`--secrets-key FILE` (`nucleus_server::keyfile::KeyFile`: 64 hex
digits, made with mode 0600 when the first secret with a value is;
default `<dir>.secrets-key` beside the data directory), so a copy of
the directory or a backup holds no secret in the clear — the key file
must be kept and backed up apart.  Without a keyring a secret with a
value is refused (`55000`); a damaged record does not open (`22000`).
Values are `secrets::Revealed`, which prints as `***`; a statement that
names a secret or a password is kept in the audit log and the server's
query log with its strings as `'***'` (`statements::redacted`), and a
mistyped `CREATE SECRET k 'v'` is a syntax error that does not echo the
string.

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
`login_lock_time` (`15 minutes`), `work_mem` (`64MB`, §8.5), `batch_size`
(`1024`, 0 – 65536, §8.5), `parallel_workers` (`0`, automatic; up to 256,
§8.5), `model_urls` (empty; URL prefixes a model not an
administrator's may call, §8.23); a duration
is milliseconds, `5s`, `100ms`, `2min`, `1h`, `1d` or an interval; a
memory size kB, or with `B`, `kB`, `MB`, `GB`, `TB` (at least 64 kB).  *Passwords*: at least
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
through (the SQLSTATE if not); a statement naming a secret or a
password with its strings as `'***'`.  It is written in the transaction of what
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
inferred, or given; the description kept by the statement's text in the
session's cache, §8.2), Bind (text or binary parameters; the statement
described again against the catalog as it is — from the cache while the
catalog holds — and refused with `0A000` "cached plan must not change
result type" when its result columns changed since Parse, as PostgreSQL
refuses it), Describe (statement
and portal), Execute (streamed as produced; with a row limit the result
is kept and sent a part at a time, `PortalSuspended`), Sync, Close,
Flush; after an error, messages are skipped until Sync.  Types on the wire:
`bool` 16, `int8` 20 (`int2`/`int4` accepted), `float8` 701 (`float4`
accepted), `text` 25 (`varchar`, `bpchar`, `name` accepted), `numeric`
1700, `date` 1082, `timestamp` 1114, `timestamptz` 1184, `interval` 1186,
and their arrays, `uuid` 2950 (binary: its 16 bytes), `vector` 13201
(`pg_type` lists it — pgvector's oid is per database, its clients look it
up with `to_regtype('vector')` — text `[1,2,3]`, binary pgvector's: the
dimension count and an unused `int16`, then big-endian `float4`s; psycopg
with pgvector's `register_vector` works both ways), `sparsevec` 13203
(listed likewise; text `{1:0.5}/10` as pgvector's, binary pgvector's:
the dimensions, the non-zero count and an unused `int32`, then the
0-based positions and the `float4` values, big-endian — positions out of
order are put in order); other registered types as `text`; text and binary formats for all of them (binary numeric
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
whole specification; `nucleus-wire` (`crates/wire`) is its one Rust
definition — versions, message kinds, value tags, frames, a value's
encoding and decoding, the columnar layout — which the server and the
Rust driver each map their own values onto.  The protocol's encoding is
its own: the server no longer writes values in the storage's row
encoding (whose tags 0–10 it once shared), so either can change
without the other.

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
| `H` Hello | client → server, first | `u16` version (7; 1–6 still accepted), string user, string database, string password (version 4: empty — it is proved, not sent; before 4: the password, empty for none), string client name |
| `A` Authentication | server → client | string: the mechanisms, space-separated (`SCRAM-SHA-256`; over TLS with a channel binding `SCRAM-SHA-256-PLUS SCRAM-SHA-256`), then SCRAM's server-first and server-final messages |
| `a` Authentication response | client → server | string: SCRAM's client-first, then client-final message — or OAUTHBEARER's initial response, or PLAIN's |
| `W` Welcome | server → client | `u16` version (the lower of the client's and the server's), string server (`nucleus 0.1.0`), `u32` session id |
| `Q` Query | client → server | string SQL, `u8` result format (0 typed, 1 text, 2 columnar — version 7), version 6: `u32` fetch size (rows per batch; 0: the whole result at once), `u16` parameter count, the parameters as values |
| `C` Columns | server → client | `u16` count, then per column: string name, type |
| `D` Row | server → client | one value per column |
| `B` Batch | server → client | version 7, result format 2: rows as a buffer per column (below); client → server during `COPY`: typed rows in the same layout |
| `I` Copy in | server → client | version 7: a `COPY … FROM STDIN` takes data — `u16` count, then per column: string name, type (as in `C`) |
| `d` Copy data | client → server | bytes of the `COPY`'s text or CSV, any split |
| `e` Copy end | client → server | empty: the data is complete |
| `f` Copy fail | client → server | string: why the client gives up (the statement fails, `57014`) |
| `K` Done | server → client | string command (`SELECT`, `INSERT`, `CREATE TABLE`, `BEGIN`, …), `u64` rows returned or affected (0 when it does not apply) |
| `E` Error | server → client | string SQLSTATE (5 characters), string message; version 5: string hint (what to do about it; empty: none) |
| `R` Ready | server → client | `u8` transaction status: 0 idle, 1 in a transaction block, 2 in a failed block |
| `S` Suspended | server → client | empty (version 6): a batch is sent; the server waits for `F` or `c` |
| `F` Fetch | client → server | `u32` the next batch's size (0: the rest at once), after `S` |
| `c` Close | client → server | empty, after `S`: no more rows — the statement ends there |
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
refer to them.

**Pipelining.**  A client may send Queries without waiting for the
replies to those before: the server reads and answers them in order,
each as if sent alone (one that fails does not skip the next; inside a
transaction block the next ones fail as the block has).  The server
writes its replies when it has read every whole message the client sent
— Ready is flushed only when no next message is waiting in its read
buffer — so a pipeline's replies go out together.  A client keeps what
it has sent and not had answered below what the sockets' buffers hold
(the Rust driver: 32 KiB, `PIPELINE_WINDOW`), reading the replies before
sending more: else both sides can wait on writes the other is not
reading.  A pipelined statement is not a `COPY FROM STDIN` (the next
Query would be read as its data, `08P01`) nor fetched in batches.

**Streaming** (version 6).  A query's rows are sent as the executor
produces them, so a result never has to fit in the server's memory, and
an error can come after some rows (Error then ends the statement, as
above).  With a fetch size *n* > 0 (one statement only, else `42601`), the
server sends *n* rows, then `S`, and waits: `F` asks for the next batch
(of its size; 0 for the rest), `c` ends the statement there — Done counts
the rows sent — and `X` closes the connection; anything else is `08P01`
and the connection closes.  A result of exactly *n* rows ends without
`S`.  The statement holds its snapshot while it waits, and the time spent
waiting does not count toward its `statement_timeout`.  While the client
handles a batch the server makes the next: the rows after an `S` are
produced and held — up to a batch of the size last asked for — and sent
when `F` comes, or dropped on `c` (Done counts the rows sent, never those
dropped).  So an error in a row read ahead ends the statement with its
Error even when the client then sends `c`, as a sort's would have before
any row was sent.  Rows of other
statements (`RETURNING`, `SHOW`, `EXPLAIN`) are sent once the statement has
committed, so one retried after a conflict sends them once.

**Columnar results** (version 7, result format 2).  Instead of a `D` per
row, `B` batches of rows, a buffer per column, so a driver can hand a
column over without building a value per cell (as a numpy or Arrow
array).  A batch holds the fetch size's rows (the server's choice, 16384,
for 0), and with a fetch size `S` follows each batch as above; a result
without rows has none.  Its body: `u32` rows *n*, then per column a `u8`
(1: a validity bitmap follows; 0: no NULLs), the bitmap (*n* bits, least
significant first, set for a value), and the data; every bitmap, data
and offsets buffer starts at a multiple of 8 bytes from the start of the
body (zero padding before it).  The data by column type:

| Types | Data |
|-------|------|
| boolean | a `u8` (0, 1) each |
| smallint · integer, date · real | `i16` · `i32` · `f32` each (date: days since 2000-01-01) |
| bigint · timestamp, timestamptz, time · double | `i64` · `i64` (µs) · `f64` each |
| interval | 16 bytes each: `i32` months, `i32` days, `i64` µs |
| uuid | its 16 bytes each |
| vector | variable width: its components, little-endian `f32`s (4-byte aligned, read in place as floats) |
| numeric | 17 bytes each: `i128` mantissa, `u8` scale |
| anything else | variable width: *n* + 1 `u32` offsets, then the bytes value *i* takes from offset *i* to *i* + 1 — UTF-8 for text, `char(n)`, enums and unknown types; the bytes of a `bytea` and a registered type's binary form; family, mask length and address for `inet` and `cidr`; an array as a value (tag 10, as in rows) |

A NULL's slot is zeros, or empty.

A query the vectorized executor runs (a scan with filters and
projections, §8.5) hands its batches to the result as columns — the
`Sink::takes_batches` / `Sink::batch` hook, `nucleus_sql::vector::batches`
— and the server copies their typed vectors into the buffers without a
value per cell (`columnar::Builder`; the same bytes as from rows, which
other queries still give).  A million rows of three numeric columns
reach numpy in 92 ms from a row table and 51 ms from a column table (262
and 241 ms through rows).  The test kit's scripts run once with their
results taken this way.

**COPY FROM STDIN** (version 7).  The statement answers `I` with the
columns it takes; the client sends `d` (text or CSV bytes, split
anyhow) or `B` (batches of rows in the columnar layout above, of the
columns `I` named: each value of its column's type) messages, then `e` —
or `f` to give up — and the statement ends as any (`K COPY n`, or `E`).
If it fails before `e`, the server reads and drops the client's data up
to `e` or `f` before Error.  A query without data to send (a plain
Query) answers `I` with `f`.  Transactions are SQL (`BEGIN`, `COMMIT`, `ROLLBACK`), as in
§8.7; outside a block every statement commits on its own.

**Versions.**  2 adds registered types (`uuid`, a module's type): tag 11
below.  3 adds tags 12–20, §M14.26's types.  4 proves the password by
SCRAM-SHA-256 instead of sending it (§M14.25).  5 adds the hint to `E`
(§8.14).  6 streams results and fetches them in batches (`Q`'s fetch
size, `S`, `F`, `c`; §M14.11).  7 adds the columnar result format (`B`)
and `COPY FROM STDIN` (`I`, `d`, `e`, `f`).  An older client gets what
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
| 11 | a registered type (version 2) | string type name, `u32` length, the value's binary (canonical) form — for `uuid` its 16 bytes, for `vector` its components as little-endian `f32`s, for `sparsevec` its dimensions and non-zero count, positions and values (§8.1) |
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
the server does not know is refused (`42704`); one the type's check refuses is refused with
the check's SQLSTATE (`22000` for a sparse vector's position past its dimensions), and one
the check accepts is kept as it reads back — a sparse vector's positions in order, its zeros
dropped — so equal values have equal bytes.  With result format 1 (text) every non-NULL value is
sent as text (tag 4), rendered as `ncsql` prints it.  Parameters are
converted to the type their place needs (a text parameter is parsed, as an
untyped literal would be).

## 11. Drivers (`drivers/`)

**Rust:** `nucleus-client` (`crates/client`, no dependencies).
`Client::connect(Config)` (defaults: `127.0.0.1:20035`, user, password and
database `nucleus`), `execute(sql, params) -> Reply` (columns, rows,
command, count), `rows(sql, params)`, `query(sql, params, Format)` for
several statements or text results, `cursor(sql, params, fetch)` — an
iterator over a result as it arrives, `fetch` rows at a time (0: 1000);
`columns()`, `command()`, `close()`, and dropped early it stops the
statement — `batches(sql, params, rows)`, the same in columnar batches
(`Batch`: `rows()`, `column(i)` — a `ColumnBatch` with its validity
bitmap and `ColumnData`, typed vectors `I32`, `I64`, `F64`, …, or offsets
and bytes; `i32s()`, `i64s()`, `f64s()`, `str(row)`, `is_null(row)` —
and `value(row, col)`), `copy_in(sql, reader)` (`COPY … FROM STDIN`
with text or CSV from any `Read`) and `copy_rows(sql, rows)` (rows sent
as columnar batches, each value fitted to its column's type; a value
that does not fit is `Error::Value`), `pipeline(&[(sql, params)])` —
statements sent without waiting for each reply (§10, *Pipelining*), each
one's result or error in its place —, `status()`, `close()`.  `Value` covers
every nucleus type (`SmallInt(i16)`, `Integer(i32)`, `Int(i64)`,
`Real(f32)`, `Float(f64)`, numeric as mantissa and scale, `Bytes`,
dates and timestamps as days and µs since 2000-01-01, `Time` as µs since
midnight, intervals, `Inet` / `Cidr { addr: IpAddr, bits }`, arrays,
`Uuid([u8; 16])`, `Vector(Vec<f32>)` (also from `Vec<f32>` and
`&[f32]`), `SparseVector { dims, indices, values }` (positions from 0,
ascending; every driver has a type of its own for it), `Other {
type_name, bytes }` for another registered type; `char(n)` and enum values as text, their columns typed `Char` and
`Enum(name)`) with `From` conversions for parameters.  Every driver
speaks protocol version 7 — results read as they arrive, a batch at a
time (Rust `Client::cursor`, Python `Connection.stream` and a lazy DB-API
cursor, JavaScript `Client.stream` with `for await`, Java
`NucleusClient.cursor`, C `nc_cursor_open` / `nc_cursor_next`, C++
`Connection::cursor`; closing one early, or the next query, stops the
statement), and columnar, a buffer per column read in place (`batches`
in each: Rust typed vectors; Python numpy arrays — `to_numpy()`, fixed
widths without a copy, NULLs masked, a `vector` column an `(n, d)`
`float32` array when its vectors share a dimension — Arrow arrays
(`vector` as `fixed_size_list<float32>`), and whole results
as `numpy`, `arrow`, `pandas`, `polars`; JavaScript typed arrays; Java
`IntBuffer`, `LongBuffer`, `DoubleBuffer` views; C and C++ pointers into
the batch), and bulk load through `COPY … FROM STDIN` (§8.7): text or
CSV in each (Rust `copy_in`, Python `Connection.copy`, JavaScript
`Client.copy`, Java `copy`, C `nc_copy` / `nc_copy_start` /
`nc_copy_write` / `nc_copy_end`, C++ `Connection::copy`) and rows or
columns as columnar batches (Rust `copy_rows`; Python
`Connection.load` — a pandas DataFrame, a pyarrow Table, a dict of
columns or rows, numpy columns without a Python object per value;
JavaScript `Client.load` — typed arrays sent as they are; Java `load` —
primitive arrays likewise), pipelines (§10, *Pipelining*: Rust
`Client::pipeline`, Python `Connection.pipeline`, JavaScript
`Client.pipeline`, Java `pipeline`, C `nc_pipeline`, C++
`Connection::pipeline` — each statement's result or error in its place)
and a statement for many rows of parameters (`execute_many`; JavaScript
and Java `executeMany`; Python's DB-API `executemany`): pipelined, all or
none — outside a transaction block in one of their own, so one commit
for all — errors with their hint (`ServerError::hint`,
Python's `.hint`, JavaScript's `hint`, Java's `hint()`, C's
`nc_error.hint`, C++'s `hint()`) — and logs in by SCRAM-SHA-256 (the C driver with
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
`nc_vector`, `nc_sparsevec`, `nc_ext`, `nc_smallint`, `nc_integer`, `nc_real`, `nc_bytea`, `nc_time`,
`nc_inet`, `nc_cidr`, …), `nc_uuid_parse`, `nc_inet_parse`,
`nc_result_free`, `nc_close`.  `uuid` is `NC_UUID` (an `nc_uuid` of 16
bytes), `vector` `NC_VECTOR` (`as.vector`: a `float` pointer and the
dimensions; in a batch the column's bytes are the floats), `sparsevec`
`NC_SPARSEVEC` (`as.sparse`: the dimensions, the positions from 0 and
the values, their count), another registered type `NC_EXT` (name and bytes;
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
seven wrappers, so an `int` literal stays a bigint parameter —, and
`Vector`, a `std::vector<float>`, and `SparseVector` — dimensions,
positions from 0, values).

**Python:** `drivers/python`, package `nucleus` (pure Python 3.8+).
`nucleus.connect()`, `conn.execute(sql, params)` → `Result` (`rows`,
`records()`, `columns`, `command`, `count`), `conn.query` for several
statements, `with conn.transaction():`, DB-API cursors; values as `bool`,
`int`, `float` (a real by its shortest digits), `Decimal`, `str`, `bytes`,
`date`, `time` (24:00:00 as `time.max`), `datetime` (naive / UTC),
`Interval`, `ipaddress` interfaces (inet) and networks (cidr),
`uuid.UUID`, `list`, a vector as an `array('f')` (sent from one or from
a one-dimensional float32 numpy array), `nucleus.SparseVector` (`dims`,
`indices` from 0, `values`; `SparseVector.of(dict, dims)`), `nucleus.Ext`
(another registered type); errors are `nucleus.Error` subclasses by SQLSTATE class
(`IntegrityError` for 23, `ProgrammingError` for 42, …).

**JavaScript / TypeScript:** `drivers/js`, package `nucleus-db` (Node;
TypeScript source that Node 22.6+ runs directly, `npm run build` for
`dist/`).  `await connect()`, `query(sql, params)` → `Result`, `queryAll`,
`transaction(fn)`; queries on one connection are queued.  Values: integers
and reals as `number` (`bigint` beyond 2^53; a real by its shortest
digits), `Decimal` (exact, with `parse` / `toString`), `Buffer` (bytea),
`Date` for dates and timestamps (UTC; millisecond precision when read),
`Time`, `Interval`, `Inet` / `Cidr`, uuid as its canonical string, a
vector as a `Float32Array`, `SparseVector` (`dims`, `indices` from 0,
`values`; `SparseVector.of(map, dims)`), `Ext`
(another registered type), arrays, `null`; errors are `NucleusError` with `code`.

**Java:** `drivers/java`, package `io.github.davidsarosi92.nucleus` (Java
17+, `javac` or Maven).  `NucleusClient.connect(new Config())`,
`query(sql, params...)` → `Result` (`rows()`, `records()`), `queryAll`,
`transaction(fn)`, `NucleusException` with `code()`; values as `Boolean`,
`Short`, `Integer`, `Long`, `Float`, `Double`, `BigDecimal`, `String`,
`byte[]`, `LocalDate`, `LocalTime`, `LocalDateTime`, `OffsetDateTime`
(UTC), `Interval`, `Inet` / `Cidr` (over `InetAddress`), `UUID`,
`float[]` (a vector; a column of them in `load` as `float[][]`),
`SparseVector` (dimensions, positions from 0, values;
`SparseVector.of(map, dims)`), `Ext` (another registered type), `List`.

**In the process (the embedded edition, §M14.23).**  `editions/embedded`
(`nucleus-embedded`) is the engine as a library: `Database::open(path,
options)` opens a database in directory `path` (made if missing) or, for
`":memory:"`, a new one on `MemoryPlatform` (§3.6); `options` is
`"cache=64MB; sync=full; wal=minimal"` (`cache`, `sync` `ordered` /
`full`, `wal` `minimal` / `full`; in memory the page cache keeps the
file's default, 128 MiB, and checkpoints come after 16 MiB of WAL).  It
runs what `nucleus server` runs beside the sessions — the maintenance
thread (`nucleus_server::maintain_until`) and the embedding columns'
worker — and `connect()` starts a session: the nucleus protocol's own
server (`handle_native_stream`, with `Trust`: no login, the session is
the user its Hello names; the directory's permissions guard the data)
on a thread of its own, its bytes through two pipes in the process
(each holds 4 MiB before its writer waits; a reader spins 50 µs before
it sleeps).  So every driver's protocol code runs unchanged over it:
one API for a server and for a database in the process.  A directory
this process has open already is shared (`open` again returns it; the
last `close` closes it); another process's is refused by its lock.
`close` ends the sessions (each after its statement), stops the
background threads (`Stop`, `embedding::stop`), checkpoints and frees
the directory.  The registry is the engine's plus the `embeddings` and
`local-models` modules (features of the same names, on by default).
The C API (`include/nucleus_embedded.h`): `nce_open(path, options,
&error)`, `nce_connect(db, &error)`, `nce_send(c, buf, len)`,
`nce_recv(c, buf, cap, timeout_ms)` (bytes; 0 at the end; -1 when the
time ran out), `nce_end(c)` (ends the session without freeing it — the
one call another thread may make on a connection while one reads),
`nce_disconnect(c)`, `nce_close(db, &error)`, `nce_free_error`,
`nce_version`.  Rust programs use `Database` directly, or
`nucleus_client::Client::over(Box::new(db.connect()?), config)`.

Python: `nucleus.connect("file:app.db")` (options as parameters,
`file:app.db?cache=64MB`, or `options=`), `nucleus.connect(":memory:")`
or `nucleus.open(path, options)` — a `Connection` that closes its
database with it — and `nucleus.Database(path, options)` with
`connect(user=…)` for several sessions on one (a `:memory:` database is
one per `Database`).  The library is loaded with `ctypes` (each call
releases the GIL) from `NUCLEUS_EMBEDDED_LIB` or beside the package;
`drivers/python/build_wheel.py <library>` makes the wheel for this
platform, `py3-none-<platform>`, one for every Python 3.  JavaScript:
`await connect("file:app.db")`, `connect(":memory:")`, or
`Database.open(path, options)` with `connect({ user })` and `close()`;
the session is a `Duplex` over the Node-API addon `native/addon.c`
(linked with the engine into one `nucleus.node` by `native/build.sh`),
which reads each session on a thread of its own and hands its bytes to
the event loop through a thread-safe function (16 chunks queued at
most: then the session waits, a socket's backpressure).  The addon is
looked for in `NUCLEUS_EMBEDDED_ADDON`, the package
`nucleus-db-<platform>-<arch>`, then `native/<platform>-<arch>/`.  Bun
and Deno load Node-API addons too (CI runs the example under both).  Both drivers also take
`"nucleus://user:password@host:port/database?tls=true&tls_ca=…"` for a
server, so a program moves between the two by its connection string
alone (`examples/python/anywhere.py`, `examples/js/anywhere.ts`).

**In the browser (the edition in WebAssembly, §M14.23).**  The embedded
edition builds for `wasm32-wasip1` without the model modules
(`drivers/js/wasm/build.sh` → `drivers/js/wasm/nucleus.wasm`).  There are
no threads there: a statement's workers are 1 (`budget::workers`), the
engine starts no background thread, and sessions are *stepped* —
`nucleus_server::Exchange`, the nucleus protocol's server driven by the
bytes given to it (`Database::exchange`; C `nce_exchange`, `nce_feed`,
`nce_exchange_ended`, `nce_exchange_free`): each `feed` runs the whole
messages it has and returns the reply.  Nothing can wait mid-statement,
so in a stepped session a `COPY`'s data comes with the `COPY` (one
without fails, `57014`; data after it is dropped), a fetch's batches
follow one another without an `S` to answer, and one statement's reply
holds at most 256 MB (`54000`).  A directory is kept in a pool of slots
(§3.7) that the host provides as imports (`nucleus_slots`), one pool per
instance; besides those the module imports fifteen WASI functions
(clocks, randomness, the console; no files).  The JavaScript driver's
browser entry, `drivers/js/src/browser.ts` (the package's `browser`
export), has the same `connect` and `Client`: `connect("file:app.db")`
keeps the database in OPFS (directory `.nucleus` of the origin's,
`OpfsSlots`), `connect(":memory:")` in memory; `nucleus://` is refused.
In a page the engine runs in a module worker (`worker.js`), reached by
messages; in a worker, and in Node, Bun and Deno, on the calling thread
(`LocalBackend`; in Node the pool is a directory of files, `NUCLEUS_POOL`
or `.nucleus`).  Its socket (`Session`) sends what the client wrote in
one turn of the event loop as one `feed`, and the client is *eager*
there: `copy` and `load` send the data with the `COPY` (`load` learns
the columns' types from a `LIMIT 0` query first), and no `F` answers a
batch.  The driver's protocol code has no Node imports (`core.ts`;
`bytes.ts` is Node's `Buffer` or, without one, a `Uint8Array` with the
part of its interface the driver uses; SCRAM is WebCrypto's), so
`index.ts` (Node's sockets, the addon) and `browser.ts` share it.

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
them all; the CI job `drivers` does so on Linux.  `python-embedded`
and `js-embedded` run the Python and JavaScript suites again on a
database in their own process (`NUCLEUS_URL=file:…`; the login tests
skipped) and the `anywhere` examples in a directory, in memory and
against the server; `editions/embedded/tests` runs the slt suite there
(each script on a new `:memory:` database through `Client::over`).  `scripts/psql-smoke.sh`
checks `psql` with and without TLS.

## 12. Change log

- **2026-11-13 — §M14.23 step 2, the browser (WASM):** the engine on
  `wasm32-wasip1` (CI's `wasm` job: platform conformance, storage,
  transactions, SQL and slt under Node's WASI, `scripts/wasi-run.mjs`);
  `nucleus_platform::{temp_dir, process_id}`; `nucleus_platform::slots`
  (`Slots`, `SlotFs`, `SlotPlatform`, `MemorySlots`);
  `nucleus_server::Exchange`; `Database::exchange` and the C API's
  `nce_exchange`, `nce_feed`, `nce_exchange_ended`, `nce_exchange_free`,
  `nce_alloc`, `nce_dealloc`, `nce_pool_free`; JavaScript split into
  `core.ts`, `index.ts` (Node) and `browser.ts` (with `wasm.ts`,
  `worker.ts`, `opfs.ts`, `node-slots.ts`, `bytes.ts`), SCRAM on
  WebCrypto; `examples/browser/`; `driver-tests.sh js-wasm` with
  `drivers/js/test/browser.mjs` (headless Chrome).  `COPY`'s text format
  checks each field as UTF-8 (a fuzz finding).

- **2026-11-13 — §M14.23 step 1, the embedded edition in Python and
  JavaScript:** `editions/embedded` (`nucleus-embedded`: `Database`,
  `Connection`, the C API `nucleus_embedded.h`); `nucleus_platform::
  memory` (`MemoryFs`, `MemoryPlatform`); `nucleus_server::Stop` and
  `maintain_until`; `nucleus_sql::embedding::stop`;
  `nucleus_client::Client::over`; Python `nucleus.open`, `Database`,
  connection strings in `connect`, `build_wheel.py`; JavaScript
  `Database`, `connect(string, options)`, `native/addon.c` and
  `build.sh`; `driver-tests.sh` `python-embedded` and `js-embedded`;
  `examples/*/anywhere`.  D15 (versioning) decided.

- **2026-11-13 — §M14.29 S2, fuzzing:** `fuzz/` — ten `cargo fuzz`
  targets (§2), seeds (`fuzz/seeds.py`), CI's `fuzz` job and the fuzz
  crate in the lint job.  The front ends serve any byte stream
  (`nucleus_server::handle_native_stream`,
  `nucleus_pg_compat::handle_stream`).  Fixed: `websearch_to_tsquery`
  on a multi-byte character after a word (a panic); Bind's negative
  counts (a panic, now `08P01`); a columnar load's row count past its
  batch (an allocation, now `08P01`); the buffer pool's flush made every
  frame (the first checkpoint allocated the whole cache;
  `BufferPool::frames_made`); a B-tree node with keys out of order
  (`Node::decode` and sibling merges refuse it, `Corrupt`, instead of a
  panic).  `nucleus-http`'s `Request::total` bounds a whole exchange;
  `models::Request::time_left` and `ext::time_left` give a call the
  statement's remaining time (embedding providers stop by it); a local
  model's file is at most a quarter of the machine's memory.

- **2026-11-13 — model URLs on a list:** database setting `model_urls`,
  `ModelProvider::urls`, `models::split_url` / `url_under`; checked at
  `CREATE MODEL` (not for administrators) and at every call by the
  owner's standing.

- **2026-11-13 — §M14.22, a review's fixes:** an embedding column's
  expression may not call `embed` (`0A000`); a row whose text fails
  fails its job only; a round's panic is an error; the server's worker
  woken after the writer commits; `ADD COLUMN … GENERATED BY EMBEDDING`,
  `SET MODEL`, `swap` and `CREATE TABLE … CHUNKS OF` read validated, and
  `swap` gives a row without a next vector a job; `DROP MODEL` refused
  under a chunk table, whose writers need no `USAGE` on its model
  (`models::count_tokens_unchecked`); `ATTACH PARTITION` refuses chunked
  tables; batches answered beside a failed one kept; local models'
  `config.json` numbers bounded, tensor sizes checked, Unigram pieces at
  most 1 KiB, redirects only to `https://`; `nucleus-http` refuses URLs
  with spaces or control characters and chunk sizes that would wrap.

- **2026-11-13 — §M14.22 E8b, the end-to-end check:**
  `bench/embed/beir.py` (BEIR documents to ranked chunks by a local
  model in a server killed twice; the results in
  `docs/benchmarks.md`).  §M14.22 is done.

- **2026-11-13 — §M14.22 E8, what model calls cost:** model options
  `batch_tokens`, `requests_per_minute`, `tokens_per_minute`, `parallel`
  (model record version 3); token buckets per database and model;
  `Request::report_tokens`; `budget::ModelCost` charged per statement
  and its workers; `EXPLAIN ANALYZE`'s `Models:` line;
  `nc_catalog.statements` `model_*` columns; `nc_catalog.models`
  `tokens`, `throttled_ms` and the limits; `embed_unchecked` takes
  whether to wait.

- **2026-11-12 — §M14.22 E7, chunk tables:** `CREATE TABLE … CHUNKS OF
  t (c) BY … SIZE … [OVERLAP …] [USING m]` (`chunk_tables`), table
  record version 16 (`ChunkSpec`), `nc_catalog.chunk_tables`; writes to a
  table rechunk its chunk tables in the same transaction; `DROP TABLE …
  CASCADE` drops a table's chunk tables.

- **2026-11-12 — §M14.22 E6, chunks:** `chunks(text, by, size [,
  overlap [, model]])` (`chunks`), UAX #29 sentences (`text::sentences`,
  `scripts/gen-sentence-breaks.py`), `ModelProvider::count_tokens`
  (`local` counts by its tokenizer), `models::count_tokens`; table
  functions may return several columns.

- **2026-11-12 — §M14.22 E5, a column moved to another model:** `ALTER
  TABLE … ALTER COLUMN … SET MODEL`; next-model jobs (column bit
  `0x8000`), their vectors at `00 18`, the swap transaction; table record
  keeps the model's version and next model; `nc_catalog.columns`
  `model_version`, `next_model`; `nc_catalog.embedding_jobs.kind`.
- **2026-11-12 — §M14.22 E4b, embedding columns:** `GENERATED BY
  EMBEDDING (expr) USING m`; the queue at `00 17`, `embedding::work`,
  `after_statement`, `serve` (started by the server), `drain`,
  `rounds_by_caller`; backoff with full jitter, `Retry-After`
  (`models::retry_hint`, `retry_after`), a circuit a model;
  `embedding.pending`; `embed(q)` by a column's model;
  `nc_catalog.embedding_jobs`; `Kv::sleep`.
- **2026-11-12 — §M14.22 E4a, stored generated columns:** `GENERATED
  ALWAYS AS (expr) STORED`, `428C9`; table record version 15; pg-compat's
  `attgenerated`, `pg_attrdef`, `information_schema` generation columns.
- **2026-11-12 — §M14.22 E3c, local models in the database:** the
  `local` provider (`huggingface`, `path`, `endpoint`, `pooling`,
  `threads`, `batch`); model files kept by content at `00 16`;
  `bench/embed/speed.py` against fastembed.

- **2026-11-12 — §M14.22 E3b, the encoder:** `nucleus-kernels` gains an
  f32 matrix product (`PackedB`, `matmul`), `gelu`, `softmax_rows`,
  `layer_norm`, `add_assign`, `erf`, `exp`, reached by modules as
  `ext::kernels`; its `Avx2` level requires FMA.  `nucleus-local-models`
  runs BERT and XLM-RoBERTa encoders from safetensors (`LocalModel`,
  `Encoder`), packed batches without padding, the same vector on any
  threads; `bench/embed/encoder.py` compares with ONNX Runtime.
- **2026-11-12 — §M14.22 E3a, the tokenizers:** new module
  `nucleus-local-models` with WordPiece and Unigram tokenizers read from
  `tokenizer.json`, the same ids as Hugging Face's `tokenizers` on
  2.5 million texts; `scripts/gen-tokenizer-tables.py` (Unicode 8
  classes for BERT, Unicode 16 grapheme clusters);
  `bench/embed/models.py`, `bench/embed/tokens.py`.
- **2026-11-12 — §M14.22 E2b, models and `embed()`:** `CREATE | DROP
  MODEL … USING provider WITH (…)`, providers registered
  (`Registry::add_model_provider`), `hashing` built in; `embed(text, m [,
  'query' | 'document'])` with the model's prefixes, `USAGE` on models,
  a content-addressed cache, batches through `call_batch`;
  `nc_catalog.models`, `nc_catalog.model_providers`.  New modules:
  `nucleus-http` (the oidc module's client, now shared: any method,
  headers, status, `Retry-After`) and `nucleus-embeddings` (`openai`,
  `cohere`, `voyage`), in the `nucleus` tool; `nucleus sql <dir>` seals
  secrets with the server's key file too.
- **2026-11-12 — §M14.22 E2a, secrets:** `CREATE | ALTER | DROP SECRET`,
  sealed by ChaCha20-Poly1305 (`nucleus_crypto::aead`, RFC 8439's
  vectors) under a keyring's key outside the data directory (`nucleus
  server --secrets-key`, `KeyFile`), or `FROM ENV`; `GRANT USAGE ON
  SECRET`; `nc_catalog.secrets`; `tests/secrets.rs`.  The audit log and
  the query log mask the strings of a statement naming a secret or a
  password (`statements::redacted`) — a password was kept as written
  before.  `SECURITY.md`.
- **2026-11-11 — §M14.21 T8c, every kind of table and the wire:** the
  text and sparse indexes on `column`, `row,column` and partitioned
  tables — BM25's statistics a partitioned table's partitions' together
  (`text::index::stats`), a ranking's values read from each partition's
  index (`optimizer::ranked`), `rrf` and `convex` over a partitioned
  table's `Append` (`hybrid::fused`); `tests/text_tables.rs`.  pg-compat's
  `tsvector` and `tsquery` as PostgreSQL's types on its wire (oids 3614,
  3615, `pg_type`, `format_type`), their binary forms both ways
  (`tsearch::vector_send` / `vector_recv` / `query_send` /
  `query_recv`), `tsvectorsend` and `tsquerysend`; `<N>` up to 16,384
  and the clean-up's distance unchecked, as PostgreSQL's;
  `tests/wire.rs`'s `text_search_types_through_a_real_driver` and
  `…_through_nucleus_protocol`; `bench/text/beir.py --layout`.
- **2026-11-11 — CI fixes:** `ext::text` and `ext::Expr` (pg-compat's
  text search named `nucleus_sql::text` and `::expr`, past the module
  boundary); `tests/vector.rs`'s few-groups spill runs until two workers
  take morsels (a single worker's groups fit: on a runner of few cores
  one may take them all).
- **2026-11-11 — §M14.21 T8b, PostgreSQL's ranking and headlines in
  pg-compat:** `ts_rank`, `ts_rank_cd` (`tsrank.rs`, with a port of
  PostgreSQL's `qsort`) and `ts_headline` (`tsheadline.rs`) as
  PostgreSQL computes them; the optimizer pass `postponed` (output
  expressions computed above `LIMIT`); `tests/tsearch.rs` with 2,700 more
  of PostgreSQL 18's answers, `tests/sort.rs`'s
  `the_output_is_computed_on_the_rows_kept`; `bench/text/tsearch.py
  --rank`.
- **2026-11-11 — §M14.21 T8a, PostgreSQL's text search in pg-compat:**
  `tsvector`, `tsquery`, the makers and `@@` (`modules/pg-compat`
  `tsearch.rs`), evaluated as PostgreSQL's `TS_execute`;
  `ScalarFunction::rewrite` (the binder binds the call it gives), by
  which `to_tsvector(cfg, col) @@ maker(cfg, 'q')` becomes the text
  index's `col @@ q'`; `tests/tsearch.rs` with PostgreSQL 18's answers
  (`bench/text/tsearch_pg.py`), `tests/slt/03_text_search.slt`;
  `bench/text/tsearch.py` (against PostgreSQL's GIN).
- **2026-11-10 — §M14.21 T7b, the sparse vector index:** `USING sparse`
  (`text/index/sparse.rs`) on the text index's storage with weighted
  documents (`Doc::weights`, `segment::weight_bits`,
  `Postings::push_weighted`, `Store::weighted`, `segment::merge`'s
  `weighted`), `Scorer::dot`; `<#>` ranked exactly within a margin;
  `bench/text/hybrid.py --sparse` (SPLADE, pgvector's sparse HNSW).
- **2026-11-10 — §M14.21 T7a, hybrid search:** `rrf(r1, r2, …)` and
  `convex(r1, r2, α)` (`hybrid.rs`; the optimizer pass `hybrid::fused`:
  each ranking's first 100 rows by its index, `UNION ALL`, grouped by the
  primary key, ranked or normalized over each list);
  `ScalarFunction::found` (`text_score`'s rows are `@@`'s);
  `bench/text/embed.py` (fastembed: bge-small-en-v1.5, SPLADE++) and
  `bench/text/hybrid.py` (BEIR: text, vector, `rrf`, `convex`).
- **2026-11-10 — §M14.21 T6, slop, highlights and snippets:**
  `"a b"~N` (`Node::Phrase`'s slop, `query::phrase` — Lucene's sloppy
  matcher and weight — for rows, segments, BM25 and highlights);
  `highlight(col @@ q [, start, stop])` and `snippet(col @@ q [, start,
  stop, ellipsis, words])` (`text/highlight.rs`, `text_highlight`,
  `text_snippet`) through `ScalarFunction::of_condition` /
  `bind_condition`; `bench/text/beir.py --snippets`.
- **2026-11-10 — §M14.21, `@@` estimated by the text index:**
  `IndexMethod::selectivity` and `index_method::selectivity`, asked by
  `Stats::sampled` before the kept sample; the text index's from its
  words' document counts (`query::share`).  An analyzed table's text
  query no longer analyzes the sample's texts at each planning (SciFact
  46 → 0.27 ms a query).  `59_text_score.slt` checks the estimates.
- **2026-11-10 — §M14.21 T5c, scores read from the ranking:**
  `IndexMethod::values` (the text index's BM25) and `RankAsk::values`;
  `MethodRank::value` — the ranked call, its value one more column past
  the table's (`Plan::width`, `Access::valued`); `optimizer::ranked`
  rewrites the projection's call to read it (one table's scan, not an
  `Append`); `exec` appends the call's value where the index did not
  rank (`method_rows` says whose rows they are); `prune` reads the call's
  columns for it; the text index pushes each row's score; `EXPLAIN`
  `… with values`; `tests/text_index.rs`, `59_text_score.slt` (§8.17,
  §8.19).
- **2026-11-10 — the index cache sized by the machine:**
  `nucleus_platform::real::physical_memory` (macOS `hw.memsize`; Linux
  `MemTotal` and the cgroup v2 / v1 limit; `None` elsewhere);
  `object_cache::budget` a quarter of it unless `--index-cache` sets it
  (1 GB where unknown), `nucleus_sql::index_cache`; the server's first
  line names it (§3.2, §9).
- **2026-11-10 — §M14.21 T5b, block-max MaxScore and a parallel
  build:** segments version 2 (`segment.rs`: a skip list per term in
  more than one block — last document, bytes, the (frequency, length)
  impacts no other pair beats; `Segment::blocks`, `Segment::block`;
  version 1 read); scores in fixed point (`score::fixed`, units of
  2⁻³², the same sum in any order); windowed MaxScore over the impacts
  (`store.rs`: `max_score`, `Cursor`, `Top`), `View::ranked(q, sc, n)`
  and `Ranked::complete`, `n` sized by the expected share and every
  match scored where it is a 64th of them, a second pass sized by the
  share seen, `every_match` term at a time; `CREATE INDEX … USING text`
  on the statement's workers (`TextIndex::build`, `Store::append_segments`,
  `Store::merge_all` / `merge_due`, `segment::merge` in runs: one segment
  up to 3 GB); the server's
  `--index-cache` (`object_cache`'s budget; by default a quarter of
  the memory the process may use, `nucleus_platform::real::physical_memory`);
  `bench/text/beir.py` loads MS MARCO in parts (`--split`, `--queries`,
  `--index-cache`); `tests/text_index.rs` `pruned_as_scored_in_full`
  (§8.19, §9).
- **2026-11-10 — §M14.21 T5b, found rows by a merge with the table's
  scan:** `KvChunks::seek` and `next_chunk_where` (keys a predicate
  takes, the rest passed by their keys, their values not read), the
  engine's `Scan::seek`, the B+tree's `Range::next_key_into` /
  `value_into`; `TableStorage::rows_at` (rows to a callback, the share
  measured kept by the caller), `exec::entries_at` (a lookup each or
  reading on, by the share wanted, measured as it goes; the scan opened
  only where it reads), `exec::rows_at` (runs on the workers);
  `exec::method_rows` reads the found rows a block at a time through it;
  the text index's rank still a lookup each (measured);
  `tests/text_index.rs` ties at every limit (§6, §8.17, §8.19).
- **2026-11-10 — §M14.21 T5a, one ranking for every method:**
  `IndexMethod::ranks` (with `nulls_first`), `rank_cost`, `rows`,
  `rank` (`RankAsk`, `RankCost`, `Keep`, `Among`; `Pks` moved to
  `index_method`), `rows_where` without its rank; `Access::Nearest`
  gone, `Access::Method { index, served, rank }` with `plan::Served`
  (`above`: a statement's condition over a row-level security barrier)
  and `MethodRank`'s `within`, `share`, `checks`, `partition`;
  `optimizer::ranked` in place of `nearest` and `method_ranks`,
  `method_filters` under a barrier; `exec::method_rows`, `ranked_rows`,
  `among`, `unserved`; HNSW's cost, `among_most` and partitions behind
  its `rank`; the text index ranks with filters, key filters and
  policies (`View::ranked`, `Ranked`), and its matches are what HNSW
  ranks among (`among ix: body @@ …`); `tests/text_index.rs` (filters
  and row-level security against the unranked order),
  `59_text_score.slt`, `bench/text/beir.py --filters` (§8.17–§8.19).
- **2026-11-09 — §M14.21 T5, BM25:** `score(col @@ q)` — Lucene's BM25
  over the index's statistics read once a statement (`text_stats` in an
  uncorrelated subquery, `text_score` a row), `k1` and `b` options;
  `ORDER BY score(...) DESC LIMIT k` ranked by the index from its
  postings (a bag of words term at a time), the same numbers as a row's;
  `IndexMethod::function`, `ranks`, `rows_where`'s `Rank`,
  `Access::Method`'s `rank`, `optimizer::method_ranks`; versions counted
  only where a tombstone names them, unions of many lists by a bitmap
  (`count(*)` of a FiQA query 3.2 → 1.4 ms); `59_text_score.slt`,
  rankings in `tests/text_index.rs`, `bench/text/beir.py` (§8.17,
  §8.19).
- **2026-11-09 — §M14.21 T3–T4, the text index:** `USING text (col)
  WITH (analyzer = …)` — deltas and tombstones, segments of bit-packed
  128-blocks, deleted objects, seals and logarithmic merges, exact
  matches and BM25 statistics in every snapshot (`text/index/`);
  `IndexMethod::operand`, `serves`, `rows_where`, `Access::Method`,
  `optimizer::method_filters`, `exec::method_rows`, the binder's
  `index_operand`; `object_cache` (shared with HNSW, with `object_id`);
  `tests/text_index.rs`, `tests/text_index_crash.rs`,
  `58_text_index.slt`; indexed queries in `bench/text/analyzers.py`
  (§8.17, §8.19).
- **2026-11-09 — §M14.21 T2, text queries:** `text @@ query` on rows —
  Lucene's syntax (words, phrases, `*` prefixes, `~` fuzzy terms, `+`,
  `-`, `AND`, `OR`, `NOT`, parentheses), parsed once into leaves and a
  boolean tree, a batch kernel; `Analyzer::normalize`; queries in
  `bench/text/analyzers.py` (§8.19).
- **2026-11-09 — §M14.21 T1, text analyzers:** `text::Analyzer`,
  registered by name (`Registry::add_analyzer`, `nc_catalog.text_analyzers`);
  UAX #29 words from generated Unicode 16 tables, Porter's stemmer
  (Lucene's), Snowball 3.0.1's stemmers for thirteen languages
  (generated Rust, vendored) with Snowball's stop words, a trigram
  analyzer; `tokenize(text [, analyzer])`; `bench/text/data.py` (§8.19).
- **2026-11-09 — §M14.12 V7, closing vectors:** vectors and sparse
  vectors stay in their row however long (`Type::stored_with_row`,
  `large::out_of_line_value`; `crates/sql/tests/large.rs`
  `vectors_stay_in_their_row`); exact searches held to the workers
  (`crates/sql/tests/vector.rs` `exact_vector_searches_run_on_the_workers`);
  `sparsevec` in every driver (Rust `Value::SparseVector`, C
  `NC_SPARSEVEC` / `nc_sparsevec`, C++ / Python / JavaScript / Java
  `SparseVector`) and on pg-compat (oid 13203, pgvector's binary form);
  a registered type's `ncp` parameter kept as its check reads it back
  (canonical), refused with the check's SQLSTATE; `ext::sparse`
  (§8.1, §8.12, §9, §10, §11).

- **2026-11-09 — §M14.12 V7 step 1, query vectors from rows:** a
  subquery that does not read the scanned row is a fixed query vector
  (`optimizer::fixed`, `expr::row_free`); `LATERAL` (`TableRef::Lateral`,
  `binder::item_after`, `binder::lateral_join`; functions in `FROM` read
  the items before them), run as an index nested loop's right side,
  also on worker threads (`exec::parallel_lookups` takes a sort and a
  limit above a scan and an index method's search;
  `parallel::SharedTxn` gives its transaction's shared reads and
  instance; `budget::workers` is 1 on a worker); `EXPLAIN` `Nested Loop
  (per row)`; `crates/testkit/slt/55_lateral.slt`;
  `bench/ann/related.py`; `bench/common.py` and `bench/ann/related.py`
  reach the server over `::1` with `NUCLEUS_BENCH_HOST` / `--host`, and
  `ncsql -h [addr]:port` takes an IPv6 address; the bench scripts remove
  their containers' volumes (§8.3, §8.4, §8.5, §8.18).

- **2026-11-07 — §M14.12, vectors against pgvector; V7 planned:**
  `bench/ann/pgvector_ref.py --quantize half|binary --rescore
  --no-inserts` (cosine data too; pgvector's `halfvec` and
  `binary_quantize` indexes) and `--server-efs` there and in
  `bench/ann/sql.py` (the server's own time by `EXPLAIN ANALYZE`);
  `docs/benchmarks.md`'s vectors summary; the types table, the HNSW
  syntax, `sparsevec` on pg-compat and in the drivers (§8.1, §8.18, §9,
  §10).  PLAN.md §M14.12 V7 recorded as the next step.

- **2026-11-07 — §M14.12 V6b, sparse vectors:** `sparsevec` /
  `sparsevec(n)` (`sparse.rs`: `Sparse`, `View`, `sparse::distance`);
  the vector distance functions take either type
  (`vectors::distance_function`); the law kit's generator;
  `crates/testkit/slt/54_sparsevec.slt`; `bench/ann/sparse.py` (§8.1).

- **2026-11-07 — §M14.12 V6a, quantized HNSW graphs:** `WITH (quantize
  = 'half' | 'int8' | 'binary', rescore = n)` (`graph::Codec`,
  `graph::Space` — `Params::metric` is now `space`; `hnsw::serves`
  gives the `rescore` too); the kernels `to_half`, `from_half`,
  `l2_squared_half`, `dot_half`, `l1_half`, `dot_u8`, `l1_u8`, `hamming`;
  `bench/ann/sql.py --quantize --rescore --no-inserts` (cosine datasets
  too), `bench/ann/data.py dbpedia-openai3-1536-100k` (§8.18).

- **2026-11-07 — §M14.12 V5, filtered vector search:** `ef_search`
  counts the rows that pass (`hnsw::Ask`, its `share`; the graph asked
  again as the share passing near the query needs); searches among a
  filter's B-tree range, exactly, from the vectors in memory
  (`Access::Nearest`'s `within`, `exec::among`, `hnsw::among_most`,
  `Index::candidates_among`, `ByRow` a hash map behind a read-write lock,
  `Pks` public); row-level security as the search's `checks`
  (`Access::Nearest`'s `checks`; `Access::for_each_expr`,
  `for_each_expr_mut`); `PARTITION BY (c, …)` on an HNSW index, a graph
  per value (`CreateIndex::partition_by`, the method's `partition_by`
  option, `hnsw::Id`'s `part`, `Access::Nearest`'s `partition`); a
  partitioned table's partitions searched each under the `Append`;
  `bench/ann/filtered.py` (§8.18).

- **2026-11-07 — §M14.12 V4b, HNSW searches while rows come in:**
  `Kv::vacuum_range` (`Engine::vacuum_range`, the vacuum's sweep in
  chunks, `sweep_chunk`) after each seal; a compaction apart from the
  folds (`Index::compaction`, `put_compaction`, `install`, `warm`;
  `Compaction`); the patches' levels merged by the logarithmic method;
  the upkeep only by writing sessions while rows come in
  (`IndexMethod::after_statement(kv, wrote)`); seal and fold sizes from
  the row costs the searches measure; the buffer pool's flushes a page
  at a time under the shard's read lock (§4.3, §6.1, §8.18).

- **2026-11-07 — §M14.12 V4b step 4, HNSW settled:** the upkeep after
  every statement outside a block (`IndexMethod::after_statement`,
  `index_method::after_statement`, renamed from `after_commit`), the rows
  left sealed noted until folded; a search reads about `k` rows, nearest
  first, passing over stale ones; `crates/sql/tests/hnsw_crash.rs`;
  `bench/ann/sql.py` (§8.18).
- **2026-11-07 — §M14.12 V4b step 3, nearest-neighbour searches in the
  planner:** `Access::Nearest` (`Access::secondary`), the optimizer's
  `nearest` pass, `exec::nearest`; `hnsw::search`, `nearest_rows`,
  `rows`, `serves`, the session setting `hnsw.ef_search`;
  `vectors::Metric::of_function` (§8.18).
- **2026-11-07 — §M14.12 V4b step 2, HNSW as an index method:** `hnsw`
  (`graph`, `store`, `cache`; `nearest`, `status`, `upkeep_now`),
  registered by the built-in module; `IndexMethod::after_commit` and
  `index_method::after_commit`, called by the session after each commit;
  `Kv::instance` / `KvTxn::instance` (§6.1, §8.18).
- **2026-11-07 — §M14.12 V4b step 1, index access methods:**
  `index_method` (`IndexMethod`: `define`, `write`, `build`, `check`;
  `Registry::add_index_method`); `CREATE INDEX … USING method (col
  opclass) WITH (…)`; the index record's method, operator classes and
  options (table record version 14); `nc_catalog.indexes.method`,
  `opclasses`, `options`; pg-compat's `indexdef` with them (§8.17).
- **2026-11-06 — §M14.27 S8b, `SUMMARIZE`:** the statement
  (`Statement::Summarize`, `summarize`), read-only and described; the
  MCP tool `summarize_table` (§8.5, *`SUMMARIZE`*; MCP in §6).
- **2026-11-06 — §M14.27 S8a, drift and tests:** `ks_test`, `auc`,
  `psi`, `js_divergence`, `chisq_test` (§8.5, *Drift and tests*);
  `quantile::Doubles` (the ordered sets' store for walks in order);
  `quantile::sort_keys`, an LSD radix sort for 8-byte keys (`mode` and
  the walks).
- **2026-11-06 — §M14.27 S7, sampling and splits:** `TABLESAMPLE
  BERNOULLI | SYSTEM (p) [REPEATABLE (seed)]` (a filter by the primary
  key's stable hash with `REPEATABLE`), `hash_unit`, `reservoir_sample`,
  `random()`; `ScalarFunction::parallel_safe`; `ANALYZE`'s Algorithm L reservoir moved to `sample::Reservoir` and
  shared (§8.5, *Sampling*).
- **2026-11-06 — §M14.27 S6, sketches:** `stablehash` (xxHash64 of
  the key encoding); `approx_count_distinct`, `hll_agg`, `hll_merge`,
  `hll_count` and the type `hll` (HyperLogLog++, Ertl's estimator);
  `approx_percentile` / `approx_quantile`, `ddsketch_agg`,
  `ddsketch_merge`, `ddsketch_quantile`, `ddsketch_count` and the type
  `ddsketch`; `approx_top_k`, `approx_top_k_counts` (§8.5, *Sketches*).
- **2026-11-06 — §M14.27 S5, ordered sets:** `percentile_cont`,
  `percentile_disc`, `mode` `WITHIN GROUP (ORDER BY …)`, `median`,
  `width_bucket`, `histogram` (§8.5, *Ordered sets*);
  `Aggregate::ordered_set` and `check_direct`; the grammar's `WITHIN
  GROUP`; exact selection over order-preserving 8-byte keys, combinable
  partial states, a radix-partitioned spill past `work_mem`; constant
  expressions evaluated once per batch in the vectorized executor (§8.5,
  *Batches*).
- **2026-11-06 — §M14.27 S4·sort, the sort and provided orders:**
  `sorter` — normalized-key records with an abbreviated prefix, chunks
  sorted stably in memory or in scratch runs with a sparse index, a
  merge split into key ranges on the workers — for every full sort
  (§8.5, *Memory*); a `Sort` a primary key's scan already gives left
  out, identity projections dropped; unfiltered chains on the workers
  and a `LIMIT`'s bound through projections and filters to the scan.
- **2026-11-06 — §M14.27 S4, window functions:** `OVER`, `WINDOW`,
  `ROWS` / `RANGE` / `GROUPS` frames with `EXCLUDE`, every aggregate as a
  window aggregate (a running state, small frames fed directly, a
  segment tree of partial states), the ranking and value functions
  (§8.5, *Window functions*); a `Sort` per window, a final `ORDER BY` the
  last window gives left out; `GroupAccumulator::finish_keeps_state`;
  `avg` of doubles rounded once (`ExactSum::mean`).
- **2026-11-06 — §M14.27 S3, two variables:** `corr`, `covar_pop`,
  `covar_samp` and the `regr_*` family (§8.5, *Parallelism*);
  `ExactSum::add_product` one exact step; `BigInt::div_big` Knuth's long
  division.
- **2026-11-06 — §M14.27 S2, moments:** `variance`, `var_pop`,
  `stddev`, `stddev_pop` (and the `_samp` names), `skewness`, `kurtosis`
  (§8.5, *Parallelism*), `bigint::BigInt` and `Wide`; `ExactSum`'s
  state smaller and merged without copies (`avg` by a million groups
  3.2 → 2.0 s).
- **2026-11-06 — §M14.27 S1, the exact accumulator:** `fsum::ExactSum`
  is Neal's superaccumulator (§8.5, *Parallelism*): additions, exact
  subtraction and products, rounding from the integer; the split law
  tested at 1–32 workers.
- **2026-11-06 — §M14.12 V2b, components by position:** `x[i]`,
  `x[lo:hi]`, `UPDATE … SET x[i] = v` for arrays and vectors (§8.1),
  through the type capability `Elements` (§8.12); Python vectors as
  `(n, d)` numpy and Arrow `fixed_size_list<float32>` (§10); pg-compat's
  `vector` oid, `to_regtype` and pgvector's binary format (§9).
- **2026-11-06 — §M14.12 V2, the exact search bound by bandwidth:**
  `large::Chunks` (values out of line read beside a batch scan's rows,
  §8.1), type lookups kept a thread (§8.12), the server's `--cache`
  (§9).  100k × 768 nearest neighbours 233 → 48 ms, numpy's 55.
- **2026-11-06 — §M14.28 L0, the commit's bytes:** the real platform's
  sync is `fdatasync` on Linux (§3.2); `FileSystem::writes_in_place`
  (false on Apple's systems); where true, WAL segments grown ahead by
  zeros and recycled as spares (§4.5); `truncate_before` syncs the
  directory before dropping segments (a window the simulation found).
  On ext4 a one-row commit 16.6 → 4.4 KiB written, ~4× faster.
- **2026-11-06 — §M14.12 V1, vectors:** `vector` / `vector(n)` (§8.1),
  the `Type` capabilities it needed (`arithmetic`, `modifier` / `fit`,
  `cast_from` / `cast_to`, `aggregate`), distance kernels in
  `nucleus-kernels`, `ncp` tag 11 named `vector`, and every driver's
  native float array (§11); `45_vector.slt`.

- **2026-11-05 — §M14.20 W8, the next batch made ahead:** with a fetch
  size, the server makes the next batch while the client reads one, and
  sends it when asked (drops it on `c`): a cursor of a million rows, 1000
  a batch, 600 → 461 ms; a Python stream 2.8 → 2.4 s.  W8 closed.

- **2026-11-05 — §M14.20 W8, one wire format crate:** `nucleus-wire`
  holds the protocol's versions, kinds, tags, frames, values and columnar
  layout; the server and the Rust driver use it instead of their four
  encoders and decoders, the server no longer through the storage's row
  encoding.  The same bytes on the wire, every type checked.

- **2026-11-05 — §M14.20 W8, every driver pipelines:** `pipeline` and
  `execute_many` (`executeMany`) in Python, JavaScript, Java, C and C++
  as in Rust; a statement for many rows runs all or none, one commit for
  all — an insert 185 → 11.5 µs a row, 100 a call.  The C driver sends a
  frame in one write and reads through a buffer: a point read 27.5 → 25
  µs.

- **2026-11-05 — §M14.20 W8, pipelining:** the server flushes its
  replies only when no next message waits in its read buffer; the Rust
  driver's `Client::pipeline` sends statements 32 KiB at a time and reads
  their replies.  A point read 100 at a time costs 5.8 µs each, one at a
  time 25.7.

- **2026-11-05 — §M14.20 W8, a session's texts parsed and normalized
  once:** a session keeps a text's statements as parsed
  (`Session::parse`) and its key in the statement statistics, with the
  plans' admission (`by_text::ByText`, one type for every entry by a
  text).  A parameterized point read on one connection 29.3 → 25.8 µs.

- **2026-11-05 — §M14.20 W8, pg-compat's prepared statements on the
  session's cache:** Parse describes by the statement's text, and Bind
  describes again (from the cache while the catalog holds), refusing a
  statement whose result changed since Parse with `0A000`, as PostgreSQL
  does — before, it kept Parse's description and ran the new plan.  A
  prepared point read through `postgres` unchanged, 40.7 µs.

- **2026-11-05 — §M14.20 W8, a session's plans by their text:** a
  query's plan and a statement's description kept by its text while the
  catalog holds, once the text came again (at most 512); a parameterized
  point read on one connection 39.2 → 29.0 µs.

- **2026-11-05 — §M14.20 W8, a point read set up as one:** a scan of
  one key of a unique index is not cut into morsels; `split_keys` finds
  each node's children in the range by binary search; the real platform
  asks the OS for its CPUs once.  A point read on one connection 38.8 →
  35.8 µs (literal), 42.7 → 39.9 (parameter).

- **2026-11-05 — §M14.20 W8, a session's catalog cache:** the KV store
  watches key ranges (`Kv::watch`, `KvTxn::watched_as_at`); a session
  keeps tables, database settings, ACLs, owners and effective roles
  across statements while no commit writes the catalog — never for a
  snapshot that reads it otherwise.  A parameterized point read on one
  connection 52 → 42 µs, `SELECT 1` 28 → 25.

- **2026-11-05 — §M14.20 W7, LZ by wild copies:** the decoder copies
  literals and matches in fixed 16-byte pieces into room left past the
  output (LZ4's way), 2–2.5× the one before; ClickBench's `SELECT *` on a
  column table 133 → 116 ms.

- **2026-11-05 — §M14.20 W7, the file grown ahead:** the pager writes
  zeros ahead of the pages it allocates, an eighth of the file at a time
  (1–64 MiB), so a page written past the old end lands in blocks the
  file already has.  The TPC-H load on row tables 56 → 27 s; a logged
  `COPY` of `lineitem` 22 → 9 s.

- **2026-11-05 — §M14.20 W7, page reads without zeroing:** the file is
  read into a buffer each thread keeps, each page copied out once (a
  read had allocated and zeroed both its run and every page); a batch
  scan reading no column (`count(*)`) does not look into its rows.
  ClickBench on a row table 6.5 → 6.1 s, its `count(*)` 93 → 78 ms.

- **2026-11-05 — §M14.20 W7, row format version 3:** an offset every
  16 columns, so a scan reading a few of a wide row's columns skips at
  most 15 values to each (`RowHeader::seek`, both decoders); text,
  `char(n)`, bytea and enums under 256 bytes with a one-byte length
  (tags 22–25, rows only — the wire unchanged); one row writer
  (`encoding::RowWriter`).  Versions 1 and 2 still read.  ClickBench on a
  row table 7.2 → 6.25 s, its file 984 → 902 MB.

- **2026-11-04 — §M14.20 W6, a column table's point reads:** a segment's
  directory entry decoded once and cached by its id (it decoded every
  column's least and greatest values a lookup); its row keys searched
  through marks — every 32nd key's first 8 bytes — first.  TPC-H Q19
  40 → 37 ms, Q20 167 → 156.

- **2026-11-04 — §M14.20 W6, a small frame for `Expr::eval`:** its
  less common nodes evaluated in `eval_other`, so each node of a row's
  expression takes 480 bytes of stack, not 1.7 kB (TPC-H Q18 305 → 262
  ms on column tables).

- **2026-11-03 — §M14.20 W6, correlated subqueries on the workers:** a
  correlated subquery runs on worker threads for each row, as on the
  statement's thread, in a query whose subquery reads only the row it
  runs for and nothing that differs by thread (`expr::ready_subqueries`);
  a filter or projection with one over rows that are not a pipeline's
  runs in blocks on the workers (`exec::OnWorkers`, shared with an index
  nested loop's lookups); the buffer pool's frames and shards take a
  cache line each, its hits counted by shard.  TPC-H Q20 577 → 173 ms on
  column tables.

- **2026-11-03 — §M14.20 W6, shared reads without a queue:** the
  engine's intent statuses and running ranges under a reader-writer
  lock; `KvRead::first` (a worker's lookup of a column table's segment
  without read-ahead); no lock for unlogged writes in a transaction
  without a fresh range; `EXPLAIN ANALYZE` reads only the columns the
  query needs (TPC-H Q19's took 18.6 s, every column of `lineitem` a
  row).

- **2026-11-03 — §M14.20 W6, `DISTINCT` inside aggregates as groupings:**
  `count(DISTINCT x)` and the aggregates beside it become a grouping by
  the group and `x` (the others' partial states) and one by the group
  (the `DISTINCT` call fed `x`, the others combining) — both the
  vectorized, parallel grouping; `Aggregate::order_free` says which
  aggregates may be fed so (§8.6, §8.12).

- **2026-11-03 — §M14.20 W6, typed aggregate states:** a grouping's
  `min`, `max`, `bool_and` and `bool_or` keep a value a group, its `sum`
  and `avg` of integers or of decimals at one scale an exact sum and a
  row count (a general state a group for the rest, and for a sum that
  meets another scale or kind); an `avg`'s partial state is its rows and
  sum; a grouping counts its states' bytes as the accumulators tell
  (`GroupAccumulator::group_bytes`), so fewer groups spill (§8.5, §8.12).

- **2026-11-03 — §M14.20 W6, a decimal division in one step:** `/` on
  numerics divides once when the dividend scaled up by the digits asked
  for (and one to round on) fits 128 bits, else by long division as
  before; a tiny dividend over a huge divisor gives its zero, not an
  overflow.

- **2026-11-03 — §M14.20 W6, top-N:** a sort right below a `LIMIT`
  keeps only the rows the limit takes and skips — a heap of that many
  (`spill::Top`; ties in the input's order, so the rows of the full
  sort), its rows built only when their keys enter, on each morsel of a
  parallel sort; past `work_mem` the full sort, as before (§8.5).

- **2026-11-03 — §M14.20 W6, groupings past `work_mem` on the workers:**
  one bound for a parallel aggregate's groups — past `work_mem` a running
  morsel writes its partial states to scratch by merge part and goes on
  empty, a morsel done keeps its groups while they fit in half; the merge
  parts read them back on the workers (they no longer give up and leave
  it to one thread partitioning rows); merged rows past half of
  `work_mem` a part at a time.  Scratch runs are shared between threads
  and read by byte ranges (`Run::at`, `Sealed::range`).  ClickBench Q32
  2.5 → 0.62 s (§8.5).

- **2026-11-03 — §M14.20 W3b, batches into storage:** one batch insert
  path (`dml::insert_batch`, `TableStorage::insert_batch`) for `INSERT`,
  `COPY` and partition moves — rows owned, row ids per batch, foreign-key
  keys deduplicated, the first error in row order; a column table's
  ascending batch gathered at once; segments built from typed vectors on
  threads (the same bytes as before); `COPY`'s parse overlapped with its
  stores (`nucleus_kv::Locked`); `TableStorage::sample`, so `ANALYZE`
  builds only the rows it takes.  Fixed: enum values read from a column
  segment lost their labels.  TPC-H SF 1: row load ~34 → ~30 s, column
  load ~34 → ~26 s (§8.7, §8.8, §8.15).
- **2026-11-02 — §M14.20 W2, one sorted bulk path:** `CREATE INDEX`,
  `ALTER … TYPE` and key rewrites build their indexes through
  `IndexBuild`: only the index's columns read, in parallel; entries in one
  buffer with abbreviated keys, sorted in parallel, past `work_mem` one
  run of byte records per spill merged by a heap; uniqueness by
  neighbours; written in key order into the fresh range; internal nodes
  packed to 7/8 as leaves are.  `exec::index_entry` (unused) removed.
  Fixed: a column-table statement that failed part way left its gathered
  rows for the next statement on the thread, which stored them past the
  unique checks.  TPC-H SF 1 row load 69.8 → 45.6 s (its indexes 40.3 s
  when profiled, now 15.8 s).
- **2026-11-02 — §M14.20 W1, quick gains:** `ANALYZE` by Algorithm L
  (11.5 → 5.5 s on `lineitem`), row decoding stops after the last wanted
  column, checkpoints after 1 GiB, `--sync ordered|full` (D17), 64 KiB
  wire buffers, the JS driver's frames joined once.  Load 79.4 → 69.8 s.
- **2026-11-02 — §M14.20 W0, column tables' concurrent writes:**
  overlapping writers conflict (`40001`) as on a row table; the second's
  rows were lost.

- **2026-11-01 — §M14.20 V13, decimal vectors:** `Vector::Dec` —
  decimals of one scale as their mantissas, decoded so from a column
  store's decimal segments and a row table's numeric columns —, kernels
  for their `+`, `-`, `*` and comparisons (`vector::dec_kernel`), and
  `sum` / `avg` adding them at their scale.  TPC-H SF 1 on column tables
  Q1 467 → 185 ms, Q6 143 → 82, Q14 120 → 97, Q15 101 → 74, Q3 162 →
  133; on row tables Q1 784 → 493.
- **2026-11-01 — §M14.20 V13, key filters, eager aggregation, parallel
  merges:** `keyfilter` — an inner or semi hash join whose right side was
  narrowed hands its integer keys (range, a list of up to 64, else a
  Bloom filter) to the scans below its probe side (`Step::Keys`, the
  range a column store's segment bound; every executor alike, `EXPLAIN
  ANALYZE` marks the scan); `eager` — a grouping by a join's left key
  over infallible aggregates of the right side computed below the join
  (`Aggregate::infallible`); parallel aggregates of many groups merged on
  the workers a hash part each (`MERGE_PARTS`), those in key order
  finished on the workers but for each morsel's edges; compact partial
  states (`count`, `min`/`max`/`bool_*`, `sum` of no doubles), the
  accumulators grown by half again, one probe for an integer key;
  groups by a direct index over a narrow integer range, else a salted
  table hashing and comparing a batch's vectors in place (`keytable`);
  join tables on two integer keys hashed as a pair, a batch's lookups
  before its output, and a build side that is no pipeline gathered in
  columns (`JoinTable::gather`; spilled past work_mem as before);
  dictionary segments decoded and cached as codes and entries
  (`Vector::Dict`), a filter's leading tests of such a column tried on
  the entries once a segment (`vector::DictFilter`, `Scan::dicts`).
  TPC-H SF 1 on column tables Q1 535 → 471 ms, Q5 528 → 354, Q9 2437 →
  946, Q13 367 → 99, Q18 885 → 373; on row tables Q5 1010 → 793, Q9 2950
  → 1423, Q13 424 → 173, Q18 1334 → 747.  ClickBench on column tables
  0.31× → 0.26× PostgreSQL's time.
- **2026-11-01 — §M14.20, TPC-H Q13 and Q16:** `columns::Columns` —
  rows a column at a time — for the hash join tables the workers make
  (the keys too); `kernels::like_parts` (`LIKE` with `%` inside the
  pattern), substrings found by their first and last bytes 32 places at
  a time, the row-at-a-time `LIKE` over bytes for ASCII; `NOT LIKE`'s
  selectivity 0.9; uncorrelated `IN (subquery)` through the `IN`-list
  kernel (`in_consts`, a hash set past eight items);
  `expr::prime_subqueries` / `vector::parallel_safe_in`: uncorrelated
  subqueries run once before workers use them, the cache shared with
  the workers (`Arc` rows).  TPC-H SF 1 0.67× / 1.27× PostgreSQL's time
  (column / row tables), ClickBench 0.31× / 2.31×
  (`docs/benchmarks.md`).
- **2026-11-01 — §M14.20, TPC-H Q19 and Q22:** `Vector::Char`
  (`char(n)` from row and column storage, compared without trailing
  blanks), an `IN`-list kernel, `ScalarFunction::call_batch` (`substr`'s
  kernel), uncorrelated subqueries evaluated once per batch evaluation;
  index nested loops on worker threads (`vector::on_workers`, shared
  with the morsels); the buffer pool's hits under its shard's read lock,
  page ids hashed by one multiplication; the column store's cache of
  decoded columns in 16 read-locked shards under one bound.  TPC-H SF 1
  0.75× / 1.40× PostgreSQL's time (column / row tables), ClickBench
  0.32× / 2.32× (`docs/benchmarks.md`).
- **2026-11-01 — §M14.20, TPC-H and ClickBench again** (at `e2d1afa`,
  `docs/benchmarks.md`): TPC-H SF 1 1.03× PostgreSQL's time on column
  tables and 2.34× on row tables in the geometric mean (1.10× and 2.24×
  on 10-30); ClickBench 0.34× and 2.47× (0.34× and 2.51×); the loads
  faster (TPC-H 98 s / 111 s from 118 s / 125 s, ClickBench 18.7 s /
  20.9 s from 21.7 s / 21.4 s).  Row tables' times vary between runs by
  whole queries (Q3 3.2 s in one run, 1.2 s in the next); not yet
  explained.

- **2026-11-01 — §M14.20, the self-join:** a hash join whose right side
  is a pipeline has it collected (its keys evaluated a batch at a time)
  on worker threads, morsels in key order; the table groups each key's
  row numbers in one array (it allocated a vector per key, to make and
  to free).  `SELECT count(*) FROM b x JOIN b y ON x.id = y.v` over 1M
  rows ~490 → ~190 ms: 1.7× PostgreSQL's (4.2× before).

- **2026-11-01 — §M14.20, the load:** `put_new` notes the run of keys
  without an entry up to the tree's next one, and a bulk insert of new
  keys in order looks at the tree once, not once a row; a spill checks
  its keys as a commit does (the recent commits' keys first), not by a
  look at the tree per key; a spilled transaction's commit deletes the
  intents only of the keys inside its spilled span (it deleted one for
  every key written after the spill) and builds its ops in key order,
  without a sort.  Per row of the load, fewer allocations: the primary
  key's suffix borrowed, `||` appending to its left text, the casts to a
  column's own type first.  The 1M-row `INSERT … SELECT` ~2,000 →
  ~1,250 ms through the server (~1,080 embedded): 1.2–1.4× PostgreSQL's.

- **2026-11-01 — §M14.20, a commit's leaves prepared while others are
  installed:** a sorted batch's workers run for the whole batch, a
  window ahead of the walk that installs their leaves (it waited for each
  window, and they for its installs); prepared pages are swapped into
  the pool's frames, not copied.  The 14,768 leaves of the 10% delete:
  installed in ~24 ms (the walk was ~37, waiting for windows ~20 of
  them).  A 10% delete ~144 → ~136 ms, a 25% update ~773 → ~750 ms.

- **2026-11-01 — §M14.20, a delete's rows stashed once:** `DELETE` from a
  table that is not partitioned writes the rows as the scan stashed them
  (key first), without stashing each again behind its partition's
  number.  A 10% delete ~160 → ~141 ms (1.96× PostgreSQL's time in the
  same run).

- **2026-11-01 — §M14.20, the write set and the recent keys without a
  tree per key:** a transaction's writes are a sorted vector while they
  come in order (a B-tree map once they do not), and a large commit's
  keys join the recent commits' as one sorted run; the commits before a
  snapshot are skipped as a whole.  Per written row: a push and one
  comparison, where it was two tree lookups and two inserts.  A 10%
  delete ~225 → ~160 ms, a 25% update ~935 → ~785 ms, the load
  ~2 300 → ~2 000 ms; 20 updates of 100 000 rows 5.5 → 4.6 s.

- **2026-11-01 — §M14.20, garbage collected as leaves are rewritten:** a
  commit no longer walks its keys' versions to delete the old ones; its
  batch drops them from the leaves it rewrites anyway (a compaction
  filter, `BTree::apply_sorted_filtered` / `Store::write_filtered`, not
  logged), for every key there, and its conflict check is answered by the
  recent commits' keys, the tree read only for the rest.  A 10% delete
  ~260 → ~225 ms, a 25% update ~975 → ~935 ms; 20 updates of 100 000 rows
  6.6 → 5.5 s, the table 32 → 28 MB.

- **2026-11-01 — §M14.20, a commit's leaves on workers:** a sorted batch
  reaching 64 leaves or more merges and encodes them on the CPUs
  (`BTree::set_workers`), installed in order, so the tree is the one a
  thread makes, page for page; entries go to a node's children by a
  search per child.  A 10% delete ~310 → ~260 ms, a 25% update
  ~1 050 → ~975 ms.
- **2026-11-01 — §M14.20, less per written row:** `UPDATE` and `DELETE`
  consume their stashed rows (copied only for `RETURNING`), a delete
  computes index keys only for a table with indexes, a row's key is
  encoded in one buffer, and a commit's batch is sorted by the engine and
  applied as it is (the store copied and sorted it).  A 10% delete
  341 → ~310 ms, a 25% update ~1 150 → ~1 050 ms, the load
  ~2 800 → ~2 500 ms.
- **2026-11-01 — §M14.20, the write path:** `UPDATE` and `DELETE` find
  their rows by the vectorized, parallel scan; a write's early conflict
  check answered by the recent commits' keys; a B+tree range descends
  once (it descended twice), a commit's checks walk one cursor
  (`Cursor::seek_near`), and a sorted batch's leaves are merged by
  reference and encoded in place (`node::encode_leaf`), nothing decoded.
  Fixed: the recent keys were pruned past the visible snapshot, which a
  transaction could still begin at — its serializable reads could miss a
  conflict.  `bench/run.py`: a 10% delete 826 → 341 ms, a 25% update
  1 896 → 1 150 ms.
- **2026-10-30 — §M14.20 V11, vacuum when idle:** the server's vacuum
  runs when no transaction began for 10 s and none runs, in batches of
  2 048 entries,
  stopping when one begins and going on from there later
  (`Kv::vacuum_background`, `Kv::activity`); paced after 10 minutes
  without a pause.  At full speed after each large commit it slowed the
  index builds, `ANALYZE` and queries after a load: TPC-H SF 1 row load
  182 → 129 s.
- **2026-10-30 — §M14.20 V10, more from the V12 measurements:** scan
  filters compare with constant expressions' values (computed when the
  plan is made); a column's two range bounds are estimated together; a
  semi or anti join hashes its left side when that is the smaller.  TPC-H
  SF 1 on column tables: Q4 1.26 → 0.11 s, Q22 0.87 → 0.24 s, Q6 0.49 →
  0.17 s, Q14 0.57 → 0.18 s, Q3 0.36 → 0.19 s.
- **2026-10-30 — §M14.20 V10, from the V12 measurements:** `char(n)`
  columns are segment dictionaries and offsets of their own (they were
  stored value by value), and a dictionary's row is read where it lies —
  a column table's index lookup no longer decodes `l_shipmode`'s segment:
  TPC-H Q19 on column tables 42 s → 0.19 s; a query's correlated
  subqueries keep their results by the outer values they read: Q17 1.5 →
  0.12 s, Q2 0.20 → 0.06 s.
- **2026-10-30 — §M14.20 V12, statistics across columns:** `ANALYZE`
  counts the columns of every multi-column key, index and `CREATE
  STATISTICS` together — distinct combinations and functional
  dependencies — and equalities on several columns, joins on several
  columns and `GROUP BY` / `DISTINCT` are estimated from them; `CREATE` /
  `DROP STATISTICS`; `nc_catalog.statistics_objects`, `.column_groups`,
  `.dependencies`; table record version 13, statistics version 2.
- **2026-10-30 — §M14.20 V11, write intents:** a large transaction's
  spilled writes are versions stamped with its id (intents), resolved by
  its status; the commit is a status record and nothing moves after it —
  every write of a load or an index build written once; `VACUUM` (SQL,
  `Kv::vacuum`) makes committed intents versions, and the server's
  maintenance thread runs it when there are some; store readers let
  waiting writers first; index builds read their rows in chunks.  SF 1:
  `lineitem`'s `COPY` 52 → 38 s, an index on it 30 → 18 s.
- **2026-10-30 — §M14.20 V10, OR implications and existence lookups:**
  an OR across two tables pushes each its implied filter (TPC-H Q19
  770 → 128 ms); uncorrelated subqueries go below semi and anti joins and
  existence lookups stop at the first match (Q22 1034 → 386 ms).
- **2026-10-30 — §M14.20 V10, column tables' point reads:** a B+tree
  range checks its end on the key before reading the value; decimals of
  one scale packed as integers in segments; one row's value read from a
  column's cached encoding (`segment::value_at`); subqueries' scans decode
  only their columns; index nested loops on column tables, costed as
  such; `KvTxn::first`; a commit's conflict checks and old versions in one
  walk per key.  Fixed: an index nested loop's right side reads its left
  row one level down — the plan walkers that move references now count it
  (`XX000 outer column out of range` on TPC-H Q2 over column tables).  SF 1
  in columns: the load 675 → 136 s; Q17 timed out → 1.4 s, Q20 → 0.6 s,
  Q21 4.7 → 1.2 s.
- **2026-10-30 — §M14.20 V10, scans that want one entry:** a
  transaction's scan reads ahead 8 entries at first, doubling to 128 —
  a column table's segment lookup read its whole directory per row (an
  SF 1 `lineitem` loads into columns in 84 s, from most of 729);
  `DELETE` of short rows without reading them again.
- **2026-10-30 — §M14.20 V10, vectorized joins and more:** hash joins
  probed a batch at a time and in parallel; narrow rows through joins;
  the executor's tables on a fast hash; parallel sorts; decimal sums
  without dispatch; `CREATE INDEX` writes declared fresh (`KvTxn::fresh`:
  no conflict checks or old-version collection for a new index's keys);
  `COPY` and `INSERT` tell a duplicate and a conflict from one look at the
  key (`KvTxn::put_new`), encode rows without a buffer per value and parse
  decimals without building strings.  SF 1 on rows, all 22 queries 35 →
  26 s (Q9 6.7 → 2.9 s, Q13 2.5 → 1.3 s).

- **2026-10-30 — §M14.20 V10, index nested loops:** `JoinAlgo::Lookup`
  chosen by cost; semi and anti joins placed among the ordered joins by
  estimate; parallel aggregates in key order merged a morsel edge at a
  time; key-aware join estimates in every estimate.  SF 1 on rows, all 22
  queries 125 → 35 s (Q2 14 → 0.16 s, Q18 25 → 1.7 s, Q21 11 → 3.0 s).
- **2026-10-30 — §M14.20 V10, from the TPC-H profile:** joins on a key of
  two columns estimated by the key (Q9 26 → 6.7 s at SF 1); semi and anti
  joins placed below the inner joins (Q18 25 → 9.6 s); the buffer pool in
  shards, each its own lock; CRC-32C by the CPU's instructions; LZ matches
  copied a period at a time, and only values of 1 KiB or more compressed;
  decoded column segments cached by size, those used longest ago evicted
  (column tables' point reads no longer decode a segment each).

- **2026-10-30 — §M14.20 TPC-H (V10, second part):** `bench/tpch.py`
  (generated TPC-H data, the 22 queries, results checked against
  PostgreSQL's); `EXISTS` / `NOT EXISTS` / `IN` unnested into semi and
  anti joins; common conjuncts taken out of `OR`; `INTERVAL '90' DAY`
  literals.  Fixed: a correlated subquery in a join's `WHERE` reading
  both sides was pushed into one (`XX000 outer column out of range`), and
  references inside correlated subqueries now follow their rows when the
  optimizer moves conditions; `COPY` into a `column` table lost its rows
  (its statement's writes were never finished), and into a partitioned
  table wrote the parent.

- **2026-10-30 — §M14.20 results columnar end to end (V10, first part):**
  a vectorized query's batches go into `ncp` columnar results as columns
  (`Sink::batch`, `vector::batches`, the server's `columnar::Builder`); a
  million numeric rows into numpy 262 → 92 ms (row table), 241 → 51 ms
  (column table); the Python driver cuts text columns from one decoded
  buffer.

- **2026-10-30 — §M14.20 partitioning (V9, second part):** `ATTACH
  PARTITION`, `DETACH PARTITION`, `DROP PARTITION`; storage policies by
  age (`SET STORAGE POLICY (storage AFTER interval) | NONE`), applied when
  set and by the server's maintenance thread.

- **2026-10-30 — §M14.20 partitioning (V9, first part):** `PARTITION
  BY RANGE | LIST | HASH`, `PARTITION OF … FOR VALUES … | DEFAULT`;
  partitions pruned when the plan is made and when it runs; inserts
  routed, updates moving rows; indexes and columns on every partition;
  `nc_catalog.partitions`; `partition_hash`.
- **2026-10-30 — §M14.20 storage per table and per column (V8):** `row,
  column` (both copies; the column copy of every column or some), `ALTER
  TABLE … SET STORAGE` converting online in parts with small switches;
  deletion markers by row; a `column` table's delta store under its own
  index.
- **2026-10-29 — §M14.20 columnar storage (V7):** `WITH (storage =
  column)`: column segments (bit packing, runs, dictionaries), a delta
  store, deletion markers, merges; scans of only the wanted columns,
  segments skipped by their least and greatest values;
  `nc_catalog.segments`.  A million-row `sum` 22 → 2 ms.
- **2026-10-29 — §M14.20 table storage as a module (V6):** the
  `TableStorage` trait and registry (`Registry::add_storage`), `row`
  behind it; `CREATE TABLE … WITH (storage = name)`,
  `nc_catalog.tables.storage`, `nc_catalog.storages`.
- **2026-10-29 — §M14.20 compression (V5):** leaves store their keys'
  shared prefix once; values of 64 bytes or more are stored LZ-compressed
  when that pays; format version 2 (version 1 read).  A million-row
  table's file 93 → 61 MB.
- **2026-10-29 — §M14.20 parallel execution (V4):** the KV contract's
  `shared`, `split_points` and `parallelism`; aggregates and filtering
  pipelines run on worker threads by morsels, combined in key order;
  `parallel_workers`; exact sums of doubles.  A million-row `sum` 77 → 27
  ms, the 1000-group `GROUP BY` 100 → 37 ms.
- **2026-10-29 — §M14.20 batch kernels (V3):** `nucleus-kernels` —
  comparisons, arithmetic, sums, minima and maxima, selection, string
  equality and `LIKE`, hashing — per CPU level (AVX2, AVX-512, NEON),
  each tested against its scalar reference; text and date / time
  vectors.  A million-row `sum` 90 → 77 ms.
- **2026-10-29 — §M14.20 the vectorized executor (V2):** pipelines of
  scans, filters and projections a batch at a time, vectorized
  aggregates (`Aggregate::grouped`), `batch_size`; the test kit's scripts
  run three ways.  A million-row `sum` 265 → 90 ms, the filtered count and
  the 1000-group `GROUP BY` likewise.
- **2026-10-29 — §M14.20 batch reads (V1):** the KV contract's
  `scan_chunks`; the B+tree, the engine and table scans read through
  reused buffers instead of allocating per entry.  A million-row
  `count(*)` 360 → 200 ms, `sum` 430 → 265 ms.
- **2026-10-27 — §M14.11 shipped:** streaming and columnar results, bulk
  load, large transactions, a memory-bounded executor, faster B+tree
  paths, the benchmark suite in CI.
- **2026-10-27 — §M14.11 benchmarks (L8):** `bench/run.py`, one workload
  on nucleus and PostgreSQL; a CI job against `postgres:18`;
  `docs/benchmarks.md`.
- **2026-10-27 — §M14.11 scans decode only the columns used (L7):**
  `prune` marks each scan's needed columns; the rest are skipped
  undecoded (out-of-line values unfetched).
- **2026-10-27 — §M14.11 faster checksums, a larger cache (L7):**
  CRC-32C by slicing-by-8; buffer frames made when first used; the
  default page cache 128 MiB.
- **2026-10-27 — §M14.11 the B+tree's read and write paths (L7):** pages
  read in place (`node::View`) by `get` and cursors; batches applied in
  one sorted pass; leaves packed fuller on sequential inserts.  A
  2-million-row load: 197 s → 13 s.
- **2026-10-27 — §M14.11 joins past work_mem (L6c):** grace hash joins,
  block nested loops, full joins that no longer collect their output.
- **2026-10-27 — §M14.11 aggregation, DISTINCT and set operations past
  work_mem (L6b):** groups, rows seen and counts kept in memory up to the
  limit; the rest partitioned by key and taken in turn.
- **2026-10-27 — §M14.11 a memory-bounded executor, first part (L6a):**
  `work_mem`; external merge sorts; materialised CTEs, recursion and the
  rows writes collect spill to scratch space (the KV contract's
  `scratch()`); `generate_series` streams; ANALYZE samples tables past
  30 000 rows.
- **2026-10-27 — §M14.11 large transactions (L5b):** writes past
  `txn_memory` spill into the store's side area; the commit stays one
  atomic batch (a status record), the side area is then moved into
  versions while readers merge it; rollbacks, drops and crashes leave
  nothing.
- **2026-10-27 — §M14.11 bulk load in the drivers (L5):** `copy` with
  text or CSV in every driver; `load` of rows, columns, DataFrames and
  Arrow tables as columnar batches in Python, JavaScript and Java.
- **2026-10-27 — §M14.11 bulk load (L5):** `COPY t [(columns)] FROM
  STDIN` in text and CSV, parsed as the data arrives, each row checked as
  an INSERT's; `ncp`'s `I`, `d`, `B`, `e`, `f`; the Rust driver's
  `copy_in` and `copy_rows`; pg-compat's COPY sub-protocol (psql's
  `\copy`, drivers' `copy_in`); `ncsql \copy`.
- **2026-10-27 — §M14.11 columnar results in the drivers (L4):**
  Python (numpy, Arrow, pandas, polars), JavaScript (typed arrays), Java
  (buffers), C and C++ (pointers) read batches in place.
- **2026-10-27 — §M14.11 columnar results (L3):** `ncp` version 7's
  result format 2 sends batches `B`, a buffer per column (fixed widths
  back to back, variable widths as offsets and bytes, a validity bitmap,
  8-byte aligned); the Rust driver's `Client::batches`.
- **2026-10-27 — §M14.11 streaming results and cursors (L1, L2):** a
  query's rows go to the client as the executor produces them; `ncp`
  version 6 fetches them in batches (`Q`'s fetch size, `S`, `F`, `c`);
  a cursor in every driver; pg-compat streams simple queries and Execute
  without a row limit.

- **2026-10-23 — §M14.10 `docs/llm.md` (B4) — §M14.10 shipped:** a
  compact, model-oriented reference of the dialect, its gaps with what to
  use instead, the catalog, hints and the analyzer.
- **2026-10-23 — §M14.10 `nucleus mcp` (B3):** the `mcp` module, a Model
  Context Protocol server over stdio — tables (with comments), their
  descriptions, samples, read-only queries with row and time limits,
  plans, lint, writes when allowed; `nucleus mcp` for a directory or a
  server.
- **2026-10-23 — §M14.10 comments and hints (B1, B2):** `COMMENT ON
  TABLE / COLUMN` (table record version 9; nc_catalog, `\d`, pg-compat's
  descriptions); errors with a hint — the name meant, the names there
  are, the cast — on `ncp` version 5 and pg's `H`, in every driver and
  ncsql.
- **2026-10-23 — §M14.9 the index advisor (A4) — §M14.9 shipped:**
  `nc_catalog.index_advice`, the recorded statements planned again with
  hypothetical indexes; the analyzer in §8.13.
- **2026-10-23 — §M14.9 the linter (A3):** `ANALYZE QUERY <statement>`
  and `ncsql \lint` — full scans under a selective filter, comparisons
  that defeat an index, foreign keys without an index, cartesian
  products, `NOT IN` over a nullable subquery; `ncsql \statements`.
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
