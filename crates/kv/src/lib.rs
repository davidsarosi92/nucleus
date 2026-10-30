//! The KV contract — the interface every layer above the core is written
//! against (PLAN.md principle 1), and that the distributed implementation
//! (§F3) will also implement.
//!
//! - [`Kv`] starts transactions; [`KvTxn`] reads and writes inside one.
//! - Keys and values are byte strings; keys are ordered bytewise.  Encode
//!   keys so that bytewise order is the order you need (principle 2).
//! - Every transaction reads a consistent snapshot ([`KvTxn::read_ts`]) and
//!   commits atomically.  A commit can fail with [`Error::Conflict`], which
//!   is **retryable**: run the whole transaction again ([`transact`] does).
//! - Reads take `&self`, so a scan may be open while the same transaction
//!   does point reads; writes take `&mut self`.
//!
//! [`Database`] is the local implementation, over `nucleus-txn`.

use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use nucleus_platform::Platform;
use nucleus_platform::real::RealPlatform;
use nucleus_storage::StoreOptions;

pub use nucleus_txn::{Chunk, Isolation, Timestamp};

/// Errors of the KV contract.
#[derive(Debug)]
pub enum Error {
    /// A concurrent transaction committed a conflicting change first.
    /// Retryable.
    Conflict {
        /// A key involved.
        key: Vec<u8>,
    },
    /// The requested snapshot is no longer available.
    SnapshotTooOld,
    /// A write in a read-only transaction.
    ReadOnly,
    /// A key longer than the implementation accepts.
    KeyTooLarge(usize),
    /// Anything else from the implementation (I/O, corruption, …).  Not
    /// retryable; the database may need reopening.
    Storage(Box<dyn std::error::Error + Send + Sync>),
}

impl Error {
    /// Whether running the transaction again may succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::Conflict { .. })
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Conflict { key } => {
                write!(f, "conflict with a concurrent transaction (key {key:?})")
            }
            Error::SnapshotTooOld => f.write_str("snapshot too old"),
            Error::ReadOnly => f.write_str("write in a read-only transaction"),
            Error::KeyTooLarge(n) => write!(f, "key of {n} bytes is too large"),
            Error::Storage(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Storage(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

impl From<nucleus_txn::Error> for Error {
    fn from(e: nucleus_txn::Error) -> Self {
        match e {
            nucleus_txn::Error::Conflict { key } => Error::Conflict { key },
            nucleus_txn::Error::SnapshotTooOld => Error::SnapshotTooOld,
            nucleus_txn::Error::ReadOnly => Error::ReadOnly,
            nucleus_txn::Error::KeyTooLarge(n) => Error::KeyTooLarge(n),
            e => Error::Storage(Box::new(e)),
        }
    }
}

/// The result type of the KV contract.
pub type Result<T> = std::result::Result<T, Error>;

/// How to start a transaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxnOptions {
    /// Isolation level.
    pub isolation: Isolation,
    /// Refuse writes (lets implementations skip commit work).
    pub read_only: bool,
    /// Read the snapshot at this timestamp instead of the latest (implies
    /// read-only).  Fails with [`Error::SnapshotTooOld`] if it is gone.
    pub at: Option<Timestamp>,
}

impl TxnOptions {
    /// A read-only transaction on the latest snapshot.
    pub fn read_only() -> Self {
        Self {
            read_only: true,
            ..Self::default()
        }
    }

    /// A serializable read-write transaction.
    pub fn serializable() -> Self {
        Self {
            isolation: Isolation::Serializable,
            ..Self::default()
        }
    }
}

/// An iterator over `(key, value)` pairs.
pub type KvIter<'a> = Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>;

/// A scan read a chunk of entries at a time ([`KvTxn::scan_chunks`]).
pub trait KvChunks {
    /// Replaces `out`'s entries with the next ones — about
    /// [`CHUNK_ENTRIES`], fewer once they take [`CHUNK_BYTES`]; false at
    /// the end.
    fn next_chunk(&mut self, out: &mut Chunk) -> Result<bool>;
}

/// Entries a chunk holds at most.
pub const CHUNK_ENTRIES: usize = 1024;
/// The bytes after which a chunk ends early.
pub const CHUNK_BYTES: usize = 1 << 18;

/// Chunks over a scan's iterator: what a transaction that has no chunked
/// scan of its own gives.
struct IterChunks<'a>(KvIter<'a>);

impl KvChunks for IterChunks<'_> {
    fn next_chunk(&mut self, out: &mut Chunk) -> Result<bool> {
        out.clear();
        while out.len() < CHUNK_ENTRIES && out.bytes() < CHUNK_BYTES {
            let Some(e) = self.0.next() else { break };
            let (k, v) = e?;
            out.push(&k, &v);
        }
        Ok(!out.is_empty())
    }
}

/// An engine scan's chunks.
struct ScanChunks<'a>(nucleus_txn::Scan<'a>);

impl KvChunks for ScanChunks<'_> {
    fn next_chunk(&mut self, out: &mut Chunk) -> Result<bool> {
        Ok(self.0.next_chunk(out, CHUNK_ENTRIES, CHUNK_BYTES)?)
    }
}

/// A database that runs transactions.
pub trait Kv: Send + Sync {
    /// Starts a transaction.
    fn begin(&self, opts: TxnOptions) -> Result<Box<dyn KvTxn>>;

    /// Wall-clock time as the implementation keeps it — the local one's
    /// platform clock; a distributed one answers from its hybrid logical
    /// clock.  Layers above read the time only here (principle 4), so a
    /// simulation's `now()` is the simulated clock.
    fn wall_clock(&self) -> SystemTime;

    /// Monotonic time since an arbitrary origin, for measuring how long
    /// something took (`EXPLAIN ANALYZE`, statement statistics): the
    /// platform's, so a simulation measures simulated time.
    fn elapsed(&self) -> Duration;

    /// Waits before retrying a transaction for the `attempt`-th time (from
    /// 1): exponentially longer each time, with jitter, so retries that
    /// collided once spread out.  Through the implementation's platform, so
    /// a simulation does not wait in real time.
    fn back_off(&self, attempt: u32);

    /// Fills `buf` with unpredictable bytes (`gen_random_uuid()`): the
    /// platform's (the operating system's generator; a simulation's seed).
    fn random_bytes(&self, buf: &mut [u8]);

    /// How many threads a statement's work can usefully be split across
    /// (parallel scans, §M14.20); 1 by default.
    fn parallelism(&self) -> usize {
        1
    }

    /// Scratch space for work too large for memory — a sort's runs, a hash
    /// join's partitions: a file of this process's alone, gone when
    /// dropped (and when the database next opens, after a crash).  The
    /// default keeps it in memory, for an implementation without files.
    fn scratch(&self) -> Result<Box<dyn Scratch>> {
        Ok(Box::new(MemoryScratch::default()))
    }

    /// Maintenance: removes the versions no snapshot can see, and makes
    /// committed large transactions' intents versions; how many entries it
    /// removed.  The default has nothing to do.
    fn vacuum(&self) -> Result<u64> {
        Ok(0)
    }

    /// [`vacuum`](Kv::vacuum) as a background task: in small batches,
    /// `pause` called between them.  The default: `vacuum`.
    fn vacuum_background(&self, pause: &mut dyn FnMut()) -> Result<u64> {
        let _ = pause;
        self.vacuum()
    }

    /// Whether [`vacuum`](Kv::vacuum) has intents to make versions (reads
    /// resolve them meanwhile, for a little more).  The default: no.
    fn vacuum_wanted(&self) -> bool {
        false
    }
}

/// Scratch space ([`Kv::scratch`]): bytes written and read back by offset.
pub trait Scratch: Send {
    /// Writes `data` at `offset`.
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()>;
    /// Fills `buf` from `offset` (all of it, or an error).
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;
}

/// Scratch space in memory: [`Kv::scratch`]'s default.
#[derive(Debug, Default)]
pub struct MemoryScratch(Vec<u8>);

impl Scratch for MemoryScratch {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        let end = offset as usize + data.len();
        if self.0.len() < end {
            self.0.resize(end, 0);
        }
        self.0[offset as usize..end].copy_from_slice(data);
        Ok(())
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let src = self
            .0
            .get(offset as usize..offset as usize + buf.len())
            .ok_or_else(|| Error::Storage("read past the end of scratch space".into()))?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

/// A scratch file in the database's directory (`scratch/<n>`), removed
/// when dropped.
struct FileScratch {
    file: Box<dyn nucleus_platform::File>,
    path: PathBuf,
    platform: Arc<dyn Platform>,
}

impl Scratch for FileScratch {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.file
            .write_all_at(data, offset)
            .map_err(|e| Error::Storage(Box::new(e)))
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.file
            .read_exact_at(buf, offset)
            .map_err(|e| Error::Storage(Box::new(e)))
    }
}

impl Drop for FileScratch {
    fn drop(&mut self) {
        let _ = self.platform.fs().remove_file(&self.path);
    }
}

/// A transaction's reads, shared by several threads at once
/// ([`KvTxn::shared`]): the same snapshot, the transaction's own writes
/// included, each method as [`KvTxn`]'s.
pub trait KvRead: Sync {
    /// [`KvTxn::read_ts`].
    fn read_ts(&self) -> Timestamp;
    /// [`KvTxn::get`].
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    /// [`KvTxn::scan`].
    fn scan<'a>(&'a self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<KvIter<'a>>;
    /// [`KvTxn::get_validated`].
    fn get_validated(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    /// [`KvTxn::scan_validated`].
    fn scan_validated<'a>(&'a self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<KvIter<'a>>;
    /// [`KvTxn::scan_chunks`].
    fn scan_chunks<'a>(
        &'a self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Box<dyn KvChunks + 'a>>;
    /// [`KvTxn::scan_chunks_validated`].
    fn scan_chunks_validated<'a>(
        &'a self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Box<dyn KvChunks + 'a>>;
}

/// A transaction.  Dropping it without committing rolls it back.
pub trait KvTxn: Send {
    /// The snapshot this transaction reads.
    fn read_ts(&self) -> Timestamp;

    /// The value of `key`, if any (including this transaction's writes).
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// The pairs with keys in `(start, end)`, ascending.
    fn scan<'a>(&'a self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<KvIter<'a>>;

    /// [`get`](Self::get), and the commit fails with a conflict if a commit
    /// after this transaction's snapshot wrote `key` — at any isolation
    /// level.  For decisions that must still hold at commit (a foreign key's
    /// referenced row exists) without making the whole transaction
    /// serializable.
    fn get_validated(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// [`scan`](Self::scan), validated as [`get_validated`](Self::get_validated)
    /// over the part of the range consumed.
    fn scan_validated<'a>(&'a self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<KvIter<'a>>;

    /// [`scan`](Self::scan) a chunk of entries at a time, packed into one
    /// buffer the caller reuses — nothing allocated per entry: what large
    /// reads use.
    fn scan_chunks<'a>(
        &'a self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Box<dyn KvChunks + 'a>> {
        Ok(Box::new(IterChunks(self.scan(start, end)?)))
    }

    /// [`scan_chunks`](Self::scan_chunks), validated as
    /// [`scan_validated`](Self::scan_validated).
    fn scan_chunks_validated<'a>(
        &'a self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Box<dyn KvChunks + 'a>> {
        Ok(Box::new(IterChunks(self.scan_validated(start, end)?)))
    }

    /// The transaction's reads for other threads to share while it is
    /// borrowed (parallel scans, §M14.20); `None` (the default) when it
    /// cannot share them.
    fn shared(&self) -> Option<&dyn KvRead> {
        None
    }

    /// The first entry of the range `[start, end)` (bounds as [`scan`]'s),
    /// for a reader that wants just it: by default the scan's first; an
    /// implementation may skip the read-ahead a scan does.
    ///
    /// [`scan`]: KvTxn::scan
    fn first(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        self.scan(start, end)?.next().transpose()
    }

    /// Up to `parts - 1` keys inside `[start, end)` (`end` `None`: no
    /// bound) that cut it into parts of about equal size, ascending — where
    /// a parallel scan splits its range; none (the default) when the
    /// implementation cannot tell, or the range is small.
    fn split_points(&self, start: &[u8], end: Option<&[u8]>, parts: usize) -> Result<Vec<Vec<u8>>> {
        let _ = (start, end, parts);
        Ok(Vec::new())
    }

    /// Sets `key` to `value`.
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()>;

    /// Deletes `key` (no error if absent).
    fn delete(&mut self, key: &[u8]) -> Result<()>;

    /// Sets `key` to `value` unless it has a value (`false`, nothing
    /// written): [`get`](Self::get), then [`put`](Self::put), by default; an
    /// implementation may tell both from one look at the key.
    fn put_new(&mut self, key: &[u8], value: &[u8]) -> Result<bool> {
        if self.get(key)?.is_some() {
            return Ok(false);
        }
        self.put(key, value)?;
        Ok(true)
    }

    /// Declares the keys in `[start, end)` fresh: none has a version yet and
    /// no other transaction writes one before this one ends (a new index's
    /// keys, a table made in this transaction).  An implementation may skip,
    /// for them, what guards against other transactions' writes; the
    /// default ignores the declaration.  The caller answers for it.
    fn fresh(&mut self, start: &[u8], end: &[u8]) {
        let _ = (start, end);
    }

    /// Commits atomically; returns the commit timestamp.  On error nothing
    /// was written.
    fn commit(self: Box<Self>) -> Result<Timestamp>;

    /// Discards the transaction's writes.
    fn rollback(self: Box<Self>);
}

/// The keys starting with `prefix`, as scan bounds' upper end: the smallest
/// key greater than every key with that prefix (`None` if there is none —
/// the prefix is all `FF` bytes, so scan to the end).
pub fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xFF {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

/// Runs `f` in a transaction and commits, retrying from the start on
/// retryable errors (up to 100 times), with [`Kv::back_off`] between
/// attempts.
pub fn transact<T>(
    kv: &dyn Kv,
    opts: TxnOptions,
    mut f: impl FnMut(&mut dyn KvTxn) -> Result<T>,
) -> Result<T> {
    let mut attempt: u32 = 0;
    loop {
        let mut txn = kv.begin(opts)?;
        let result = f(txn.as_mut()).and_then(|v| txn.commit().map(|_| v));
        match result {
            Err(e) if e.is_retryable() && attempt < 100 => {
                attempt += 1;
                kv.back_off(attempt);
            }
            other => return other,
        }
    }
}

/// Tuning for a [`Database`].
#[derive(Clone, Debug)]
pub struct Options {
    /// Page cache size in bytes (rounded down to 8 KiB pages; at least 16),
    /// allocated as pages are first cached.
    pub cache_bytes: usize,
    /// Checkpoint once the WAL holds this many bytes.
    pub checkpoint_wal_bytes: u64,
    /// WAL segment size in bytes.
    pub wal_segment_bytes: u64,
    /// Old versions kept, in commits, beyond what running transactions need.
    pub history: u64,
    /// A transaction's writes kept in memory, in bytes; past it they spill
    /// into the store, so a transaction of any size runs in bounded memory.
    pub txn_memory_bytes: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            // 128 MiB, or 16 MiB in a 32-bit address space.
            cache_bytes: if cfg!(target_pointer_width = "64") {
                128 << 20
            } else {
                16 << 20
            },
            checkpoint_wal_bytes: 64 << 20,
            wal_segment_bytes: 16 << 20,
            history: 0,
            txn_memory_bytes: 64 << 20,
        }
    }
}

/// The local, embedded implementation of [`Kv`]: one directory on disk.
/// Cloning gives another handle to the same database.
#[derive(Clone)]
pub struct Database {
    engine: nucleus_txn::Engine,
    platform: Arc<dyn Platform>,
    /// Where scratch files go (`<dir>/scratch`), and how many were made.
    scratch_dir: PathBuf,
    scratch_made: Arc<AtomicU64>,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("engine", &self.engine)
            .finish_non_exhaustive()
    }
}

impl Database {
    /// Opens (or creates) the database in directory `path`.
    pub fn open(path: impl AsRef<Path>, opts: Options) -> Result<Self> {
        let platform = RealPlatform::new().map_err(|e| Error::Storage(Box::new(e)))?;
        Self::open_on(Arc::new(platform), path.as_ref(), opts)
    }

    /// Opens (or creates) the database in `path` on `platform` (the
    /// simulation, in tests).
    pub fn open_on(platform: Arc<dyn Platform>, path: &Path, opts: Options) -> Result<Self> {
        let engine = nucleus_txn::Engine::open(
            platform.clone(),
            path,
            nucleus_txn::EngineOptions {
                store: StoreOptions {
                    cache_pages: (opts.cache_bytes / nucleus_storage::PAGE_SIZE).max(16),
                    wal_segment_size: opts.wal_segment_bytes,
                },
                checkpoint_wal_bytes: opts.checkpoint_wal_bytes,
                history: opts.history,
                txn_memory: opts.txn_memory_bytes,
            },
        )?;
        // Scratch files a crash left behind.
        let scratch_dir = path.join("scratch");
        let fs = platform.fs();
        if fs
            .exists(&scratch_dir)
            .map_err(|e| Error::Storage(Box::new(e)))?
        {
            for name in fs
                .list_dir(&scratch_dir)
                .map_err(|e| Error::Storage(Box::new(e)))?
            {
                let _ = fs.remove_file(&scratch_dir.join(name));
            }
        }
        Ok(Self {
            engine,
            platform,
            scratch_dir,
            scratch_made: Arc::new(AtomicU64::new(0)),
        })
    }

    /// How many scratch files this handle (and its clones) made — for
    /// tests that work spilled out of memory.
    pub fn scratch_files_made(&self) -> u64 {
        self.scratch_made.load(Ordering::Relaxed)
    }

    /// Writes everything to the data file and trims the WAL.
    pub fn checkpoint(&self) -> Result<()> {
        Ok(self.engine.checkpoint()?)
    }

    /// Removes versions no snapshot can see; returns how many.
    pub fn vacuum(&self) -> Result<u64> {
        Ok(self.engine.vacuum()?)
    }

    /// The newest committed snapshot.
    pub fn latest(&self) -> Timestamp {
        self.engine.visible()
    }

    /// Checkpoints and closes this handle.
    pub fn close(self) -> Result<()> {
        self.checkpoint()
    }
}

impl Kv for Database {
    fn begin(&self, opts: TxnOptions) -> Result<Box<dyn KvTxn>> {
        let txn = self.engine.begin_with(nucleus_txn::TxnOptions {
            isolation: opts.isolation,
            read_only: opts.read_only,
            at: opts.at,
        })?;
        Ok(Box::new(LocalTxn(txn)))
    }

    fn wall_clock(&self) -> SystemTime {
        self.platform.clock().wall()
    }

    fn elapsed(&self) -> Duration {
        self.platform.clock().now()
    }

    fn random_bytes(&self, buf: &mut [u8]) {
        self.platform.rng().fill_unpredictable(buf);
    }

    fn parallelism(&self) -> usize {
        self.platform.parallelism()
    }

    fn vacuum(&self) -> Result<u64> {
        Database::vacuum(self)
    }

    fn vacuum_background(&self, pause: &mut dyn FnMut()) -> Result<u64> {
        Ok(self
            .engine
            .vacuum_paced(nucleus_txn::BACKGROUND_CHUNK, pause)?)
    }

    fn vacuum_wanted(&self) -> bool {
        self.engine.intents_pending()
    }

    fn scratch(&self) -> Result<Box<dyn Scratch>> {
        let fs = self.platform.fs();
        fs.create_dir_all(&self.scratch_dir)
            .map_err(|e| Error::Storage(Box::new(e)))?;
        let n = self.scratch_made.fetch_add(1, Ordering::Relaxed);
        let mut id = [0u8; 8];
        self.platform.rng().fill_bytes(&mut id);
        let path = self
            .scratch_dir
            .join(format!("{n}-{:016x}", u64::from_le_bytes(id)));
        let file = fs
            .open(&path, nucleus_platform::OpenMode::CreateNew)
            .map_err(|e| Error::Storage(Box::new(e)))?;
        Ok(Box::new(FileScratch {
            file,
            path,
            platform: Arc::clone(&self.platform),
        }))
    }

    /// From 2 µs, doubling up to about 1 ms, plus up to as much again at
    /// random.
    fn back_off(&self, attempt: u32) {
        let base = 1u64 << attempt.clamp(1, 10);
        let jitter = self.platform.rng().below(base);
        self.platform
            .clock()
            .sleep(Duration::from_micros(base + jitter));
    }
}

struct LocalTxn(nucleus_txn::Transaction);

/// The fewest leaves a part of a split range has: about 64 × 4 KiB, a few
/// thousand small rows — less is not worth a thread.
const SPLIT_MIN_LEAVES: usize = 64;

impl KvRead for LocalTxn {
    fn read_ts(&self) -> Timestamp {
        self.0.read_ts()
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get(key)?)
    }

    fn scan<'a>(&'a self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<KvIter<'a>> {
        KvTxn::scan(self, start, end)
    }

    fn get_validated(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get_validated(key)?)
    }

    fn scan_validated<'a>(&'a self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<KvIter<'a>> {
        KvTxn::scan_validated(self, start, end)
    }

    fn scan_chunks<'a>(
        &'a self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Box<dyn KvChunks + 'a>> {
        KvTxn::scan_chunks(self, start, end)
    }

    fn scan_chunks_validated<'a>(
        &'a self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Box<dyn KvChunks + 'a>> {
        KvTxn::scan_chunks_validated(self, start, end)
    }
}

impl KvTxn for LocalTxn {
    fn read_ts(&self) -> Timestamp {
        self.0.read_ts()
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get(key)?)
    }

    fn scan<'a>(&'a self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<KvIter<'a>> {
        let scan = self.0.scan(start, end)?;
        Ok(Box::new(scan.map(|r| r.map_err(Error::from))))
    }

    fn first(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        Ok(self.0.first(start, end)?)
    }

    fn get_validated(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get_validated(key)?)
    }

    fn scan_validated<'a>(&'a self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<KvIter<'a>> {
        let scan = self.0.scan_validated(start, end)?;
        Ok(Box::new(scan.map(|r| r.map_err(Error::from))))
    }

    fn scan_chunks<'a>(
        &'a self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Box<dyn KvChunks + 'a>> {
        Ok(Box::new(ScanChunks(self.0.scan(start, end)?)))
    }

    fn scan_chunks_validated<'a>(
        &'a self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Box<dyn KvChunks + 'a>> {
        Ok(Box::new(ScanChunks(self.0.scan_validated(start, end)?)))
    }

    fn shared(&self) -> Option<&dyn KvRead> {
        Some(self)
    }

    fn split_points(&self, start: &[u8], end: Option<&[u8]>, parts: usize) -> Result<Vec<Vec<u8>>> {
        Ok(self.0.split_points(start, end, parts, SPLIT_MIN_LEAVES)?)
    }

    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        Ok(self.0.put(key, value)?)
    }

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        Ok(self.0.delete(key)?)
    }

    fn fresh(&mut self, start: &[u8], end: &[u8]) {
        self.0.fresh(start, end);
    }

    fn put_new(&mut self, key: &[u8], value: &[u8]) -> Result<bool> {
        Ok(self.0.put_new(key, value)?)
    }

    fn commit(self: Box<Self>) -> Result<Timestamp> {
        Ok(self.0.commit()?)
    }

    fn rollback(self: Box<Self>) {
        self.0.rollback();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_ends() {
        assert_eq!(prefix_end(b"ab"), Some(b"ac".to_vec()));
        assert_eq!(prefix_end(b"a\xff"), Some(b"b".to_vec()));
        assert_eq!(prefix_end(b"\xff\xff"), None);
        assert_eq!(prefix_end(b""), None);
    }
}
