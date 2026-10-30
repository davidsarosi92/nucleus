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
    self, NEXT_ID, RESERVED_END, SIDE, STATUS, after_versions, decode_key, decode_key_onto,
    decode_value, encode_key, encode_value, first_version, intent_of, intent_ts, side_prefix,
    status_key,
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
    /// Transactions whose writes spilled (as intents) and that have not
    /// committed: a commit checks its keys against theirs.  Changed under
    /// the commit lock.
    spilling: Mutex<BTreeSet<u64>>,
    /// Committed transactions with intents vacuum has not made versions
    /// yet: id → commit timestamp.  Readers resolve the intents by it.
    unresolved: Mutex<BTreeMap<u64, u64>>,
    /// How many transactions are in `spilling` or `unresolved` (reads skip
    /// intents when none).
    side_users: AtomicUsize,
    /// Transactions begun so far ([`Engine::activity`]).
    begun: AtomicU64,
    /// A paused vacuum sweep ([`Engine::vacuum_paced`]): where it goes on,
    /// the committed transactions it makes versions of, what it removed.
    sweep: Mutex<Option<(Vec<u8>, Vec<(u64, u64)>, u64)>>,
}

/// Entries vacuum changes per batch (each under the commit lock).
const VACUUM_CHUNK: usize = 16_384;

/// … and in the background ([`Engine::vacuum_paced`]): small batches, so
/// the tree and the commit lock are held only briefly.
pub const BACKGROUND_CHUNK: usize = 2048;

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
                begun: AtomicU64::new(0),
                sweep: Mutex::new(None),
            }),
        };
        engine.inner.recover_side()?;
        Ok(engine)
    }

    /// Whether committed transactions have intents vacuum has not made
    /// versions yet.
    pub fn intents_pending(&self) -> bool {
        !lock(&self.inner.unresolved).is_empty()
    }

    /// How many transactions have begun: unchanged over a while, the
    /// database was idle.
    pub fn activity(&self) -> u64 {
        self.inner.begun.load(Ordering::Relaxed)
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
        self.inner.begun.fetch_add(1, Ordering::Relaxed);
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

    /// Removes every version no snapshot can see any more, makes committed
    /// transactions' intents versions at their commit timestamps and
    /// deletes aborted ones'.  Returns how many entries it removed.
    /// Commits also collect the old versions of the keys they write; this
    /// sweeps the rest.  After a whole sweep, the transactions committed
    /// before it began have no intent left: their status records go.
    pub fn vacuum(&self) -> Result<u64> {
        *lock(&self.inner.sweep) = None;
        self.vacuum_paced(VACUUM_CHUNK, &mut || true)
    }

    /// [`vacuum`](Self::vacuum) in batches of about `chunk` entries,
    /// `go_on` asked between them (a background vacuum's): `false` stops
    /// the sweep there, and the next call goes on from it — with the
    /// committed transactions it began with, whose statuses go only when a
    /// sweep has passed every key.  What it removed (in the sweep so far).
    pub fn vacuum_paced(&self, chunk: usize, go_on: &mut dyn FnMut() -> bool) -> Result<u64> {
        let (mut from, committed, mut removed) = lock(&self.inner.sweep)
            .take()
            .unwrap_or_else(|| (Vec::new(), self.inner.unresolved_ids(), 0));
        loop {
            // In chunks, releasing the commit lock between them.
            let commit = lock(&self.inner.commit);
            let wm = self.inner.current_watermark();
            let others = self.inner.others(None);
            let mut ops = Vec::new();
            let mut last_user: Option<Vec<u8>> = None;
            let mut kept_one = false;
            let mut next = None;
            {
                let tree = self.inner.store.read()?;
                let mut pinned = false;
                for entry in versions(&tree, Bound::Included(&from), Bound::Unbounded)?
                    .into_iter()
                    .flatten()
                {
                    let (k, v) = entry?;
                    let (user, ts) =
                        decode_key(&k).ok_or(Error::Corrupt("undecodable version key"))?;
                    if last_user.as_deref() != Some(&user[..]) {
                        if ops.len() >= chunk.max(1) {
                            next = Some(first_version(&user));
                            break;
                        }
                        last_user = Some(user.clone());
                        kept_one = false;
                        pinned = false;
                    }
                    if let Some(id) = intent_of(ts) {
                        // A key with an intent keeps its versions this sweep.
                        pinned = true;
                        if let Some(&(_, cts)) = committed.iter().find(|&&(u, _)| u == id) {
                            ops.push((encode_key(&user, cts), Some(v)));
                            ops.push((k, None));
                            removed += 1;
                        } else if !others.contains(&id)
                            && !lock(&self.inner.unresolved).contains_key(&id)
                        {
                            ops.push((k, None)); // aborted
                            removed += 1;
                        }
                        continue;
                    }
                    if ts > wm || pinned {
                        continue; // still needed by someone
                    }
                    let tombstone = decode_value(&v)
                        .ok_or(Error::Corrupt("bad version value"))?
                        .is_none();
                    if kept_one || tombstone {
                        ops.push((k, None));
                        removed += 1;
                    }
                    kept_one = true;
                }
            }
            if next.is_none() {
                // The whole keyspace swept: these transactions' intents are
                // all versions now.
                for &(id, _) in &committed {
                    ops.push((status_key(id), None));
                }
            }
            if !ops.is_empty() {
                let lsn = self.inner.store.write(&Batch { meta: *commit, ops })?;
                drop(commit);
                if next.is_none() {
                    // Durable — and with it every batch before (the log is
                    // in order) — before the statuses are forgotten: an
                    // intent never outlives its status.
                    self.inner.store.wait_durable(lsn)?;
                    for &(id, _) in &committed {
                        lock(&self.inner.unresolved).remove(&id);
                        self.inner.side_users.fetch_sub(1, Ordering::AcqRel);
                    }
                }
            }
            match next {
                Some(n) => from = n,
                None => return Ok(removed),
            }
            if !go_on() {
                *lock(&self.inner.sweep) = Some((from, committed, removed));
                return Ok(removed);
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

    /// Intents in the store — committed ones vacuum has not made versions
    /// yet, running and aborted ones' — for tests.
    pub fn intent_count(&self) -> Result<u64> {
        let tree = self.inner.store.read()?;
        let mut n = 0;
        for e in versions(&tree, Bound::Unbounded, Bound::Unbounded)?
            .into_iter()
            .flatten()
        {
            let (k, _) = e?;
            if decode_key(&k).is_some_and(|(_, ts)| intent_of(ts).is_some()) {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Entries in an older engine's side area — for tests.
    pub fn side_count(&self) -> Result<u64> {
        let tree = self.inner.store.read()?;
        let mut n = 0;
        for e in tree.range(Bound::Included(&SIDE[..]), Bound::Excluded(&STATUS[..]))? {
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

    /// The running transactions with intents, but `me`.
    fn others(&self, me: Option<u64>) -> Vec<u64> {
        if self.side_users.load(Ordering::Acquire) == 0 {
            return Vec::new();
        }
        lock(&self.spilling)
            .iter()
            .copied()
            .filter(|&id| Some(id) != me)
            .collect()
    }

    /// The newest version of `user` visible at `ts` to transaction `me` (its
    /// own intents first): `Some(Some(v))` a value, `Some(None)` a
    /// tombstone, `None` no version at all.
    fn read_at(&self, user: &[u8], ts: u64, me: Option<u64>) -> Result<Option<Option<Vec<u8>>>> {
        Ok(self.look(user, ts, me, false)?.1)
    }

    /// One look at `user`'s versions for transaction `me` reading at `ts`:
    /// whether writing it conflicts — a newer commit wrote it (a version,
    /// or an intent committed after `ts`), or another running transaction
    /// has an intent of it — and the value `me` sees (its own intent, else
    /// the newest of the committed intents and versions at or below `ts`).
    /// `conflict_only`: the value is not wanted (the walk stops sooner).
    #[allow(clippy::type_complexity)]
    fn look(
        &self,
        user: &[u8],
        ts: u64,
        me: Option<u64>,
        conflict_only: bool,
    ) -> Result<(bool, Option<Option<Vec<u8>>>)> {
        let unresolved = self.unresolved_ids();
        let others = self.others(me);
        let tree = self.store.read()?;
        let decode = |v: &[u8]| -> Result<Option<Vec<u8>>> {
            Ok(decode_value(v)
                .ok_or(Error::Corrupt("bad version value"))?
                .map(<[u8]>::to_vec))
        };
        if unresolved.is_empty() && others.is_empty() && me.is_none() {
            // No intent anyone sees: the versions alone (an intent left by
            // an aborted transaction sorts first and is passed over).
            let from = if conflict_only {
                first_version(user)
            } else {
                encode_key(user, ts)
            };
            let to = after_versions(user);
            for entry in tree.range(Bound::Included(&from), Bound::Excluded(&to))? {
                let (k, v) = entry?;
                let (_, vts) = decode_key(&k).ok_or(Error::Corrupt("undecodable version key"))?;
                if intent_of(vts).is_some() {
                    continue;
                }
                if conflict_only {
                    return Ok((vts > ts, None));
                }
                return Ok((false, Some(decode(&v)?)));
            }
            return Ok((false, None));
        }
        let mut conflict = false;
        let mut best: Option<(u64, Vec<u8>)> = None;
        let mut regular = false;
        for entry in tree.range(
            Bound::Included(&first_version(user)),
            Bound::Excluded(&after_versions(user)),
        )? {
            let (k, v) = entry?;
            let (_, vts) = decode_key(&k).ok_or(Error::Corrupt("undecodable version key"))?;
            let rank = match intent_of(vts) {
                Some(id) if Some(id) == me => Some(u64::MAX),
                Some(id) => match unresolved.iter().find(|&&(u, _)| u == id) {
                    Some(&(_, cts)) if cts > ts => {
                        conflict = true;
                        None
                    }
                    Some(&(_, cts)) => Some(cts),
                    None => {
                        conflict |= others.contains(&id);
                        None // running (another's) or aborted: not seen
                    }
                },
                None => {
                    // The first version: newer than the snapshot conflicts;
                    // the first at or below it is the newest seen.
                    if !regular && vts > ts {
                        conflict = true;
                        if conflict_only {
                            break;
                        }
                    }
                    regular = true;
                    if vts > ts {
                        continue;
                    }
                    if best.as_ref().is_none_or(|(b, _)| vts > *b) {
                        best = Some((vts, v));
                    }
                    break;
                }
            };
            if let Some(r) = rank
                && best.as_ref().is_none_or(|(b, _)| r > *b)
            {
                best = Some((r, v));
            }
            if conflict && conflict_only {
                break;
            }
        }
        let value = match best {
            None => None,
            Some((_, v)) => Some(decode(&v)?),
        };
        Ok((conflict, value))
    }

    /// Whether writing `user` conflicts for a transaction that read at
    /// `start` (`me`, if it has intents): see [`Inner::look`].
    fn conflicts(&self, user: &[u8], start: u64, me: Option<u64>) -> Result<bool> {
        Ok(self.look(user, start, me, true)?.0)
    }

    /// The transactions as the engine found them on opening: those with a
    /// status record committed — their intents read as versions at its
    /// timestamp until vacuum makes them versions —, every other one's
    /// intents aborted (passed over, and swept by vacuum); the next id
    /// past every id used.  An older engine's side area: the writes of
    /// transactions that never committed dropped, those of committed ones
    /// moved into versions.
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
            if let Some(v) = tree.get(&NEXT_ID)? {
                let next = u64::from_be_bytes(
                    v[..]
                        .try_into()
                        .map_err(|_| Error::Corrupt("bad next id record"))?,
                );
                max_id = max_id.max(next.saturating_sub(1));
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
        // Every id in the side area without a status never committed; the
        // ids with one there committed under an older engine.
        let mut sided = BTreeSet::new();
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
            } else {
                sided.insert(found);
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
            if sided.contains(&id) {
                self.resolve(id, ts)?;
            }
        }
        Ok(())
    }

    /// Deletes the intents of aborted transaction `id` among the keys
    /// `lo..=hi`, a batch at a time.
    fn drop_intents(&self, id: u64, lo: &[u8], hi: &[u8]) -> Result<()> {
        let mut from = first_version(lo);
        let to = after_versions(hi);
        let mine = intent_ts(id);
        loop {
            let last = lock(&self.commit);
            let mut ops = Vec::new();
            let mut next = None;
            {
                let tree = self.store.read()?;
                for e in versions(&tree, Bound::Included(&from), Bound::Excluded(&to))?
                    .into_iter()
                    .flatten()
                {
                    let (k, _) = e?;
                    let (user, ts) =
                        decode_key(&k).ok_or(Error::Corrupt("undecodable version key"))?;
                    if ts == mine {
                        ops.push((k, None));
                        if ops.len() == SIDE_CHUNK {
                            next = Some(after_versions(&user));
                            break;
                        }
                    }
                }
            }
            if !ops.is_empty() {
                self.store.write(&Batch { meta: *last, ops })?;
            }
            match next {
                Some(n) => from = n,
                None => return Ok(()),
            }
        }
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

    /// A commit's checks of the keys it writes, in one walk over each key's
    /// versions (and one read of the tree for them all): the first key a
    /// newer commit wrote — or another running transaction has an intent
    /// of — is a conflict (`Some`); else the versions the commit lets go go
    /// to `ops` (those older than the one it keeps; none of a key with an
    /// intent still to be made a version).  Fresh keys
    /// ([`Transaction::fresh`]) are skipped.
    #[allow(clippy::type_complexity)]
    fn commit_checks(
        &self,
        keys: &mut dyn Iterator<Item = (&[u8], bool)>,
        start: u64,
        me: Option<u64>,
        wm: u64,
        ops: &mut Vec<(Vec<u8>, Option<Vec<u8>>)>,
    ) -> Result<Option<Vec<u8>>> {
        let unresolved = self.unresolved_ids();
        let others = self.others(me);
        let tree = self.store.read()?;
        let mut dropped = Vec::new();
        for (user, fresh) in keys {
            if fresh {
                continue;
            }
            // Intents first: another's running, or committed after the
            // snapshot, conflicts; any but an aborted one pins the
            // versions.  Then the versions, newest first: the first decides
            // the conflict; those at or below the watermark past the first
            // (or a tombstone) can go.
            dropped.clear();
            let mut pinned = false;
            let mut newest = None;
            let mut kept = false;
            for entry in tree.range(
                Bound::Included(&first_version(user)),
                Bound::Excluded(&after_versions(user)),
            )? {
                let (k, v) = entry?;
                let (_, vts) = decode_key(&k).ok_or(Error::Corrupt("undecodable version key"))?;
                if let Some(id) = intent_of(vts) {
                    if Some(id) == me {
                        pinned = true;
                    } else if let Some(&(_, cts)) = unresolved.iter().find(|&&(u, _)| u == id) {
                        if cts > start {
                            return Ok(Some(user.to_vec()));
                        }
                        pinned = true;
                    } else if others.contains(&id) {
                        return Ok(Some(user.to_vec()));
                    }
                    continue;
                }
                if newest.is_none() {
                    if vts > start {
                        return Ok(Some(user.to_vec()));
                    }
                    newest = Some(vts);
                }
                if vts <= wm {
                    let tombstone = decode_value(&v)
                        .ok_or(Error::Corrupt("bad version value"))?
                        .is_none();
                    if kept || tombstone {
                        dropped.push((k, None));
                    }
                    kept = true;
                }
            }
            if !pinned {
                ops.append(&mut dropped);
            }
        }
        Ok(None)
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
        // The intents of a transaction that did not commit: aborted once it
        // stops running (no reader sees them, no writer conflicts with
        // them), then deleted (on a failure here, by vacuum).
        if let Some(id) = self.spilled.take() {
            lock(&self.engine.spilling).remove(&id);
            self.engine.side_users.fetch_sub(1, Ordering::AcqRel);
            if let Some((lo, hi)) = self.span.take() {
                let _ = self.engine.drop_intents(id, &lo, &hi);
            }
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
        Ok(self
            .engine
            .read_at(key, self.start, self.spilled)?
            .flatten())
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
        // A fresh key has no versions to look at (but this transaction's
        // own intents, once it spilled).
        if self.is_fresh(key) && self.spilled.is_none() {
            self.record(key, Some(value))?;
            return Ok(true);
        }
        if self.isolation == Isolation::Serializable {
            lock(&self.reads).keys.insert(key.to_vec());
        }
        let (conflict, seen) = self.engine.look(key, self.start, self.spilled, false)?;
        if seen.flatten().is_some() {
            return Ok(false);
        }
        if conflict && !self.is_fresh(key) {
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

    /// Moves the writes held in memory into the store as intents (versions
    /// stamped with this transaction's id, which no one else sees before it
    /// commits).  Under the commit lock, each key is checked as a commit
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
        // The next id past this one, before its intents (so no later
        // transaction is given an id an intent of this one has).
        ops.push((NEXT_ID.to_vec(), Some((id + 1).to_be_bytes().to_vec())));
        let mut bytes = 0;
        for (key, value) in std::mem::take(&mut self.writes) {
            bytes += key.len() + value.as_ref().map_or(0, Vec::len) + 16;
            ops.push((
                encode_key(&key, intent_ts(id)),
                Some(encode_value(value.as_deref())),
            ));
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
            batch: FIRST_BATCH,
        })
    }

    /// The first visible entry in the range, read without reading ahead.
    pub fn first(
        &self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let mut scan = self.scan(start, end)?;
        scan.batch = 1;
        scan.next().transpose()
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
            let wm = engine.current_watermark();
            let mut ops = Vec::with_capacity(self.writes.len() + 1);
            // Each key's conflict and the versions its write lets go, in
            // one walk (spilled keys were checked as they spilled, and every
            // commit since has checked its keys against them).
            let mut keys = self.writes.keys().map(|k| (k.as_slice(), self.is_fresh(k)));
            if let Some(key) = engine.commit_checks(&mut keys, self.start, spilled, wm, &mut ops)? {
                return Err(Error::Conflict { key });
            }
            // Serializable transactions record every read; others only
            // their validated ones.
            self.validate_reads(&engine)?;
            let ts = *last + 1;
            for (key, value) in &self.writes {
                ops.push((encode_key(key, ts), Some(encode_value(value.as_deref()))));
                if let Some(id) = spilled {
                    // Written again after it spilled: this write is the one.
                    ops.push((encode_key(key, intent_ts(id)), None));
                }
            }
            if let Some(id) = spilled {
                // The commit: this transaction's intents are versions at
                // `ts` (and become them, as vacuum rewrites them).
                ops.push((status_key(id), Some(ts.to_be_bytes().to_vec())));
            }
            let lsn = engine.store.write(&Batch { meta: ts, ops })?;
            *last = ts;
            if let Some(id) = spilled {
                // Committed before it stops running: a writer never finds
                // its intents neither (a reader at `ts` starts after both).
                lock(&engine.unresolved).insert(id, ts);
                lock(&engine.spilling).remove(&id);
                self.spilled = None; // its intents are the commit's now
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
    /// The next refill's entries.
    batch: usize,
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

/// Store entries a refill passes over (intents no one sees, versions too
/// new) before it ends its batch, letting the tree's lock go.
const PASS_LIMIT: usize = 4096;

/// The first refill's entries: a scan that wants one entry (the first key
/// of a range, a `LIMIT 1`) reads a few, not a batch; each refill after
/// doubles, up to [`SCAN_BATCH`].
const FIRST_BATCH: usize = 8;

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
            let mut passed = 0;
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
                    if !seen && ts > txn.start {
                        // Too new (another's intent, a later commit): passed
                        // over — many of them, and the batch ends here, the
                        // tree's lock let go, the key read again next time.
                        passed += 1;
                        if passed >= PASS_LIMIT {
                            upper = Some(user.to_vec());
                            buf.data.truncate(at);
                            break 'ranges;
                        }
                    }
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
        // Intents seen — committed ones at their commit timestamps, the
        // transaction's own above everything — against the versions: per
        // user key the highest ranked.
        let rank_of = |id: u64| sides.iter().find(|&&(i, _)| i == id).map(|&(_, r)| r);
        // A user key's versions come together (intents first): the best so
        // far — rank, live, value — goes out when the next key begins.
        let [k, v] = &mut self.bufs;
        let (buf, live) = (&mut self.buffer, &mut self.live);
        let mut user: Vec<u8> = Vec::new();
        let mut next: Vec<u8> = Vec::new();
        let mut value: Vec<u8> = Vec::new();
        let mut best: Option<(u64, bool)> = None;
        let mut have = false;
        let mut taken = false;
        let mut passed = 0;
        let flush = |buf: &mut Chunk, live: &mut Vec<bool>, user: &[u8], best, value: &[u8]| {
            if let Some((_, alive)) = best {
                buf.push(user, if alive { value } else { &[] });
                live.push(alive);
            }
        };
        'ranges: for mut range in versions(&tree, Bound::Included(&seek), end)? {
            while range.next_into(k, v)? {
                next.clear();
                let ts = decode_key_onto(k, &mut next)
                    .ok_or(Error::Corrupt("undecodable version key"))?;
                if !have || next != user {
                    if have {
                        flush(buf, live, &user, best, &value);
                    }
                    if buf.len() >= max || passed >= PASS_LIMIT {
                        upper = Some(next.clone());
                        have = false;
                        break 'ranges;
                    }
                    std::mem::swap(&mut user, &mut next);
                    have = true;
                    best = None;
                    taken = false;
                }
                passed += 1;
                let rank = match intent_of(ts) {
                    Some(id) => match rank_of(id) {
                        Some(r) => r,
                        None => continue, // running, aborted, or committed later
                    },
                    None => {
                        if ts > txn.start || taken {
                            continue; // too new, or an older version
                        }
                        taken = true;
                        ts
                    }
                };
                if best.is_none_or(|(r, _)| rank > r) {
                    let stored = decode_value(v).ok_or(Error::Corrupt("bad version value"))?;
                    value.clear();
                    value.extend_from_slice(stored.unwrap_or_default());
                    best = Some((rank, stored.is_some()));
                }
            }
        }
        if have {
            flush(buf, live, &user, best, &value);
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
                self.refill(self.batch, usize::MAX)?;
                self.batch = (self.batch * 2).min(SCAN_BATCH);
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
