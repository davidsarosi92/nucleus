use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Bound;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use nucleus_platform::Platform;
use nucleus_storage::node::MAX_KEY;
use nucleus_storage::{Batch, Store, StoreOptions};

use crate::error::{Error, Result};
use crate::mvcc::{
    self, after_versions, decode_key, decode_value, encode_key, encode_value, first_version,
};

/// A commit timestamp.  Logical for now (PLAN.md principle 3): a counter that
/// increases by one per committed read-write transaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub u64);

/// The isolation level of a transaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Isolation {
    /// Snapshot isolation.
    #[default]
    Snapshot,
    /// Serializable: snapshot isolation plus read-set validation (§M14).
    Serializable,
}

/// How to start a transaction.
#[derive(Clone, Copy, Debug, Default)]
pub struct TxnOptions {
    /// The isolation level.
    pub isolation: Isolation,
    /// Refuse writes.
    pub read_only: bool,
    /// Read an older snapshot (read-only).  Must not be older than the
    /// versions kept ([`EngineOptions::history`]).
    pub at: Option<Timestamp>,
}

/// Tuning for an [`Engine`].
#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// The store underneath.
    pub store: StoreOptions,
    /// Checkpoint automatically once the WAL holds this many bytes since the
    /// last checkpoint.
    pub checkpoint_wal_bytes: u64,
    /// Keep old versions for this many commits beyond what running
    /// transactions need, so snapshots that far back stay readable.
    pub history: u64,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            store: StoreOptions::default(),
            checkpoint_wal_bytes: 64 << 20,
            history: 0,
        }
    }
}

struct Inner {
    store: Store,
    opts: EngineOptions,
    /// The commit lock: the last commit timestamp handed out.
    commit: Mutex<u64>,
    /// Every commit up to here is durable and applied: new snapshots start here.
    visible: AtomicU64,
    /// Start timestamps of running transactions (with counts), guarding
    /// their versions from garbage collection.
    active: Mutex<BTreeMap<u64, usize>>,
    /// Keys written by recent commits, with the newest commit timestamp:
    /// what serializable transactions validate their reads against.
    /// Entries no running transaction can conflict with are pruned.
    /// Changed only under the commit lock.
    recent: Mutex<BTreeMap<Vec<u8>, u64>>,
}

/// What a serializable transaction read: keys, and key ranges.
#[derive(Default)]
struct ReadSet {
    keys: BTreeSet<Vec<u8>>,
    ranges: Vec<(Bound<Vec<u8>>, Bound<Vec<u8>>)>,
}

/// Prune `recent` once it holds this many keys.
const RECENT_PRUNE: usize = 4096;

/// Whether `(start, end)` is a non-empty, well-formed range (BTreeMap
/// panics on the others).
fn valid_range(start: &Bound<Vec<u8>>, end: &Bound<Vec<u8>>) -> bool {
    match (start, end) {
        (Bound::Included(a), Bound::Included(b)) => a <= b,
        (Bound::Included(a) | Bound::Excluded(a), Bound::Excluded(b))
        | (Bound::Excluded(a), Bound::Included(b)) => a < b,
        _ => true,
    }
}

/// A database of versioned keys.  Cloning gives another handle to the same
/// engine.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("visible", &self.visible())
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Engine {
    /// Opens (or creates) the database in `dir` and recovers it.
    pub fn open(platform: Arc<dyn Platform>, dir: &Path, opts: EngineOptions) -> Result<Self> {
        let (store, recovery) = Store::open(platform, dir, &opts.store)?;
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                opts,
                commit: Mutex::new(recovery.meta),
                visible: AtomicU64::new(recovery.meta),
                active: Mutex::new(BTreeMap::new()),
                recent: Mutex::new(BTreeMap::new()),
            }),
        })
    }

    /// The newest snapshot a transaction would see.
    pub fn visible(&self) -> Timestamp {
        Timestamp(self.inner.visible.load(Ordering::Acquire))
    }

    /// Starts a snapshot-isolation read-write transaction.
    pub fn begin(&self) -> Transaction {
        self.begin_with(TxnOptions::default())
            .expect("a current snapshot is always available")
    }

    /// Starts a transaction.
    pub fn begin_with(&self, opts: TxnOptions) -> Result<Transaction> {
        let start = {
            // Registered under the same lock the GC watermark is computed
            // under, so no version this snapshot needs can be collected.
            let mut active = lock(&self.inner.active);
            let visible = self.inner.visible.load(Ordering::Acquire);
            let start = match opts.at {
                None => visible,
                Some(Timestamp(at)) => {
                    if at > visible {
                        return Err(Error::SnapshotTooOld); // a future snapshot is not a snapshot
                    }
                    if at < self.inner.watermark(&active, visible) {
                        return Err(Error::SnapshotTooOld);
                    }
                    at
                }
            };
            *active.entry(start).or_insert(0) += 1;
            start
        };
        Ok(Transaction {
            engine: Arc::clone(&self.inner),
            start,
            writes: BTreeMap::new(),
            read_only: opts.read_only || opts.at.is_some(),
            isolation: opts.isolation,
            done: false,
            reads: Mutex::new(ReadSet::default()),
        })
    }

    /// Makes the database file current (see `Store::checkpoint`).
    pub fn checkpoint(&self) -> Result<()> {
        let _commit = lock(&self.inner.commit);
        Ok(self.inner.store.checkpoint()?)
    }

    /// Removes every version no snapshot can see any more.  Returns how many
    /// were removed.  Commits also collect the old versions of the keys they
    /// write; this sweeps the rest.
    pub fn vacuum(&self) -> Result<u64> {
        let mut removed = 0;
        let mut from = Vec::new();
        loop {
            // In chunks, releasing the commit lock between them.
            let commit = lock(&self.inner.commit);
            let wm = self.inner.current_watermark();
            let mut ops = Vec::new();
            let mut last_user: Option<Vec<u8>> = None;
            let mut kept_one = false;
            let mut next = None;
            {
                let tree = self.inner.store.read()?;
                for entry in tree.range(Bound::Included(&from), Bound::Unbounded)? {
                    let (k, v) = entry?;
                    let (user, ts) =
                        decode_key(&k).ok_or(Error::Corrupt("undecodable version key"))?;
                    if last_user.as_deref() != Some(&user[..]) {
                        if ops.len() >= 512 {
                            next = Some(first_version(&user));
                            break;
                        }
                        last_user = Some(user);
                        kept_one = false;
                    }
                    if ts > wm {
                        continue; // still needed by someone
                    }
                    let tombstone = decode_value(&v)
                        .ok_or(Error::Corrupt("bad version value"))?
                        .is_none();
                    if kept_one || tombstone {
                        ops.push((k, None));
                    }
                    kept_one = true;
                }
            }
            removed += ops.len() as u64;
            if !ops.is_empty() {
                let lsn = self.inner.store.write(&Batch { meta: *commit, ops })?;
                drop(commit);
                self.inner.store.wait_durable(lsn)?;
            }
            match next {
                Some(n) => from = n,
                None => return Ok(removed),
            }
        }
    }

    /// Store entries (all versions) — for tests and statistics.
    pub fn version_count(&self) -> Result<u64> {
        let tree = self.inner.store.read()?;
        let mut n = 0;
        for e in tree.range(Bound::Unbounded, Bound::Unbounded)? {
            e?;
            n += 1;
        }
        Ok(n)
    }

    /// The store (for checks and statistics).
    pub fn store(&self) -> &Store {
        &self.inner.store
    }
}

impl Inner {
    /// The oldest timestamp any snapshot may read: versions older than the
    /// newest one at or below it are garbage.
    fn watermark(&self, active: &BTreeMap<u64, usize>, visible: u64) -> u64 {
        let oldest = active
            .keys()
            .next()
            .copied()
            .unwrap_or(visible)
            .min(visible);
        oldest.saturating_sub(self.opts.history)
    }

    fn current_watermark(&self) -> u64 {
        let active = lock(&self.active);
        self.watermark(&active, self.visible.load(Ordering::Acquire))
    }

    /// The newest version of `user` visible at `ts`: `Some(Some(v))` a value,
    /// `Some(None)` a tombstone, `None` no version at all.
    fn read_at(&self, user: &[u8], ts: u64) -> Result<Option<Option<Vec<u8>>>> {
        let tree = self.store.read()?;
        let from = encode_key(user, ts);
        let to = after_versions(user);
        let mut range = tree.range(Bound::Included(&from), Bound::Excluded(&to))?;
        match range.next() {
            None => Ok(None),
            Some(entry) => {
                let (_, v) = entry?;
                let value = decode_value(&v).ok_or(Error::Corrupt("bad version value"))?;
                Ok(Some(value.map(<[u8]>::to_vec)))
            }
        }
    }

    /// The timestamp of the newest version of `user`, durable or not.
    fn newest_ts(&self, user: &[u8]) -> Result<Option<u64>> {
        let tree = self.store.read()?;
        let from = first_version(user);
        let to = after_versions(user);
        match tree
            .range(Bound::Included(&from), Bound::Excluded(&to))?
            .next()
        {
            None => Ok(None),
            Some(entry) => {
                let (k, _) = entry?;
                Ok(decode_key(&k).map(|(_, ts)| ts))
            }
        }
    }

    /// Deletes for the versions of `user` that no snapshot at or after `wm`
    /// can see: all but the newest at or below `wm`, and that one too if it
    /// is a tombstone.
    fn garbage(
        &self,
        user: &[u8],
        wm: u64,
        ops: &mut Vec<(Vec<u8>, Option<Vec<u8>>)>,
    ) -> Result<()> {
        let tree = self.store.read()?;
        let from = encode_key(user, wm);
        let to = after_versions(user);
        let mut first = true;
        for entry in tree.range(Bound::Included(&from), Bound::Excluded(&to))? {
            let (k, v) = entry?;
            let tombstone = decode_value(&v)
                .ok_or(Error::Corrupt("bad version value"))?
                .is_none();
            if !first || tombstone {
                ops.push((k, None));
            }
            first = false;
        }
        Ok(())
    }
}

/// A transaction.  Dropping it without committing rolls it back.
pub struct Transaction {
    engine: Arc<Inner>,
    start: u64,
    writes: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    read_only: bool,
    isolation: Isolation,
    done: bool,
    /// What was read, for validation at commit: every read of a
    /// serializable transaction, and the validated reads
    /// ([`Transaction::get_validated`], [`Transaction::scan_validated`]) of
    /// any transaction.
    reads: Mutex<ReadSet>,
}

impl std::fmt::Debug for Transaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("start", &self.start)
            .field("writes", &self.writes.len())
            .finish_non_exhaustive()
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        let mut active = lock(&self.engine.active);
        if let Some(n) = active.get_mut(&self.start) {
            *n -= 1;
            if *n == 0 {
                active.remove(&self.start);
            }
        }
    }
}

fn check_key(key: &[u8]) -> Result<()> {
    let encoded = key.len() + key.iter().filter(|&&b| b == 0).count() + mvcc::OVERHEAD;
    if encoded > MAX_KEY {
        Err(Error::KeyTooLarge(key.len()))
    } else {
        Ok(())
    }
}

impl Transaction {
    /// The snapshot this transaction reads.
    pub fn read_ts(&self) -> Timestamp {
        Timestamp(self.start)
    }

    /// The isolation level.
    pub fn isolation(&self) -> Isolation {
        self.isolation
    }

    /// The value of `key` in this transaction (its own writes included).
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(w) = self.writes.get(key) {
            return Ok(w.clone());
        }
        if self.isolation == Isolation::Serializable {
            lock(&self.reads).keys.insert(key.to_vec());
        }
        Ok(self.engine.read_at(key, self.start)?.flatten())
    }

    /// [`get`](Self::get), and the commit fails if a commit after this
    /// transaction's snapshot wrote `key` — at any isolation level.  Under
    /// snapshot isolation this makes a decision on the value safe without
    /// serializing everything (a foreign key check: the referenced row must
    /// still be there at commit).
    pub fn get_validated(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        lock(&self.reads).keys.insert(key.to_vec());
        self.get(key)
    }

    /// [`scan`](Self::scan), validated as [`get_validated`](Self::get_validated):
    /// the commit fails if a later commit wrote inside the part of the range
    /// that was consumed.
    pub fn scan_validated(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Scan<'_>> {
        let mut scan = self.scan(start, end)?;
        scan.validated = true;
        Ok(scan)
    }

    /// Sets `key` to `value`.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.write(key, Some(value))
    }

    /// Deletes `key`.
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.write(key, None)
    }

    fn write(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        check_key(key)?;
        // Fail early when a newer version exists: the commit would fail
        // anyway (first committer wins), and the client learns at the
        // statement, as PostgreSQL reports it.  The commit check stays the
        // guarantee — a conflicting commit can still land after this.
        if !self.writes.contains_key(key)
            && self
                .engine
                .newest_ts(key)?
                .is_some_and(|ts| ts > self.start)
        {
            return Err(Error::Conflict { key: key.to_vec() });
        }
        self.writes.insert(key.to_vec(), value.map(<[u8]>::to_vec));
        Ok(())
    }

    /// The keys and values in `(start, end)`, ascending, as this transaction
    /// sees them.
    pub fn scan(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Scan<'_>> {
        let seek = match start {
            Bound::Unbounded => Vec::new(),
            Bound::Included(k) => first_version(k),
            Bound::Excluded(k) => after_versions(k),
        };
        let store_end = match end {
            Bound::Unbounded => None,
            Bound::Included(k) => Some(after_versions(k)),
            Bound::Excluded(k) => Some(first_version(k)),
        };
        let writes = self.writes.range::<[u8], _>((start, end)).peekable();
        Ok(Scan {
            txn: self,
            seek: Some(seek),
            store_end,
            buffer: VecDeque::new(),
            writes,
            range: (start.map(<[u8]>::to_vec), end.map(<[u8]>::to_vec)),
            last: None,
            done: false,
            validated: self.isolation == Isolation::Serializable,
        })
    }

    /// Commits; returns the commit timestamp (the snapshot for a transaction
    /// that wrote nothing).  On [`Error::Conflict`] nothing was written.
    pub fn commit(mut self) -> Result<Timestamp> {
        self.done = true;
        if self.writes.is_empty() {
            return Ok(Timestamp(self.start));
        }
        let engine = Arc::clone(&self.engine);
        let (ts, lsn) = {
            let mut last = lock(&engine.commit);
            for key in self.writes.keys() {
                if engine.newest_ts(key)?.is_some_and(|ts| ts > self.start) {
                    return Err(Error::Conflict { key: key.clone() });
                }
            }
            // Serializable transactions record every read; others only
            // their validated ones.
            self.validate_reads(&engine)?;
            let ts = *last + 1;
            let wm = engine.current_watermark();
            let mut ops = Vec::with_capacity(self.writes.len());
            for (key, value) in &self.writes {
                engine.garbage(key, wm, &mut ops)?;
                ops.push((encode_key(key, ts), Some(encode_value(value.as_deref()))));
            }
            let lsn = engine.store.write(&Batch { meta: ts, ops })?;
            *last = ts;
            let mut recent = lock(&engine.recent);
            for key in self.writes.keys() {
                recent.insert(key.clone(), ts);
            }
            if recent.len() > RECENT_PRUNE {
                // Nobody started before the oldest running transaction can
                // still commit, so older writes cannot conflict any more.
                let oldest = lock(&engine.active).keys().next().copied().unwrap_or(ts);
                recent.retain(|_, t| *t > oldest);
            }
            drop(recent);
            (ts, lsn)
        };
        engine.store.wait_durable(lsn)?;
        engine.visible.fetch_max(ts, Ordering::AcqRel);
        if engine.store.wal_since_checkpoint() > engine.opts.checkpoint_wal_bytes {
            let _commit = lock(&engine.commit);
            if engine.store.wal_since_checkpoint() > engine.opts.checkpoint_wal_bytes {
                engine.store.checkpoint()?;
            }
        }
        Ok(Timestamp(ts))
    }

    /// Read validation: no key or range in the read set was written by a
    /// commit after this transaction's snapshot.  Called under the commit
    /// lock.
    fn validate_reads(&self, engine: &Inner) -> Result<()> {
        let reads = lock(&self.reads);
        let recent = lock(&engine.recent);
        for k in &reads.keys {
            if recent.get(k).is_some_and(|&ts| ts > self.start) {
                return Err(Error::Conflict { key: k.clone() });
            }
        }
        for (s, e) in &reads.ranges {
            if !valid_range(s, e) {
                continue;
            }
            let bounds = (s.as_ref().map(Vec::as_slice), e.as_ref().map(Vec::as_slice));
            if let Some((k, _)) = recent
                .range::<[u8], _>(bounds)
                .find(|(_, ts)| **ts > self.start)
            {
                return Err(Error::Conflict { key: k.clone() });
            }
        }
        Ok(())
    }

    /// Discards the transaction's writes.
    pub fn rollback(mut self) {
        self.done = true;
    }
}

/// A scan over a transaction's view (see [`Transaction::scan`]).  Reads the
/// store in batches, so it never holds the store's lock between calls.
pub struct Scan<'t> {
    txn: &'t Transaction,
    /// Where the next store batch starts (`None` once the store is done).
    seek: Option<Vec<u8>>,
    store_end: Option<Vec<u8>>,
    /// Visible store entries read ahead.
    buffer: VecDeque<Visible>,
    writes: std::iter::Peekable<std::collections::btree_map::Range<'t, Vec<u8>, Option<Vec<u8>>>>,
    /// The requested range, and how far it was read: a serializable
    /// transaction depends on the part it consumed.
    range: (Bound<Vec<u8>>, Bound<Vec<u8>>),
    last: Option<Vec<u8>>,
    done: bool,
    /// Whether the consumed range joins the read set.
    validated: bool,
}

impl Drop for Scan<'_> {
    fn drop(&mut self) {
        if !self.validated {
            return;
        }
        let read = if self.done {
            Some(self.range.clone())
        } else {
            self.last
                .clone()
                .map(|k| (self.range.0.clone(), Bound::Included(k)))
        };
        if let Some(r) = read {
            lock(&self.txn.reads).ranges.push(r);
        }
    }
}

impl std::fmt::Debug for Scan<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scan").finish_non_exhaustive()
    }
}

const SCAN_BATCH: usize = 128;

/// A user key and its visible value (`None`: deleted).
type Visible = (Vec<u8>, Option<Vec<u8>>);

impl Scan<'_> {
    /// Reads the next batch of visible versions from the store.
    fn refill(&mut self) -> Result<()> {
        let Some(seek) = self.seek.take() else {
            return Ok(());
        };
        let engine = &self.txn.engine;
        let tree = engine.store.read()?;
        let end = match &self.store_end {
            Some(e) => Bound::Excluded(e.as_slice()),
            None => Bound::Unbounded,
        };
        let mut last: Option<Vec<u8>> = None;
        let mut emitted = 0;
        for entry in tree.range(Bound::Included(&seek), end)? {
            let (k, v) = entry?;
            let (user, ts) = decode_key(&k).ok_or(Error::Corrupt("undecodable version key"))?;
            if ts > self.txn.start || last.as_deref() == Some(&user[..]) {
                continue; // too new, or an older version of a key already seen
            }
            if emitted == SCAN_BATCH {
                self.seek = Some(first_version(&user));
                return Ok(());
            }
            let value = decode_value(&v).ok_or(Error::Corrupt("bad version value"))?;
            self.buffer
                .push_back((user.clone(), value.map(<[u8]>::to_vec)));
            last = Some(user);
            emitted += 1;
        }
        Ok(())
    }

    fn next_store(&mut self) -> Result<Option<&Visible>> {
        if self.buffer.is_empty() {
            self.refill()?;
        }
        Ok(self.buffer.front())
    }

    fn step(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        loop {
            let store_key = self.next_store()?.map(|(k, _)| k.clone());
            let write_key = self.writes.peek().map(|(k, _)| (*k).clone());
            let (key, value) = match (store_key, write_key) {
                (None, None) => return Ok(None),
                (Some(s), Some(w)) if w <= s => {
                    if w == s {
                        self.buffer.pop_front(); // the transaction's own write wins
                    }
                    let (k, v) = self.writes.next().expect("peeked");
                    (k.clone(), v.clone())
                }
                (None, Some(_)) => {
                    let (k, v) = self.writes.next().expect("peeked");
                    (k.clone(), v.clone())
                }
                (Some(_), _) => self.buffer.pop_front().expect("peeked"),
            };
            if let Some(v) = value {
                return Ok(Some((key, v)));
            }
            // A tombstone: deleted in this snapshot.
        }
    }
}

impl Iterator for Scan<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.step() {
            Ok(Some((k, v))) => {
                self.last = Some(k.clone());
                Some(Ok((k, v)))
            }
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => Some(Err(e)),
        }
    }
}
