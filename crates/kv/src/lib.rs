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
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use nucleus_platform::Platform;
use nucleus_platform::real::RealPlatform;
use nucleus_storage::StoreOptions;

pub use nucleus_txn::{Isolation, Timestamp};

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

/// A database that runs transactions.
pub trait Kv: Send + Sync {
    /// Starts a transaction.
    fn begin(&self, opts: TxnOptions) -> Result<Box<dyn KvTxn>>;

    /// Wall-clock time as the implementation keeps it — the local one's
    /// platform clock; a distributed one answers from its hybrid logical
    /// clock.  Layers above read the time only here (principle 4), so a
    /// simulation's `now()` is the simulated clock.
    fn wall_clock(&self) -> SystemTime;

    /// Waits before retrying a transaction for the `attempt`-th time (from
    /// 1): exponentially longer each time, with jitter, so retries that
    /// collided once spread out.  Through the implementation's platform, so
    /// a simulation does not wait in real time.
    fn back_off(&self, attempt: u32);
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

    /// Sets `key` to `value`.
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()>;

    /// Deletes `key` (no error if absent).
    fn delete(&mut self, key: &[u8]) -> Result<()>;

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
    /// Page cache size in bytes (rounded down to 8 KiB pages; at least 16).
    pub cache_bytes: usize,
    /// Checkpoint once the WAL holds this many bytes.
    pub checkpoint_wal_bytes: u64,
    /// WAL segment size in bytes.
    pub wal_segment_bytes: u64,
    /// Old versions kept, in commits, beyond what running transactions need.
    pub history: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            cache_bytes: 8 << 20,
            checkpoint_wal_bytes: 64 << 20,
            wal_segment_bytes: 16 << 20,
            history: 0,
        }
    }
}

/// The local, embedded implementation of [`Kv`]: one directory on disk.
/// Cloning gives another handle to the same database.
#[derive(Clone)]
pub struct Database {
    engine: nucleus_txn::Engine,
    platform: Arc<dyn Platform>,
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
            },
        )?;
        Ok(Self { engine, platform })
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

    fn get_validated(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get_validated(key)?)
    }

    fn scan_validated<'a>(&'a self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<KvIter<'a>> {
        let scan = self.0.scan_validated(start, end)?;
        Ok(Box::new(scan.map(|r| r.map_err(Error::from))))
    }

    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        Ok(self.0.put(key, value)?)
    }

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        Ok(self.0.delete(key)?)
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
