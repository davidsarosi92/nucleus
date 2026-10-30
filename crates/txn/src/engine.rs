use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use nucleus_platform::Platform;
use nucleus_storage::node::MAX_KEY;
use nucleus_storage::{BTree, Batch, Store, StoreOptions};

use crate::error::{Error, Result};
use crate::mvcc::{
    self, RESERVED_END, SIDE, STATUS, after_versions, decode_key, decode_key_onto, decode_value,
    encode_key, encode_value, first_version, side_key, side_prefix, status_key,
};

/// A commit timestamp.  Logical for now (PLAN.md principle 3): a counter that
/// increases by one per committed read-write transaction.  Opaque above
/// this crate — timestamps are compared, printed and parsed, never
/// computed with — so a hybrid logical clock can replace the counter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub(crate) u64);

impl std::fmt::Display for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for Timestamp {
    type Err = std::num::ParseIntError;

    /// The text form [`Display`](std::fmt::Display) writes.
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        s.parse().map(Timestamp)
    }
}

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
    /// A transaction keeps its writes in memory up to about this many
    /// bytes; past it they spill into the store (the side area, §5.3), so
    /// a transaction of any size runs in bounded memory.
    pub txn_memory: usize,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            store: StoreOptions::default(),
            checkpoint_wal_bytes: 64 << 20,
            history: 0,
            txn_memory: 64 << 20,
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
    /// Key spans of recent commits that spilled — (lowest, highest, commit
    /// timestamp) — which reads validate against beside `recent`.
    recent_spans: Mutex<Vec<(Vec<u8>, Vec<u8>, u64)>>,
    /// The next id of a transaction whose writes spill.
    next_id: AtomicU64,
    /// Transactions whose writes spilled into the side area and that have
    /// not committed: a commit checks its keys against theirs.  Changed
    /// under the commit lock.
    spilling: Mutex<BTreeSet<u64>>,
    /// Committed transactions whose spilled writes are still in the side
    /// area, being moved into versions: id → commit timestamp.  Readers
    /// see those writes there.
    unresolved: Mutex<BTreeMap<u64, u64>>,
    /// How many transactions are in `spilling` or `unresolved` (reads skip
    /// the side area when none).
    side_users: AtomicUsize,
}

/// Side-area entries moved or deleted per batch.
const SIDE_CHUNK: usize = 4096;
/// Bytes spilled per batch (each applied under the tree's write lock).
const SPILL_CHUNK: usize = 1 << 20;
/// What a write costs in memory beside its key and value (about).
const WRITE_OVERHEAD: usize = 64;

/// The store's entries in `[from, to)` that are versions: the reserved
/// keys (`00 01` – `00 FF`) skipped.
fn versions<'t>(
    tree: &'t BTree,
    from: Bound<&[u8]>,
    to: Bound<&[u8]>,
) -> Result<Vec<nucleus_storage::btree::Range<'t>>> {
    let before_reserved = match from {
        Bound::Unbounded => true,
        Bound::Included(f) | Bound::Excluded(f) => f < &RESERVED_END[..],
    };
    let ends_before = |k: &[u8]| match to {
        Bound::Unbounded => false,
        Bound::Included(t) => t < k,
        Bound::Excluded(t) => t <= k,
    };
    if !before_reserved {
        return Ok(vec![tree.range(from, to)?]);
    }
    let low_end = if ends_before(&SIDE) {
        to
    } else {
        Bound::Excluded(&SIDE[..])
    };
    let low = tree.range(from, low_end)?;
    if ends_before(&RESERVED_END) {
        return Ok(vec![low]);
    }
    Ok(vec![
        low,
        tree.range(Bound::Included(&RESERVED_END[..]), to)?,
    ])
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
        let engine = Self {
            inner: Arc::new(Inner {
                store,
                opts,
                commit: Mutex::new(recovery.meta),
                visible: AtomicU64::new(recovery.meta),
                active: Mutex::new(BTreeMap::new()),
                recent: Mutex::new(BTreeMap::new()),
                recent_spans: Mutex::new(Vec::new()),
                next_id: AtomicU64::new(1),
                spilling: Mutex::new(BTreeSet::new()),
                unresolved: Mutex::new(BTreeMap::new()),
                side_users: AtomicUsize::new(0),
            }),
        };
        engine.inner.recover_side()?;
        Ok(engine)
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
            write_bytes: 0,
            spilled: None,
            span: None,
            read_only: opts.read_only || opts.at.is_some(),
            isolation: opts.isolation,
            done: false,
            reads: Mutex::new(ReadSet::default()),
            fresh: Vec::new(),
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
                let unresolved = self.inner.unresolved_ids();
                let mut pinned = false;
                for entry in versions(&tree, Bound::Included(&from), Bound::Unbounded)?
                    .into_iter()
                    .flatten()
                {
                    let (k, v) = entry?;
                    let (user, ts) =
                        decode_key(&k).ok_or(Error::Corrupt("undecodable version key"))?;
                    if last_user.as_deref() != Some(&user[..]) {
                        if ops.len() >= 512 {
                            next = Some(first_version(&user));
                            break;
                        }
                        // A key with a write still in the side area keeps
                        // its versions until that write is moved.
                        pinned = Inner::in_side(&tree, &unresolved, &user)?;
                        last_user = Some(user);
                        kept_one = false;
                    }
                    if ts > wm || pinned {
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
        for e in versions(&tree, Bound::Unbounded, Bound::Unbounded)?
            .into_iter()
            .flatten()
        {
            e?;
            n += 1;
        }
        Ok(n)
    }

    /// Entries in the side area: writes of large transactions not yet
    /// moved into versions — for tests.
    pub fn side_count(&self) -> Result<u64> {
        let tree = self.inner.store.read()?;
        let mut n = 0;
        for e in tree.range(
            Bound::Included(&SIDE[..]),
            Bound::Excluded(&RESERVED_END[..]),
        )? {
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

    /// The committed transactions whose writes are still in the side area,
    /// with their commit timestamps (none, without taking the lock, when
    /// no transaction uses the side area).
    fn unresolved_ids(&self) -> Vec<(u64, u64)> {
        if self.side_users.load(Ordering::Acquire) == 0 {
            return Vec::new();
        }
        lock(&self.unresolved)
            .iter()
            .map(|(&id, &ts)| (id, ts))
            .collect()
    }

    /// Whether any of `unresolved` holds a write of `user`.
    fn in_side(tree: &BTree, unresolved: &[(u64, u64)], user: &[u8]) -> Result<bool> {
        for &(id, _) in unresolved {
            if tree.get(&side_key(id, user))?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The newest version of `user` visible at `ts`: `Some(Some(v))` a value,
    /// `Some(None)` a tombstone, `None` no version at all — among the
    /// versions and the side area's writes of committed transactions.
    fn read_at(&self, user: &[u8], ts: u64) -> Result<Option<Option<Vec<u8>>>> {
        let unresolved = self.unresolved_ids();
        // One read of the tree: a side write moved into a version is in
        // one place or the other, never neither.
        let tree = self.store.read()?;
        let from = encode_key(user, ts);
        let to = after_versions(user);
        let mut best: Option<(u64, Vec<u8>)> = None;
        if let Some(entry) = tree
            .range(Bound::Included(&from), Bound::Excluded(&to))?
            .next()
        {
            let (k, v) = entry?;
            let (_, vts) = decode_key(&k).ok_or(Error::Corrupt("undecodable version key"))?;
            best = Some((vts, v));
        }
        for (id, cts) in unresolved {
            if cts <= ts
                && best.as_ref().is_none_or(|(b, _)| cts > *b)
                && let Some(v) = tree.get(&side_key(id, user))?
            {
                best = Some((cts, v));
            }
        }
        match best {
            None => Ok(None),
            Some((_, v)) => {
                let value = decode_value(&v).ok_or(Error::Corrupt("bad version value"))?;
                Ok(Some(value.map(<[u8]>::to_vec)))
            }
        }
    }

    /// One look at `user`'s versions: the newest one's timestamp (durable or
    /// not), and the value a reader at `ts` sees ([`Inner::read_at`]'s).
    #[allow(clippy::type_complexity)]
    fn newest_and_at(
        &self,
        user: &[u8],
        ts: u64,
    ) -> Result<(Option<u64>, Option<Option<Vec<u8>>>)> {
        let unresolved = self.unresolved_ids();
        let tree = self.store.read()?;
        let from = first_version(user);
        let to = after_versions(user);
        let mut newest = None;
        let mut best: Option<(u64, Vec<u8>)> = None;
        for entry in tree.range(Bound::Included(&from), Bound::Excluded(&to))? {
            let (k, v) = entry?;
            let (_, vts) = decode_key(&k).ok_or(Error::Corrupt("undecodable version key"))?;
            newest.get_or_insert(vts);
            if vts <= ts {
                best = Some((vts, v));
                break;
            }
        }
        for (id, cts) in unresolved {
            let newer = newest.is_none_or(|n| cts > n);
            let seen = cts <= ts && best.as_ref().is_none_or(|(b, _)| cts > *b);
            if (newer || seen)
                && let Some(v) = tree.get(&side_key(id, user))?
            {
                if newer {
                    newest = Some(cts);
                }
                if seen {
                    best = Some((cts, v));
                }
            }
        }
        let value = match best {
            None => None,
            Some((_, v)) => Some(
                decode_value(&v)
                    .ok_or(Error::Corrupt("bad version value"))?
                    .map(<[u8]>::to_vec),
            ),
        };
        Ok((newest, value))
    }

    /// The timestamp of the newest version of `user`, durable or not (a
    /// committed write still in the side area included).
    fn newest_ts(&self, user: &[u8]) -> Result<Option<u64>> {
        let unresolved = self.unresolved_ids();
        let tree = self.store.read()?;
        let from = first_version(user);
        let to = after_versions(user);
        let mut newest = match tree
            .range(Bound::Included(&from), Bound::Excluded(&to))?
            .next()
        {
            None => None,
            Some(entry) => {
                let (k, _) = entry?;
                decode_key(&k).map(|(_, ts)| ts)
            }
        };
        for (id, cts) in unresolved {
            if newest.is_none_or(|n| cts > n) && tree.get(&side_key(id, user))?.is_some() {
                newest = Some(cts);
            }
        }
        Ok(newest)
    }

    /// Whether writing `user` conflicts for a transaction that read at
    /// `start` (spilled as `me`): a newer commit wrote it, or another
    /// transaction has spilled a write of it.
    fn conflicts(&self, user: &[u8], start: u64, me: Option<u64>) -> Result<bool> {
        if self.newest_ts(user)?.is_some_and(|ts| ts > start) {
            return Ok(true);
        }
        if self.side_users.load(Ordering::Acquire) == 0 {
            return Ok(false);
        }
        let others: Vec<u64> = lock(&self.spilling)
            .iter()
            .copied()
            .filter(|&id| Some(id) != me)
            .collect();
        if others.is_empty() {
            return Ok(false);
        }
        let tree = self.store.read()?;
        for id in others {
            if tree.get(&side_key(id, user))?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The side area as the engine found it on opening: the writes of
    /// transactions that never committed are dropped, those of committed
    /// ones moved into versions.
    fn recover_side(&self) -> Result<()> {
        let mut unresolved = Vec::new();
        let mut max_id = 0;
        {
            let tree = self.store.read()?;
            for e in tree.range(Bound::Included(&STATUS[..]), Bound::Excluded(&[0, 3][..]))? {
                let (k, v) = e?;
                let id = u64::from_be_bytes(
                    k[2..]
                        .try_into()
                        .map_err(|_| Error::Corrupt("bad status key"))?,
                );
                let ts = u64::from_be_bytes(
                    v[..]
                        .try_into()
                        .map_err(|_| Error::Corrupt("bad status record"))?,
                );
                unresolved.push((id, ts));
                max_id = max_id.max(id);
            }
            if let Some(e) = tree
                .range_rev(Bound::Included(&SIDE[..]), Bound::Excluded(&STATUS[..]))?
                .next()
            {
                let (k, _) = e?;
                let id = k
                    .get(2..10)
                    .and_then(|b| b.try_into().ok())
                    .map(u64::from_be_bytes)
                    .ok_or(Error::Corrupt("bad side key"))?;
                max_id = max_id.max(id);
            }
        }
        self.next_id.store(max_id + 1, Ordering::Release);
        // Every id in the side area without a status never committed.
        let mut id = 0u64;
        loop {
            let found = {
                let tree = self.store.read()?;
                let from = side_prefix(id);
                match tree
                    .range(Bound::Included(&from[..]), Bound::Excluded(&STATUS[..]))?
                    .next()
                {
                    None => None,
                    Some(e) => {
                        let (k, _) = e?;
                        k.get(2..10)
                            .and_then(|b| b.try_into().ok())
                            .map(u64::from_be_bytes)
                    }
                }
            };
            let Some(found) = found else { break };
            if !unresolved.iter().any(|&(u, _)| u == found) {
                self.drop_side(found)?;
            }
            if found == u64::MAX {
                break;
            }
            id = found + 1;
        }
        for &(id, ts) in &unresolved {
            lock(&self.unresolved).insert(id, ts);
            self.side_users.fetch_add(1, Ordering::AcqRel);
        }
        for (id, ts) in unresolved {
            self.resolve(id, ts)?;
        }
        Ok(())
    }

    /// Deletes transaction `id`'s side area, a batch at a time.
    fn drop_side(&self, id: u64) -> Result<()> {
        let (from, to) = (side_prefix(id), side_prefix(id.wrapping_add(1)));
        let to: &[u8] = if id == u64::MAX { &STATUS } else { &to };
        loop {
            let last = lock(&self.commit);
            let mut ops = Vec::new();
            {
                let tree = self.store.read()?;
                for e in tree.range(Bound::Included(&from[..]), Bound::Excluded(to))? {
                    let (k, _) = e?;
                    ops.push((k, None));
                    if ops.len() == SIDE_CHUNK {
                        break;
                    }
                }
            }
            if ops.is_empty() {
                return Ok(());
            }
            self.store.write(&Batch { meta: *last, ops })?;
        }
    }

    /// Moves committed transaction `id`'s side area into versions at `ts`,
    /// a batch at a time, then forgets it.  Each batch is atomic: a reader
    /// finds a write in one place or the other.  Interrupted (a crash), it
    /// is finished when the engine opens again.
    fn resolve(&self, id: u64, ts: u64) -> Result<()> {
        let (from, to) = (side_prefix(id), side_prefix(id.wrapping_add(1)));
        let to: &[u8] = if id == u64::MAX { &STATUS } else { &to };
        loop {
            let last = lock(&self.commit);
            let mut ops = Vec::new();
            {
                let tree = self.store.read()?;
                for e in tree.range(Bound::Included(&from[..]), Bound::Excluded(to))? {
                    let (k, v) = e?;
                    ops.push((encode_key(&k[10..], ts), Some(v)));
                    ops.push((k, None));
                    if ops.len() >= 2 * SIDE_CHUNK {
                        break;
                    }
                }
            }
            let done = ops.is_empty();
            if done {
                ops.push((status_key(id), None));
            }
            self.store.write(&Batch { meta: *last, ops })?;
            if done {
                lock(&self.unresolved).remove(&id);
                self.side_users.fetch_sub(1, Ordering::AcqRel);
                return Ok(());
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
        let unresolved = self.unresolved_ids();
        let tree = self.store.read()?;
        // A key with a write still in the side area keeps its versions
        // until that write is moved (its order among them is its commit
        // timestamp's).
        if Inner::in_side(&tree, &unresolved, user)? {
            return Ok(());
        }
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
    /// The writes held in memory (the newest of each key).
    writes: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    /// About how much memory `writes` takes.
    write_bytes: usize,
    /// The transaction's id in the side area, once its writes spilled.
    spilled: Option<u64>,
    /// The lowest and highest key spilled.
    span: Option<(Vec<u8>, Vec<u8>)>,
    read_only: bool,
    isolation: Isolation,
    done: bool,
    /// What was read, for validation at commit: every read of a
    /// serializable transaction, and the validated reads
    /// ([`Transaction::get_validated`], [`Transaction::scan_validated`]) of
    /// any transaction.
    reads: Mutex<ReadSet>,
    /// Key ranges declared fresh ([`Transaction::fresh`]).
    fresh: Vec<(Vec<u8>, Vec<u8>)>,
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
        // Spilled writes of a transaction that did not commit: dropped (on
        // a failure here, by the next open).
        if let Some(id) = self.spilled.take() {
            lock(&self.engine.spilling).remove(&id);
            self.engine.side_users.fetch_sub(1, Ordering::AcqRel);
            let _ = self.engine.drop_side(id);
        }
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
        if let Some(id) = self.spilled
            && let Some(v) = self.engine.store.get(&side_key(id, key))?
        {
            let value = decode_value(&v).ok_or(Error::Corrupt("bad side value"))?;
            return Ok(value.map(<[u8]>::to_vec));
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

    /// Sets `key` to `value` unless this transaction sees a value there
    /// (`false`, nothing written) — the value it would see and the check for
    /// a newer version (the write's conflict) from one look at the key's
    /// versions.
    pub fn put_new(&mut self, key: &[u8], value: &[u8]) -> Result<bool> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        check_key(key)?;
        // Written by this transaction: its own value decides.
        if let Some(w) = self.writes.get(key) {
            if w.is_some() {
                return Ok(false);
            }
            self.record(key, Some(value))?;
            return Ok(true);
        }
        if let Some(id) = self.spilled
            && let Some(v) = self.engine.store.get(&side_key(id, key))?
        {
            if decode_value(&v)
                .ok_or(Error::Corrupt("bad side value"))?
                .is_some()
            {
                return Ok(false);
            }
            self.record(key, Some(value))?;
            return Ok(true);
        }
        // A fresh key has no versions to look at.
        if self.is_fresh(key) {
            self.record(key, Some(value))?;
            return Ok(true);
        }
        if self.isolation == Isolation::Serializable {
            lock(&self.reads).keys.insert(key.to_vec());
        }
        let (newest, seen) = self.engine.newest_and_at(key, self.start)?;
        if seen.flatten().is_some() {
            return Ok(false);
        }
        if newest.is_some_and(|ts| ts > self.start)
            || (self.engine.side_users.load(Ordering::Acquire) > 0
                && self.engine.conflicts(key, self.start, self.spilled)?)
        {
            return Err(Error::Conflict { key: key.to_vec() });
        }
        self.record(key, Some(value))?;
        Ok(true)
    }

    /// Deletes `key`.
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.write(key, None)
    }

    /// Declares the keys in `[start, end)` fresh: none has a version and no
    /// other transaction writes one before this one ends (a new index's
    /// keys).  Their writes skip what guards against other transactions —
    /// the check for a newer version as they are written, spill and
    /// commit, and the old versions' collection — which for a fresh range
    /// would find nothing.  The caller answers for the claim.
    pub fn fresh(&mut self, start: &[u8], end: &[u8]) {
        self.fresh.push((start.to_vec(), end.to_vec()));
    }

    fn is_fresh(&self, key: &[u8]) -> bool {
        self.fresh
            .iter()
            .any(|(s, e)| key >= s.as_slice() && key < e.as_slice())
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
            && !self.is_fresh(key)
            && self.engine.conflicts(key, self.start, self.spilled)?
        {
            return Err(Error::Conflict { key: key.to_vec() });
        }
        self.record(key, value)
    }

    /// Holds a write (checked already), spilling when past the memory
    /// allowed.
    fn record(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        let size = key.len() + value.map_or(0, <[u8]>::len) + WRITE_OVERHEAD;
        match self.writes.insert(key.to_vec(), value.map(<[u8]>::to_vec)) {
            Some(old) => {
                let was = key.len() + old.map_or(0, |v| v.len()) + WRITE_OVERHEAD;
                self.write_bytes = (self.write_bytes + size).saturating_sub(was);
            }
            None => self.write_bytes += size,
        }
        if self.write_bytes > self.engine.opts.txn_memory {
            self.spill()?;
        }
        Ok(())
    }

    /// Moves the writes held in memory into the side area (the
    /// transaction's own part of the store, which no one else reads before
    /// it commits).  Under the commit lock, each key is checked as a commit
    /// would check it; from then on, a commit of the same key conflicts.
    fn spill(&mut self) -> Result<()> {
        let engine = Arc::clone(&self.engine);
        let last = lock(&engine.commit);
        let id = match self.spilled {
            Some(id) => id,
            None => engine.next_id.fetch_add(1, Ordering::AcqRel),
        };
        for key in self.writes.keys() {
            if !self.is_fresh(key) && engine.conflicts(key, self.start, Some(id))? {
                return Err(Error::Conflict { key: key.clone() });
            }
        }
        if self.spilled.is_none() {
            lock(&engine.spilling).insert(id);
            engine.side_users.fetch_add(1, Ordering::AcqRel);
            self.spilled = Some(id);
        }
        if let (Some((lo, _)), Some((hi, _))) =
            (self.writes.first_key_value(), self.writes.last_key_value())
        {
            self.span = Some(match self.span.take() {
                None => (lo.clone(), hi.clone()),
                Some((a, b)) => (a.min(lo.clone()), b.max(hi.clone())),
            });
        }
        let mut ops = Vec::new();
        let mut bytes = 0;
        for (key, value) in std::mem::take(&mut self.writes) {
            bytes += key.len() + value.as_ref().map_or(0, Vec::len) + 16;
            ops.push((side_key(id, &key), Some(encode_value(value.as_deref()))));
            if bytes >= SPILL_CHUNK {
                engine.store.write(&Batch {
                    meta: *last,
                    ops: std::mem::take(&mut ops),
                })?;
                bytes = 0;
            }
        }
        if !ops.is_empty() {
            engine.store.write(&Batch { meta: *last, ops })?;
        }
        self.write_bytes = 0;
        if engine.store.wal_since_checkpoint() > engine.opts.checkpoint_wal_bytes {
            engine.store.checkpoint()?;
        }
        Ok(())
    }

    /// Up to `parts - 1` user keys inside `[start, end)` cutting it into
    /// parts of about equal size in the store, each of at least
    /// `min_leaves` of its leaves; ascending ([`BTree::split_keys`]).
    pub fn split_points(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        parts: usize,
        min_leaves: usize,
    ) -> Result<Vec<Vec<u8>>> {
        let tree = self.engine.store.read()?;
        let lo = first_version(start);
        let hi = end.map(first_version);
        let mut out: Vec<Vec<u8>> = Vec::new();
        for k in tree.split_keys(&lo, hi.as_deref(), parts, min_leaves)? {
            // Separators are version keys (the reserved ones skipped).
            if let Some((user, _)) = decode_key(&k)
                && user.as_slice() > start
                && end.is_none_or(|e| user.as_slice() < e)
                && out.last().is_none_or(|l| *l < user)
            {
                out.push(user);
            }
        }
        Ok(out)
    }

    /// The keys and values in `(start, end)`, ascending, as this transaction
    /// sees them.
    pub fn scan(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Scan<'_>> {
        let store_end = match end {
            Bound::Unbounded => None,
            Bound::Included(k) => Some(after_versions(k)),
            Bound::Excluded(k) => Some(first_version(k)),
        };
        let writes = self.writes.range::<[u8], _>((start, end)).peekable();
        Ok(Scan {
            txn: self,
            from: Some(start.map(<[u8]>::to_vec)),
            store_end,
            buffer: Chunk::default(),
            live: Vec::new(),
            pos: 0,
            bufs: Default::default(),
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
        if self.writes.is_empty() && self.spilled.is_none() {
            return Ok(Timestamp(self.start));
        }
        let engine = Arc::clone(&self.engine);
        let spilled = self.spilled;
        let (ts, lsn) = {
            let mut last = lock(&engine.commit);
            for key in self.writes.keys() {
                if !self.is_fresh(key) && engine.conflicts(key, self.start, spilled)? {
                    return Err(Error::Conflict { key: key.clone() });
                }
            }
            // (Spilled keys were checked as they spilled, and every commit
            // since has checked its keys against them.)
            // Serializable transactions record every read; others only
            // their validated ones.
            self.validate_reads(&engine)?;
            let ts = *last + 1;
            let wm = engine.current_watermark();
            let mut ops = Vec::with_capacity(self.writes.len() + 1);
            for (key, value) in &self.writes {
                if !self.is_fresh(key) {
                    engine.garbage(key, wm, &mut ops)?;
                }
                ops.push((encode_key(key, ts), Some(encode_value(value.as_deref()))));
                if let Some(id) = spilled {
                    // Written again after it spilled: this write is the one.
                    ops.push((side_key(id, key), None));
                }
            }
            if let Some(id) = spilled {
                // The commit: the side area's writes are this
                // transaction's at `ts`.
                ops.push((status_key(id), Some(ts.to_be_bytes().to_vec())));
            }
            let lsn = engine.store.write(&Batch { meta: ts, ops })?;
            *last = ts;
            if let Some(id) = spilled {
                lock(&engine.unresolved).insert(id, ts);
                lock(&engine.spilling).remove(&id);
                self.spilled = None; // the side area is the commit's now
            }
            let mut recent = lock(&engine.recent);
            for key in self.writes.keys() {
                recent.insert(key.clone(), ts);
            }
            if let Some((lo, hi)) = self.span.take() {
                lock(&engine.recent_spans).push((lo, hi, ts));
            }
            if recent.len() > RECENT_PRUNE || lock(&engine.recent_spans).len() > 64 {
                // Nobody started before the oldest running transaction can
                // still commit, so older writes cannot conflict any more.
                let oldest = lock(&engine.active).keys().next().copied().unwrap_or(ts);
                recent.retain(|_, t| *t > oldest);
                lock(&engine.recent_spans).retain(|(_, _, t)| *t > oldest);
            }
            drop(recent);
            (ts, lsn)
        };
        engine.store.wait_durable(lsn)?;
        engine.visible.fetch_max(ts, Ordering::AcqRel);
        if let Some(id) = spilled {
            // Committed: the side area's writes move into versions.  Readers
            // find them there meanwhile; a failure here is finished when the
            // engine next opens.
            let _ = engine.resolve(id, ts);
        }
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
        let spans = lock(&engine.recent_spans);
        let spans: Vec<_> = spans.iter().filter(|(_, _, t)| *t > self.start).collect();
        for k in &reads.keys {
            if recent.get(k).is_some_and(|&ts| ts > self.start) {
                return Err(Error::Conflict { key: k.clone() });
            }
            if spans.iter().any(|(lo, hi, _)| lo <= k && k <= hi) {
                return Err(Error::Conflict { key: k.clone() });
            }
        }
        for (s, e) in &reads.ranges {
            // A span a large commit wrote, overlapping a range read.
            let below_end = |k: &Vec<u8>| match e {
                Bound::Unbounded => true,
                Bound::Included(e) => k <= e,
                Bound::Excluded(e) => k < e,
            };
            let above_start = |k: &Vec<u8>| match s {
                Bound::Unbounded => true,
                Bound::Included(s) => k >= s,
                Bound::Excluded(s) => k > s,
            };
            if let Some((lo, _, _)) = spans
                .iter()
                .find(|(lo, hi, _)| below_end(lo) && above_start(hi))
            {
                return Err(Error::Conflict { key: lo.clone() });
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
    /// The user key the next store batch starts at (`None` once the store
    /// is done).
    from: Option<Bound<Vec<u8>>>,
    store_end: Option<Vec<u8>>,
    /// Visible store entries read ahead (a deleted one with an empty
    /// value and `live` false), and the next one's index.
    buffer: Chunk,
    live: Vec<bool>,
    pos: usize,
    /// Buffers reused by refills: a store key and value.
    bufs: [Vec<u8>; 2],
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

/// Keys and values packed into one buffer, what a chunked scan fills
/// ([`Scan::next_chunk`]); reused from chunk to chunk, so reading one
/// allocates nothing once the buffer has grown.
#[derive(Clone, Debug, Default)]
pub struct Chunk {
    data: Vec<u8>,
    /// Per entry: where its key starts, where its value starts.
    ents: Vec<(usize, usize)>,
}

impl Chunk {
    /// An empty chunk.
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of entries.
    #[inline]
    pub fn len(&self) -> usize {
        self.ents.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.ents.is_empty()
    }

    /// The bytes the entries take.
    pub fn bytes(&self) -> usize {
        self.data.len()
    }

    /// Removes every entry (the buffer kept).
    pub fn clear(&mut self) {
        self.data.clear();
        self.ents.clear();
    }

    /// Adds an entry.
    pub fn push(&mut self, key: &[u8], value: &[u8]) {
        let k = self.data.len();
        self.data.extend_from_slice(key);
        self.ents.push((k, self.data.len()));
        self.data.extend_from_slice(value);
    }

    #[inline]
    fn end(&self, i: usize) -> usize {
        self.ents.get(i + 1).map_or(self.data.len(), |e| e.0)
    }

    /// Entry `i`'s key.
    #[inline]
    pub fn key(&self, i: usize) -> &[u8] {
        let (k, v) = self.ents[i];
        &self.data[k..v]
    }

    /// Entry `i`'s value.
    #[inline]
    pub fn value(&self, i: usize) -> &[u8] {
        &self.data[self.ents[i].1..self.end(i)]
    }

    /// The entries in order.
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        (0..self.len()).map(|i| (self.key(i), self.value(i)))
    }
}

/// Where a scan's next entry is: the read-ahead buffer, or the
/// transaction's own writes.
enum Next<'t> {
    Store(usize),
    Write(&'t [u8], &'t [u8]),
}

impl Scan<'_> {
    /// Reads the next batch of visible versions from the store — and of the
    /// side area's writes the transaction sees: its own, and those of
    /// committed transactions not yet moved into versions — in one read of
    /// the tree.
    fn refill(&mut self, max: usize, bytes: usize) -> Result<()> {
        let Some(from) = self.from.take() else {
            return Ok(());
        };
        let txn = self.txn;
        let engine = &txn.engine;
        // Side areas to read, each with its rank: a committed one's commit
        // timestamp (a version at or below the snapshot), the
        // transaction's own above everything.
        let mut sides: Vec<(u64, u64)> = engine
            .unresolved_ids()
            .into_iter()
            .filter(|&(_, ts)| ts <= txn.start)
            .collect();
        if let Some(id) = txn.spilled {
            sides.push((id, u64::MAX));
        }
        let tree = engine.store.read()?;
        let seek = match &from {
            Bound::Unbounded => Vec::new(),
            Bound::Included(k) => first_version(k),
            Bound::Excluded(k) => after_versions(k),
        };
        let end = match &self.store_end {
            Some(e) => Bound::Excluded(e.as_slice()),
            None => Bound::Unbounded,
        };
        // The first user key this batch leaves for the next (`None`: the
        // range's end).
        let mut upper: Option<Vec<u8>> = None;
        self.buffer.clear();
        self.live.clear();
        self.pos = 0;
        if sides.is_empty() {
            // No side area to merge (the usual case): versions straight
            // into the buffer, each user key decoded in place.
            let [k, v] = &mut self.bufs;
            let buf = &mut self.buffer;
            'ranges: for mut range in versions(&tree, Bound::Included(&seek), end)? {
                while range.next_into(k, v)? {
                    let at = buf.data.len();
                    let ts = decode_key_onto(k, &mut buf.data)
                        .ok_or(Error::Corrupt("undecodable version key"))?;
                    let user = &buf.data[at..];
                    let seen = buf
                        .ents
                        .last()
                        .is_some_and(|&(kk, vv)| buf.data[kk..vv] == *user);
                    if ts > txn.start || seen {
                        // Too new, or an older version of a key already seen.
                        buf.data.truncate(at);
                        continue;
                    }
                    if buf.ents.len() >= max || at >= bytes {
                        upper = Some(user.to_vec());
                        buf.data.truncate(at);
                        break 'ranges;
                    }
                    let value = decode_value(v).ok_or(Error::Corrupt("bad version value"))?;
                    buf.ents.push((at, buf.data.len()));
                    buf.data.extend_from_slice(value.unwrap_or_default());
                    self.live.push(value.is_some());
                }
            }
            if let Some(u) = upper {
                self.from = Some(Bound::Included(u));
            }
            return Ok(());
        }
        let mut found: BTreeMap<Vec<u8>, (u64, Option<Vec<u8>>)> = BTreeMap::new();
        let mut last: Option<Vec<u8>> = None;
        for entry in versions(&tree, Bound::Included(&seek), end)?
            .into_iter()
            .flatten()
        {
            let (k, v) = entry?;
            let (user, ts) = decode_key(&k).ok_or(Error::Corrupt("undecodable version key"))?;
            if ts > txn.start || last.as_deref() == Some(&user[..]) {
                continue; // too new, or an older version of a key already seen
            }
            if found.len() == max {
                upper = Some(user);
                break;
            }
            let value = decode_value(&v).ok_or(Error::Corrupt("bad version value"))?;
            found.insert(user.clone(), (ts, value.map(<[u8]>::to_vec)));
            last = Some(user);
        }
        let range_end = &self.range.1;
        for (id, rank) in sides {
            let prefix = side_prefix(id);
            let with = |k: &[u8]| {
                let mut p = prefix.clone();
                p.extend_from_slice(k);
                p
            };
            let lo = match &from {
                Bound::Unbounded => Bound::Included(prefix.clone()),
                Bound::Included(k) => Bound::Included(with(k)),
                Bound::Excluded(k) => Bound::Excluded(with(k)),
            };
            let hi = match (&upper, range_end) {
                (Some(u), _) => Bound::Excluded(with(u)),
                (None, Bound::Unbounded) => Bound::Excluded(side_prefix(id.wrapping_add(1))),
                (None, Bound::Included(e)) => Bound::Included(with(e)),
                (None, Bound::Excluded(e)) => Bound::Excluded(with(e)),
            };
            let hi = if id == u64::MAX && matches!((&upper, range_end), (None, Bound::Unbounded)) {
                Bound::Excluded(STATUS.to_vec())
            } else {
                hi
            };
            let side = tree.range(
                lo.as_ref().map(Vec::as_slice),
                hi.as_ref().map(Vec::as_slice),
            )?;
            for (taken, e) in side.enumerate() {
                let (k, v) = e?;
                let user = k[10..].to_vec();
                if taken == max {
                    // This side area has more: the batch ends before them.
                    if upper.as_ref().is_none_or(|u| &user < u) {
                        upper = Some(user);
                    }
                    break;
                }
                let value = decode_value(&v).ok_or(Error::Corrupt("bad side value"))?;
                let newer = found.get(&user).is_none_or(|(r, _)| rank > *r);
                if newer {
                    found.insert(user, (rank, value.map(<[u8]>::to_vec)));
                }
            }
        }
        for (user, (_, value)) in found {
            if upper.as_ref().is_some_and(|u| &user >= u) {
                break;
            }
            self.buffer
                .push(&user, value.as_deref().unwrap_or_default());
            self.live.push(value.is_some());
        }
        if let Some(u) = upper {
            self.from = Some(Bound::Included(u));
        }
        Ok(())
    }

    /// Moves to the next visible entry.
    fn advance(&mut self) -> Result<Option<Next<'_>>> {
        loop {
            while self.pos == self.buffer.len() && self.from.is_some() {
                self.refill(SCAN_BATCH, usize::MAX)?;
            }
            let store = (self.pos < self.buffer.len()).then_some(self.pos);
            let write = self.writes.peek().map(|(k, _)| *k);
            let from_writes = match (store, write) {
                (None, None) => return Ok(None),
                (Some(i), Some(w)) if w[..] <= *self.buffer.key(i) => {
                    if w[..] == *self.buffer.key(i) {
                        self.pos += 1; // the transaction's own write wins
                    }
                    true
                }
                (None, Some(_)) => true,
                (Some(_), _) => false,
            };
            if from_writes {
                let (k, v) = self.writes.next().expect("peeked");
                if let Some(v) = v {
                    return Ok(Some(Next::Write(k, v)));
                }
            } else {
                let i = self.pos;
                self.pos += 1;
                if self.live[i] {
                    return Ok(Some(Next::Store(i)));
                }
            }
            // A tombstone: deleted in this snapshot.
        }
    }

    /// Replaces `out`'s entries with the next ones — at most `entries` of
    /// them, and no more once they take `bytes` — through buffers reused
    /// from chunk to chunk; false at the end.
    pub fn next_chunk(&mut self, out: &mut Chunk, entries: usize, bytes: usize) -> Result<bool> {
        out.clear();
        // None of the transaction's own writes left to merge, the read-ahead
        // used up: the store's next entries are the chunk — read into the
        // buffer, which becomes `out` as it is.
        while self.writes.peek().is_none() && self.pos == self.buffer.len() && self.from.is_some() {
            self.refill(entries.max(1), bytes)?;
            if !self.live.iter().all(|&l| l) {
                break; // deletions to skip: entry by entry below
            }
            std::mem::swap(out, &mut self.buffer);
            self.buffer.clear();
            self.live.clear();
            self.pos = 0;
            if !out.is_empty() {
                if self.validated {
                    self.last = Some(out.key(out.len() - 1).to_vec());
                }
                return Ok(true);
            }
        }
        while out.len() < entries.max(1) && out.bytes() < bytes {
            match self.advance()? {
                Some(Next::Store(i)) => {
                    let (k, v) = (self.buffer.key(i), self.buffer.value(i));
                    out.push(k, v);
                }
                Some(Next::Write(k, v)) => out.push(k, v),
                None => {
                    self.done = true;
                    break;
                }
            }
        }
        if self.validated && !out.is_empty() {
            self.last = Some(out.key(out.len() - 1).to_vec());
        }
        Ok(!out.is_empty())
    }
}

impl Iterator for Scan<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        let next = match self.advance() {
            Ok(Some(Next::Store(i))) => Ok(Some((self.buffer.key(i), self.buffer.value(i)))),
            Ok(Some(Next::Write(k, v))) => Ok(Some((k, v))),
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        };
        match next {
            Ok(Some((k, v))) => {
                let (k, v) = (k.to_vec(), v.to_vec());
                if self.validated {
                    self.last = Some(k.clone());
                }
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
