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

/// A range of keys: from, to (both may be unbounded).
pub type KeyRange = (Bound<Vec<u8>>, Bound<Vec<u8>>);
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use nucleus_platform::Platform;
use nucleus_platform::real::RealPlatform;
use nucleus_storage::StoreOptions;

mod sealed_scratch;

pub use nucleus_platform::SyncPolicy;
/// The key an encrypted database is sealed under ([`Options::encryption_key`]).
pub use nucleus_storage::PageCipher;
pub use nucleus_storage::Rekey;
/// LZ4-block compression (a backup's parts are compressed with it).
pub use nucleus_storage::lz;
/// Finding bytes in a database's files ([`Kv::find_bytes`]'s search, for tools
/// and tests that want where).
pub use nucleus_storage::scan;
/// Pages a background step of sealing again under a new key takes
/// (PLAN.md §M14.30 K3f).
pub use nucleus_storage::store::RESEAL_BATCH;
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

/// The main branch's name (PLAN.md §M14.13).
pub const MAIN_BRANCH: &str = nucleus_txn::MAIN_NAME;

pub use nucleus_storage::valid_branch_name;

/// A branch of a database ([`Kv::branches`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    /// Its name.
    pub name: String,
    /// The branch it was made from (`None`: the main one).
    pub parent: Option<String>,
}

fn no_branches(name: &str) -> Error {
    Error::Storage(format!("no branch \"{name}\": this database has no branches").into())
}

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

    /// [`next_chunk`](Self::next_chunk) of at most `entries` entries whose
    /// keys `want` takes (`None`: any), the others passed over by their keys
    /// where the scan can — their values not read — and counted in
    /// `skipped`; once `most` are passed over the chunk ends, empty
    /// perhaps, with more to come (true).  By default the usual chunk,
    /// every entry in it.
    fn next_chunk_where(
        &mut self,
        out: &mut Chunk,
        entries: usize,
        want: Option<&dyn Fn(&[u8]) -> bool>,
        skipped: &mut usize,
        most: usize,
    ) -> Result<bool> {
        let _ = (entries, want, skipped, most);
        self.next_chunk(out)
    }

    /// Moves forward to the first entry at or after `key` — the entries
    /// before it not read, where the scan can seek (`false`: it cannot,
    /// and reads on).  The chunk asked for next starts there.
    fn seek(&mut self, key: &[u8]) -> Result<bool> {
        let _ = key;
        Ok(false)
    }
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

    fn next_chunk_where(
        &mut self,
        out: &mut Chunk,
        entries: usize,
        want: Option<&dyn Fn(&[u8]) -> bool>,
        skipped: &mut usize,
        most: usize,
    ) -> Result<bool> {
        Ok(self.0.next_chunk_where(
            out,
            entries.clamp(1, CHUNK_ENTRIES),
            CHUNK_BYTES,
            want,
            skipped,
            most,
        )?)
    }

    fn seek(&mut self, key: &[u8]) -> Result<bool> {
        self.0.seek(key);
        Ok(true)
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
    /// 1): until the commits it may have lost to are visible, then
    /// exponentially longer each time, with jitter, so retries that
    /// collided once spread out.  Through the implementation's platform, so
    /// a simulation does not wait in real time.
    fn back_off(&self, attempt: u32);

    /// Waits `d`: on the platform's clock, so a simulation passes the time
    /// without waiting for it (by default, the host's sleep).
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }

    /// Fills `buf` with unpredictable bytes (`gen_random_uuid()`): the
    /// platform's (the operating system's generator; a simulation's seed).
    fn random_bytes(&self, buf: &mut [u8]);

    /// How many threads a statement's work can usefully be split across
    /// (parallel scans, §M14.20); 1 by default.
    fn parallelism(&self) -> usize {
        1
    }

    /// Which open database this is, unique in the process — what state a
    /// layer above keeps in memory for one database is keyed by (an HNSW
    /// index's upkeep, §M14.12 V4b); 0 (the default): not told apart.
    /// A transaction's [`KvTxn::instance`] is its database's.
    fn instance(&self) -> u64 {
        0
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

    /// [`vacuum`](Kv::vacuum) over the keys from `start` to `end`
    /// (exclusive) alone, leaving keys a running large transaction holds:
    /// for a range used as a queue (keys put, then deleted), whose scans
    /// would otherwise step over the deleted keys until the whole vacuum
    /// runs.  How many entries it removed.  The default: `0` (nothing to
    /// remove).
    fn vacuum_range(&self, start: &[u8], end: &[u8]) -> Result<u64> {
        let _ = (start, end);
        Ok(0)
    }

    /// The snapshot of `time`: that of the last commit at or before it
    /// (PLAN.md §M14.14), to begin a transaction at
    /// ([`TxnOptions::at`]) — the newest for a time past the last commit.
    /// [`Error::SnapshotTooOld`] when no commit at or before it is known
    /// (no retention by time, or past it).  The default: none known.
    fn snapshot_at(&self, time: SystemTime) -> Result<Timestamp> {
        let _ = time;
        Err(Error::SnapshotTooOld)
    }

    /// The time commit `ts` was made, if it is known (a retention by time
    /// set, and not past it).  The default: none known.
    fn commit_time(&self, ts: Timestamp) -> Result<Option<SystemTime>> {
        let _ = ts;
        Ok(None)
    }

    /// How far back a time reaches: the oldest snapshot readable whose
    /// time is known, and that time.  The default: none.
    fn reach(&self) -> Result<Option<(Timestamp, SystemTime)>> {
        Ok(None)
    }

    /// The retention by time ([`Options::history_time`]).  The default:
    /// none (zero).
    fn history_time(&self) -> Duration {
        Duration::ZERO
    }

    /// The newest snapshot: that of the last commit.  The default: none
    /// known.
    fn newest(&self) -> Option<Timestamp> {
        None
    }

    /// [`vacuum`](Kv::vacuum) as a background task: in small batches,
    /// `go_on` asked between them — `false` stops it there, and the next
    /// call goes on from it.  The default: `vacuum`.
    fn vacuum_background(&self, go_on: &mut dyn FnMut() -> bool) -> Result<u64> {
        let _ = go_on;
        self.vacuum()
    }

    /// How many transactions have begun so far (unchanged over a while,
    /// with none [`running`](Kv::running): the database was idle).  The
    /// default: 0.
    fn activity(&self) -> u64 {
        0
    }

    /// How many transactions are running.  The default: 0.
    fn running(&self) -> usize {
        0
    }

    /// This handle's branch of the database (PLAN.md §M14.13): its name
    /// ([`MAIN_BRANCH`] the main one).  The default: the main one.
    fn branch_name(&self) -> String {
        MAIN_BRANCH.to_owned()
    }

    /// Every branch: name and the parent's name, the main one first.  The
    /// default: the main one alone.
    fn branches(&self) -> Result<Vec<Branch>> {
        Ok(vec![Branch {
            name: MAIN_BRANCH.to_owned(),
            parent: None,
        }])
    }

    /// The database's branch `name`, to read and write as a database of
    /// its own.  The default: none but the main one.
    fn branch(&self, name: &str) -> Result<Arc<dyn Kv>> {
        Err(no_branches(name))
    }

    /// Makes branch `name` of this one as it is now: a copy in constant
    /// time and space, writes on either side invisible to the other.
    /// The default: not supported.
    fn create_branch(&self, name: &str) -> Result<Arc<dyn Kv>> {
        Err(no_branches(name))
    }

    /// [`create_branch`](Kv::create_branch) as this one was at snapshot
    /// `at` (one the history kept still reaches: else
    /// [`Error::SnapshotTooOld`]); costs a pass over its entries.  The
    /// default: not supported.
    fn create_branch_at(&self, name: &str, at: Timestamp) -> Result<Arc<dyn Kv>> {
        let _ = at;
        Err(no_branches(name))
    }

    /// Drops branch `name` (not the main one, nor one with branches of its
    /// own); its handles fail from then on.  The default: not supported.
    fn drop_branch(&self, name: &str) -> Result<()> {
        Err(no_branches(name))
    }

    /// Makes branch `name` (one without branches) its parent as it is
    /// now; its old handles fail.  The default: not supported.
    fn reset_branch(&self, name: &str) -> Result<Arc<dyn Kv>> {
        Err(no_branches(name))
    }

    /// The keys whose newest values may differ between this branch and
    /// branch `base` (PLAN.md §M14.18 O3, an incremental backup's): ranges
    /// of them, ascending and apart, found from what the two branches do
    /// not share — what changed since base was made, and little more.
    /// `None` when it cannot tell (compare all then); the default.
    fn changed_since(&self, base: &str) -> Result<Option<Vec<KeyRange>>> {
        let _ = base;
        Ok(None)
    }

    /// Whether [`vacuum`](Kv::vacuum) has intents to make versions (reads
    /// resolve them meanwhile, for a little more).  The default: no.
    fn vacuum_wanted(&self) -> bool {
        false
    }

    /// Makes `key` the key the database's files are sealed under (PLAN.md
    /// §M14.30 K3f), the database open and in use: everything written
    /// from now on is sealed under it, and what the old key sealed is
    /// sealed again in the background ([`reseal_background`]) — until
    /// then the database needs both keys to open
    /// ([`Options::previous_encryption_key`]).  The default: an error
    /// (no files of its own to seal).
    ///
    /// [`reseal_background`]: Kv::reseal_background
    fn rekey(&self, key: PageCipher) -> Result<()> {
        let _ = key;
        Err(Error::Storage(
            "this database has no encrypted files to give a new key".into(),
        ))
    }

    /// Where sealing again under a new key stands ([`Rekey::Idle`]: none
    /// runs — the old key no longer needed).  The default: idle.
    fn rekey_state(&self) -> Rekey {
        Rekey::Idle
    }

    /// Seals again under the new key what the old one sealed, as a
    /// background task: [`RESEAL_BATCH`] pages at a time, `go_on` asked
    /// between batches — `false` stops it there, and the next call goes
    /// on from it.  Where it stands after.  The default: idle.
    fn reseal_background(&self, go_on: &mut dyn FnMut() -> bool) -> Result<Rekey> {
        let _ = go_on;
        Ok(Rekey::Idle)
    }

    /// Watches the keys from `start` to `end` (exclusive) for
    /// [`KvTxn::watched_as_at`]: what a caller derives from them (a
    /// catalog) may be kept across transactions while none commits a
    /// write there.  The default watches nothing (and every transaction
    /// answers `false`).
    fn watch(&self, start: &[u8], end: &[u8]) {
        let _ = (start, end);
    }

    /// Erasure (PLAN.md §M14.24 R2): once no snapshot older than now
    /// runs (waiting up to `wait`), every version of the keys in `ranges`
    /// (each `[start, end)`) no snapshot can see removed, past any history
    /// kept, and what the files then hold no longer — free pages, the
    /// log's dropped segments — overwritten with zeros.  `None` from an
    /// implementation with no files of its own (the default: nothing kept
    /// that it could remove).
    fn scrub(&self, ranges: &[(Vec<u8>, Vec<u8>)], wait: Duration) -> Result<Option<Scrub>> {
        let _ = (ranges, wait);
        Ok(None)
    }

    /// The file system its files are on, for files written beside them
    /// — a backup's (PLAN.md §M14.24 R6).  `None` (the default) from an
    /// implementation with no files of its own.
    fn files(&self) -> Option<&dyn nucleus_platform::FileSystem> {
        None
    }

    /// Where in its files any of `needles` (four bytes or more) is: one
    /// place per file offset, or per page of the data file, its leaves
    /// decoded ([`nucleus_storage::scan`]).  `None` from an
    /// implementation with no files of its own (the default).
    fn find_bytes(&self, needles: &[Vec<u8>]) -> Result<Option<u64>> {
        let _ = needles;
        Ok(None)
    }
}

/// What [`Kv::scrub`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scrub {
    /// Whether it ran: `false` when a snapshot older than the call was
    /// still running after the wait (nothing was removed).
    pub complete: bool,
    /// Versions removed.
    pub versions: u64,
    /// Free pages zeroed.
    pub pages: u64,
    /// Bytes of the log's dropped segments zeroed.
    pub wal_bytes: u64,
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
    /// [`KvTxn::first`] — without the read-ahead, and remembering nothing
    /// the threads sharing the reads would contend for (a lookup's
    /// segment, asked once a row on each worker: PLAN.md §M14.20 W6).  By
    /// default the scan's first.
    fn first(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        self.scan(start, end)?.next().transpose()
    }
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

/// A transaction behind a lock, its reads shared by other threads while
/// its owner writes between them (a load parsing beside its stores:
/// PLAN.md §M14.20 W3b).  Each read takes the lock — a scan is read whole
/// under it —, so a read sees the transaction as it is between two of the
/// owner's writes, as a read on the owner's thread would.
pub struct Locked<'t>(std::sync::Mutex<&'t mut dyn KvTxn>);

impl<'t> Locked<'t> {
    /// `txn`, locked.
    pub fn new(txn: &'t mut dyn KvTxn) -> Self {
        Self(std::sync::Mutex::new(txn))
    }

    /// The transaction, held until the guard drops (the owner's writes).
    pub fn lock(&self) -> std::sync::MutexGuard<'_, &'t mut dyn KvTxn> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl KvRead for Locked<'_> {
    fn read_ts(&self) -> Timestamp {
        self.lock().read_ts()
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.lock().get(key)
    }

    fn scan<'a>(&'a self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<KvIter<'a>> {
        let all: Vec<_> = self.lock().scan(start, end)?.collect();
        Ok(Box::new(all.into_iter()))
    }

    fn first(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        self.lock().first(start, end)
    }

    fn get_validated(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.lock().get_validated(key)
    }

    fn scan_validated<'a>(&'a self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<KvIter<'a>> {
        let all: Vec<_> = self.lock().scan_validated(start, end)?.collect();
        Ok(Box::new(all.into_iter()))
    }

    fn scan_chunks<'a>(
        &'a self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Box<dyn KvChunks + 'a>> {
        Ok(Box::new(IterChunks(self.scan(start, end)?)))
    }

    fn scan_chunks_validated<'a>(
        &'a self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Box<dyn KvChunks + 'a>> {
        Ok(Box::new(IterChunks(self.scan_validated(start, end)?)))
    }
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

    /// Its database's [`Kv::instance`].
    fn instance(&self) -> u64 {
        0
    }

    /// The first entry of the range `[start, end)` (bounds as [`scan`]'s),
    /// for a reader that wants just it: by default the scan's first; an
    /// implementation may skip the read-ahead a scan does, and may answer
    /// a look inside a range it found empty before from what the
    /// transaction wrote since (the engine's transactions do: a load's
    /// "anything at or after this key?", asked once a row, reads the store
    /// once).
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

    /// Declares the keys in `[start, end)` fresh: none has a version yet,
    /// no other transaction writes one before this one ends, and none
    /// reads them before it commits (a new index's keys, a table made in
    /// this transaction).  An implementation may skip, for them, what
    /// guards against other transactions — locally the conflict checks,
    /// and the WAL (PLAN.md D16) —; the default ignores the declaration.
    /// The caller answers for it.
    fn fresh(&mut self, start: &[u8], end: &[u8]) {
        let _ = (start, end);
    }

    /// The engine beside the default one that holds keys in `[start,
    /// end)`, if one does (PLAN.md §M14.28 L3): its name (`lsm`).  The
    /// default: none.
    fn engine_of(&self, start: &[u8], end: &[u8]) -> Result<Option<String>> {
        let _ = (start, end);
        Ok(None)
    }

    /// Gives the keys in `[start, end)` to a new engine named `kind`
    /// (`lsm`), durable at once whatever becomes of this transaction;
    /// false, nothing done, when the range is not free — a key there has
    /// a version, or another engine's range meets it.  The default: an
    /// error, the implementation keeping every key in one engine.
    fn add_engine(&mut self, start: &[u8], end: &[u8], kind: &str) -> Result<bool> {
        let _ = (start, end);
        Err(Error::Storage(
            format!("no engine \"{kind}\" here: this store keeps every key in one").into(),
        ))
    }

    /// Whether this transaction reads the [watched](Kv::watch) keys as a
    /// snapshot at `at` read them — so what was derived from them there
    /// holds here: no commit between the two snapshots wrote one, nor did
    /// this transaction, and its reads need not be recorded (not a
    /// serializable transaction that writes).  The default: no.
    fn watched_as_at(&self, at: Timestamp) -> bool {
        let _ = at;
        false
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
    /// Old versions kept for this long, so the snapshot of any time that
    /// far back stays readable ([`Kv::snapshot_at`]; each commit's time is
    /// recorded while it is set; PLAN.md §M14.14).  Zero: not by time.
    pub history_time: Duration,
    /// A transaction's writes kept in memory, in bytes; past it they spill
    /// into the store, so a transaction of any size runs in bounded memory.
    pub txn_memory_bytes: usize,
    /// How far [`Database::open`]'s syncs go: ordered writes (the default)
    /// or the drive's cache flushed too ([`SyncPolicy`], PLAN.md D17).
    /// [`Database::open_on`] takes its platform's.
    pub sync: SyncPolicy,
    /// Bulk writes into ranges a transaction declared fresh (a new index's
    /// keys) skip the WAL, made durable by a checkpoint at the commit
    /// (PLAN.md D16; the default).  Off, every write is logged.
    pub wal_minimal: bool,
    /// The key the data file's pages (PLAN.md §M14.30 K2b) and the WAL's
    /// records (K2c) are sealed under: a new database is created
    /// encrypted, an existing one must have been made under it; its
    /// scratch files are sealed too, each under a key of its own (K2d).
    /// `None` (the default): in the clear.
    pub encryption_key: Option<PageCipher>,
    /// The key before `encryption_key`, while what it sealed is sealed
    /// again under it (PLAN.md §M14.30 K3f, [`Kv::rekey`]): a database
    /// whose files name it needs it, and one still under it is given
    /// `encryption_key` as it opens.  `None` (the default): none.
    pub previous_encryption_key: Option<PageCipher>,
}

impl Options {
    /// The defaults for a database in memory ([`Database::open_in_memory`]):
    /// its WAL is memory too, so checkpoints come sooner and segments are
    /// smaller.  The page cache stays as large as a file's: a page it
    /// misses is copied from the memory files and decoded again (a 32 MiB
    /// cache made a million-row scan twice as slow).
    pub fn in_memory() -> Self {
        Self {
            checkpoint_wal_bytes: 16 << 20,
            wal_segment_bytes: 1 << 20,
            ..Self::default()
        }
    }
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
            // 1 GiB (PostgreSQL's `max_wal_size`), or 64 MiB on 32-bit
            // targets: more WAL between checkpoints, fewer of them.
            checkpoint_wal_bytes: if cfg!(target_pointer_width = "64") {
                1 << 30
            } else {
                64 << 20
            },
            wal_segment_bytes: 16 << 20,
            history: 0,
            history_time: Duration::ZERO,
            txn_memory_bytes: 64 << 20,
            sync: SyncPolicy::default(),
            wal_minimal: true,
            encryption_key: None,
            previous_encryption_key: None,
        }
    }
}

/// The local, embedded implementation of [`Kv`]: one directory on disk.
/// Cloning gives another handle to the same database.
#[derive(Clone)]
pub struct Database {
    engine: nucleus_txn::Engine,
    platform: Arc<dyn Platform>,
    /// Its directory.
    dir: PathBuf,
    /// Where scratch files go (`<dir>/scratch`), and how many were made.
    scratch_dir: PathBuf,
    scratch_made: Arc<AtomicU64>,
    /// The key its pages and logs are sealed under (PLAN.md §M14.30
    /// K2b–K2c): its scratch files are sealed too (K2d), and the search
    /// for erased bytes opens what it reads (K2e).
    page_key: Option<PageCipher>,
    /// [`Kv::instance`]: one per open and branch (its clones share it).
    instance: u64,
    /// The instances of the branches' handles, by branch id: the same
    /// branch is the same instance, whoever opens it.
    instances: Arc<std::sync::Mutex<std::collections::BTreeMap<u32, u64>>>,
    /// Its metrics' collector, read while a handle is open (PLAN.md
    /// §M14.18).
    _metrics: Arc<nucleus_platform::metrics::Collector>,
}

static OPENED: AtomicU64 = AtomicU64::new(0);

/// Adds the storage and transaction layers' instruments to the process's
/// metrics registry (PLAN.md §M14.18); opening a database does too.
pub fn register_metrics() {
    nucleus_txn::metrics::register();
}

/// The lower layers' instruments registered, and a collector of what
/// `engine`'s store already counts — read at each snapshot, not counted
/// twice (PLAN.md §M14.18).
fn collect_metrics(engine: nucleus_txn::Engine) -> nucleus_platform::metrics::Collector {
    nucleus_txn::metrics::register();
    nucleus_platform::metrics::global().collector(move |s| {
        let store = engine.store();
        let pool = store.pool();
        let st = pool.stats();
        s.counter(
            "nucleus_cache_hits_total",
            "Page reads the page cache answered.",
            &[],
            st.hits,
        );
        s.counter(
            "nucleus_cache_misses_total",
            "Page reads that went to the data file.",
            &[],
            st.misses,
        );
        s.counter(
            "nucleus_cache_evictions_total",
            "Cache frames reused for another page.",
            &[],
            st.evictions,
        );
        s.counter(
            "nucleus_cache_writebacks_total",
            "Dirty pages written back to the data file.",
            &[],
            st.writebacks,
        );
        s.gauge(
            "nucleus_cache_pages",
            "Pages the page cache can hold.",
            &[],
            pool.capacity() as f64,
        );
        s.gauge(
            "nucleus_cache_pages_used",
            "Cache frames holding a page.",
            &[],
            pool.frames_made() as f64,
        );
        let pager = pool.pager();
        let (pages, free) = (pager.page_count(), pager.free_count());
        s.gauge(
            "nucleus_data_file_bytes",
            "The data file's size (every branch's pages).",
            &[],
            (pages * nucleus_storage::PAGE_SIZE as u64) as f64,
        );
        s.gauge(
            "nucleus_data_file_free_bytes",
            "Free pages in the data file, reused before it grows.",
            &[],
            (free * nucleus_storage::PAGE_SIZE as u64) as f64,
        );
        s.gauge(
            "nucleus_wal_since_checkpoint_bytes",
            "The write-ahead log written since the last checkpoint (main branch).",
            &[],
            store.wal_since_checkpoint() as f64,
        );
        s.gauge(
            "nucleus_transactions_running",
            "Transactions running now.",
            &[],
            engine.running() as f64,
        );
        s.gauge(
            "nucleus_oldest_transaction_seconds",
            "How long the oldest running transaction has run (0 when none runs).",
            &[],
            engine.oldest_running().map_or(0.0, |d| d.as_secs_f64()),
        );
        s.gauge(
            "nucleus_branches",
            "The database's branches, main included.",
            &[],
            engine.branches().len() as f64,
        );
    })
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("engine", &self.engine)
            .finish_non_exhaustive()
    }
}

/// The store's options from `opts`.
fn store_options(opts: &Options) -> StoreOptions {
    StoreOptions {
        cache_pages: (opts.cache_bytes / nucleus_storage::PAGE_SIZE).max(16),
        wal_segment_size: opts.wal_segment_bytes,
        page_key: opts.encryption_key.clone(),
        previous_page_key: opts.previous_encryption_key.clone(),
    }
}

/// Whether the database in directory `dir` is encrypted — its data file's
/// header says, no key needed —, `None` when there is none there (PLAN.md
/// §M14.30 K2e).
pub fn is_encrypted(dir: impl AsRef<Path>) -> Result<Option<bool>> {
    let platform = RealPlatform::new().map_err(|e| Error::Storage(Box::new(e)))?;
    let data = dir.as_ref().join("data");
    nucleus_storage::pager::file_cipher(platform.fs(), &data)
        .map(|c| c.map(|c| c != nucleus_storage::cipher::CIPHER_NONE))
        .map_err(|e| Error::Storage(Box::new(e)))
}

/// Encrypts the database in directory `dir` — in the clear, not open
/// anywhere — under `key` (PLAN.md §M14.30 K2e;
/// [`Store::encrypt`](nucleus_storage::Store::encrypt)): recovered and
/// scrubbed in the clear, its pages sealed into a copy renamed over its
/// data file, its logs started again sealed.  A crash leaves it in the
/// clear or encrypted; run again, it finishes.  The pages sealed.
pub fn encrypt(dir: impl AsRef<Path>, opts: &Options, key: &PageCipher) -> Result<u64> {
    let platform: Arc<dyn Platform> =
        Arc::new(RealPlatform::with_sync(opts.sync).map_err(|e| Error::Storage(Box::new(e)))?);
    nucleus_storage::Store::encrypt(platform, dir.as_ref(), &store_options(opts), key)
        .map_err(|e| Error::Storage(Box::new(e)))
}

impl Database {
    /// Opens (or creates) the database in directory `path`.
    pub fn open(path: impl AsRef<Path>, opts: Options) -> Result<Self> {
        let platform =
            RealPlatform::with_sync(opts.sync).map_err(|e| Error::Storage(Box::new(e)))?;
        Self::open_on(Arc::new(platform), path.as_ref(), opts)
    }

    /// A new database in memory, gone with its last handle — the
    /// embedded editions' and the CLI's `:memory:` (`opts` usually
    /// [`Options::in_memory`]).
    pub fn open_in_memory(opts: Options) -> Result<Self> {
        let platform = nucleus_platform::memory::MemoryPlatform::new()
            .map_err(|e| Error::Storage(Box::new(e)))?;
        Self::open_on(Arc::new(platform), Path::new("/db"), opts)
    }

    /// Opens (or creates) the database in `path` on `platform` (the
    /// simulation, in tests).
    pub fn open_on(platform: Arc<dyn Platform>, path: &Path, opts: Options) -> Result<Self> {
        let page_key = opts.encryption_key.clone();
        let engine = nucleus_txn::Engine::open(
            platform.clone(),
            path,
            nucleus_txn::EngineOptions {
                store: store_options(&opts),
                checkpoint_wal_bytes: opts.checkpoint_wal_bytes,
                history: opts.history,
                history_time: u64::try_from(opts.history_time.as_micros()).unwrap_or(u64::MAX),
                txn_memory: opts.txn_memory_bytes,
                wal_minimal: opts.wal_minimal,
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
        let instance = OPENED.fetch_add(1, Ordering::Relaxed) + 1;
        let metrics = Arc::new(collect_metrics(engine.clone()));
        Ok(Self {
            engine,
            platform,
            dir: path.to_path_buf(),
            scratch_dir,
            scratch_made: Arc::new(AtomicU64::new(0)),
            page_key,
            instance,
            instances: Arc::new(std::sync::Mutex::new(
                [(nucleus_storage::MAIN, instance)].into(),
            )),
            _metrics: metrics,
        })
    }

    /// A handle to `engine`'s branch, sharing this one's files.
    fn on_branch(&self, engine: nucleus_txn::Engine) -> Self {
        let id = engine.branch_info().id;
        let instance = *self
            .instances
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(id)
            .or_insert_with(|| OPENED.fetch_add(1, Ordering::Relaxed) + 1);
        Self {
            engine,
            instance,
            ..self.clone()
        }
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

    /// The data file's pages and how many of them are free — its
    /// branches' together (they share the file).  A checkpoint settles
    /// the count: pages freed since become free at the next.
    pub fn pages(&self) -> (u64, u64) {
        let pager = self.engine.store().pool().pager();
        (pager.page_count(), pager.free_count())
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
    fn branch_name(&self) -> String {
        self.engine.branch_info().name.clone()
    }

    fn branches(&self) -> Result<Vec<Branch>> {
        let all = self.engine.branches();
        let name = |id: u32| all.iter().find(|b| b.id == id).map(|b| b.name.clone());
        Ok(all
            .iter()
            .map(|b| Branch {
                name: b.name.clone(),
                parent: b.parent.and_then(name),
            })
            .collect())
    }

    fn branch(&self, name: &str) -> Result<Arc<dyn Kv>> {
        Ok(Arc::new(self.on_branch(self.engine.branch(name)?)))
    }

    fn create_branch(&self, name: &str) -> Result<Arc<dyn Kv>> {
        Ok(Arc::new(self.on_branch(self.engine.create_branch(name)?)))
    }

    fn create_branch_at(&self, name: &str, at: Timestamp) -> Result<Arc<dyn Kv>> {
        Ok(Arc::new(
            self.on_branch(self.engine.create_branch_at(name, at)?),
        ))
    }

    fn drop_branch(&self, name: &str) -> Result<()> {
        Ok(self.engine.drop_branch(name)?)
    }

    fn reset_branch(&self, name: &str) -> Result<Arc<dyn Kv>> {
        Ok(Arc::new(self.on_branch(self.engine.reset_branch(name)?)))
    }

    fn changed_since(&self, base: &str) -> Result<Option<Vec<KeyRange>>> {
        let base = self.engine.branch(base)?;
        Ok(self.engine.changed_since(&base)?)
    }

    fn begin(&self, opts: TxnOptions) -> Result<Box<dyn KvTxn>> {
        let txn = self.engine.begin_with(nucleus_txn::TxnOptions {
            isolation: opts.isolation,
            read_only: opts.read_only,
            at: opts.at,
        })?;
        Ok(Box::new(LocalTxn(txn, self.instance)))
    }

    fn instance(&self) -> u64 {
        self.instance
    }

    fn wall_clock(&self) -> SystemTime {
        self.platform.clock().wall()
    }

    fn elapsed(&self) -> Duration {
        self.platform.clock().now()
    }

    fn sleep(&self, d: Duration) {
        self.platform.clock().sleep(d);
    }

    fn random_bytes(&self, buf: &mut [u8]) {
        self.platform.rng().fill_unpredictable(buf);
    }

    fn files(&self) -> Option<&dyn nucleus_platform::FileSystem> {
        Some(self.platform.fs())
    }

    fn parallelism(&self) -> usize {
        self.platform.parallelism()
    }

    fn vacuum(&self) -> Result<u64> {
        Database::vacuum(self)
    }

    fn vacuum_range(&self, start: &[u8], end: &[u8]) -> Result<u64> {
        Ok(self.engine.vacuum_range(start, end)?)
    }

    fn snapshot_at(&self, time: SystemTime) -> Result<Timestamp> {
        Ok(self.engine.snapshot_at(time)?)
    }

    fn commit_time(&self, ts: Timestamp) -> Result<Option<SystemTime>> {
        Ok(self
            .engine
            .commit_time(ts)?
            .map(|us| SystemTime::UNIX_EPOCH + Duration::from_micros(us)))
    }

    fn reach(&self) -> Result<Option<(Timestamp, SystemTime)>> {
        Ok(self
            .engine
            .reach()?
            .map(|(ts, us)| (ts, SystemTime::UNIX_EPOCH + Duration::from_micros(us))))
    }

    fn history_time(&self) -> Duration {
        Duration::from_micros(self.engine.history_time())
    }

    fn newest(&self) -> Option<Timestamp> {
        Some(self.engine.visible())
    }

    fn vacuum_background(&self, go_on: &mut dyn FnMut() -> bool) -> Result<u64> {
        Ok(self
            .engine
            .vacuum_paced(nucleus_txn::BACKGROUND_CHUNK, go_on)?)
    }

    fn activity(&self) -> u64 {
        self.engine.activity()
    }

    fn running(&self) -> usize {
        self.engine.running()
    }

    fn vacuum_wanted(&self) -> bool {
        self.engine.intents_pending()
    }

    fn rekey(&self, key: PageCipher) -> Result<()> {
        Ok(self
            .engine
            .store()
            .rekey(key)
            .map_err(nucleus_txn::Error::from)?)
    }

    fn rekey_state(&self) -> Rekey {
        self.engine.store().rekey_state()
    }

    fn reseal_background(&self, go_on: &mut dyn FnMut() -> bool) -> Result<Rekey> {
        Ok(self
            .engine
            .store()
            .reseal(RESEAL_BATCH, go_on)
            .map_err(nucleus_txn::Error::from)?)
    }

    fn watch(&self, start: &[u8], end: &[u8]) {
        self.engine.watch(start, end);
    }

    fn scrub(&self, ranges: &[(Vec<u8>, Vec<u8>)], wait: Duration) -> Result<Option<Scrub>> {
        // Intents made versions, so none of the keys is left whole.
        if self.engine.intents_pending() {
            self.engine.vacuum()?;
        }
        let target = self.engine.visible();
        let clock = self.platform.clock();
        let started = clock.now();
        while self.engine.oldest_snapshot() < target {
            if clock.now().saturating_sub(started) >= wait {
                return Ok(Some(Scrub::default()));
            }
            clock.sleep(Duration::from_millis(2));
        }
        let mut versions = 0;
        for (start, end) in ranges {
            versions += self.engine.purge_range(start, end)?;
        }
        let done = self
            .engine
            .store()
            .scrub()
            .map_err(|e| Error::Storage(Box::new(e)))?;
        Ok(Some(Scrub {
            complete: true,
            versions,
            pages: done.pages,
            wal_bytes: done.wal_bytes,
        }))
    }

    fn find_bytes(&self, needles: &[Vec<u8>]) -> Result<Option<u64>> {
        // The data file's key as it is now — its generation, and the
        // previous key while pages are sealed again (PLAN.md §M14.30 K3f).
        let key = self.engine.store().pool().pager().cipher();
        let found = nucleus_storage::scan::find_with(
            self.platform.fs(),
            &self.dir,
            needles,
            key.as_deref().or(self.page_key.as_ref()),
        )
        .map_err(|e| Error::Storage(Box::new(e)))?;
        Ok(Some(found.len() as u64))
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
        let file = Box::new(FileScratch {
            file,
            path,
            platform: Arc::clone(&self.platform),
        });
        if self.page_key.is_none() {
            return Ok(file);
        }
        let mut key = [0u8; 32];
        self.platform.rng().fill_unpredictable(&mut key);
        let sealed = sealed_scratch::SealedScratch::new(file, key);
        key.fill(0);
        std::hint::black_box(&key);
        Ok(Box::new(sealed))
    }

    /// The commits already past their conflict check made visible first
    /// ([`nucleus_txn::Engine::settle`]: the retry sees the one it lost
    /// to); then from 2 µs, doubling up to about 1 ms, plus up to as much
    /// again at random.
    fn back_off(&self, attempt: u32) {
        // (A failure here is the next attempt's to report.)
        let _ = self.engine.settle();
        let base = 1u64 << attempt.clamp(1, 10);
        let jitter = self.platform.rng().below(base);
        self.platform
            .clock()
            .sleep(Duration::from_micros(base + jitter));
    }
}

struct LocalTxn(nucleus_txn::Transaction, u64);

/// The fewest leaves a part of a split range has: about 64 × 4 KiB, a few
/// thousand small rows — less is not worth a thread.
const SPLIT_MIN_LEAVES: usize = 64;

impl KvRead for LocalTxn {
    fn read_ts(&self) -> Timestamp {
        self.0.read_ts()
    }

    fn first(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        Ok(self.0.first_shared(start, end)?)
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

    fn instance(&self) -> u64 {
        self.1
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

    fn engine_of(&self, start: &[u8], end: &[u8]) -> Result<Option<String>> {
        Ok(self.0.engine_of(start, end)?.map(|k| {
            match k {
                nucleus_txn::EngineKind::Lsm => "lsm",
                nucleus_txn::EngineKind::Map => "map",
            }
            .to_string()
        }))
    }

    fn add_engine(&mut self, start: &[u8], end: &[u8], kind: &str) -> Result<bool> {
        let kind = match kind {
            "lsm" => nucleus_txn::EngineKind::Lsm,
            other => {
                return Err(Error::Storage(format!("no engine \"{other}\"").into()));
            }
        };
        Ok(self.0.add_engine(start, end, kind)?)
    }

    fn watched_as_at(&self, at: Timestamp) -> bool {
        self.0.watched_as_at(at)
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
