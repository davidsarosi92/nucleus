use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use nucleus_platform::Platform;
use nucleus_storage::btree::{LeafEntry, LeafFilter};
use nucleus_storage::node::MAX_KEY;
use nucleus_storage::{
    Batch, BranchInfo, Deferred, EngineKind, Recovery, Rekeying, Store, StoreOptions, Tables, Tails,
};

use crate::error::{Error, Result};
use crate::mvcc::{
    self, COMMIT_TIMES, NEXT_ID, RESERVED_END, SIDE, STATUS, UNLOGGED, after_versions, decode_key,
    decode_key_onto, decode_time, decode_value, encode_key, encode_value, first_version, intent_of,
    intent_ts, side_prefix, status_key, time_key, unlogged_key,
};
use crate::recent::Recent;

/// A range of user keys ([`Engine::changed_since`]).
pub type UserRange = (Bound<Vec<u8>>, Bound<Vec<u8>>);

/// Whether a range starting at `start` overlaps or touches one ending at
/// `end` (both inclusive or unbounded).
fn touches(end: &Bound<Vec<u8>>, start: &Bound<Vec<u8>>) -> bool {
    match (end, start) {
        (Bound::Included(e), Bound::Included(s)) => s <= e,
        (Bound::Excluded(e), Bound::Included(s) | Bound::Excluded(s)) => s < e,
        (Bound::Included(e), Bound::Excluded(s)) => s < e,
        _ => true,
    }
}
use crate::writes::WriteSet;

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
    /// Keep old versions for this long (µs; 0: not by time), so the
    /// snapshot of any time that far back stays readable: each commit's
    /// time is recorded while it is set ([`Engine::snapshot_at`],
    /// PLAN.md §M14.14).  Both kept when both are set.
    pub history_time: u64,
    /// A transaction keeps its writes in memory up to about this many
    /// bytes; past it they spill into the store (the side area, §5.3), so
    /// a transaction of any size runs in bounded memory.
    pub txn_memory: usize,
    /// Write what a transaction puts into a range it declared fresh
    /// ([`Transaction::fresh`]) without the WAL, as final versions; its
    /// commit makes them durable by a checkpoint before the commit record
    /// (PLAN.md D16; PostgreSQL's `wal_level = minimal`).  Off, every write
    /// is logged — what anything replaying the whole WAL needs (a replica,
    /// a point-in-time restore).
    pub wal_minimal: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            store: StoreOptions::default(),
            // 1 GiB (PostgreSQL's `max_wal_size`), or 64 MiB on 32-bit
            // targets: more WAL between checkpoints, fewer of them.
            checkpoint_wal_bytes: if cfg!(target_pointer_width = "64") {
                1 << 30
            } else {
                64 << 20
            },
            history: 0,
            history_time: 0,
            txn_memory: 64 << 20,
            wal_minimal: true,
        }
    }
}

/// How often the oldest snapshot a retention by time keeps is found
/// again (µs): in between, the one found last stands — older than the
/// true one, so it keeps more, never less.
const TIME_FLOOR_EVERY: u64 = 1_000_000;

/// The engines of a database's branches, by branch id: one engine per
/// branch, whoever asks (two would hand out the same timestamps).
type Registry = Arc<Mutex<BTreeMap<u32, std::sync::Weak<Inner>>>>;

struct Inner {
    store: Arc<Store>,
    opts: EngineOptions,
    /// Every branch's engine (PLAN.md §M14.13), shared by them all.
    branches: Registry,
    /// The background checkpointer, where the platform has threads to
    /// spare (§M14.20 W4); without one, checkpoints run where they fall
    /// due.
    checkpointer: Option<Checkpointer>,
    /// The commit lock: the last commit timestamp handed out.
    commit: Mutex<u64>,
    /// Every commit up to here is durable and applied: new snapshots start here.
    visible: AtomicU64,
    /// Start timestamps of running transactions (with counts), guarding
    /// their versions from garbage collection.
    active: Mutex<BTreeMap<u64, usize>>,
    /// When each running transaction began (µs since the epoch) and a
    /// number telling it apart: the oldest's age is a metric (PLAN.md
    /// §M14.18).
    began: Mutex<std::collections::BTreeSet<(u64, u64)>>,
    began_seq: AtomicU64,
    /// What recent commits wrote (keys, and the spans of those that
    /// spilled), with their timestamps: what writes check conflicts and
    /// serializable transactions validate their reads against.  Commits
    /// no running transaction can conflict with are pruned.  Changed only
    /// under the commit lock.
    recent: Mutex<Recent>,
    /// The next id of a transaction whose writes spill.
    next_id: AtomicU64,
    /// Transactions whose writes spilled (as intents) and that have not
    /// committed, with the span of their intents' keys: a commit checks
    /// its keys inside a span against their intents.  Changed under the
    /// commit lock.
    spilling: RwLock<BTreeMap<u64, Spans>>,
    /// Committed transactions with intents vacuum has not made versions
    /// yet: id → commit timestamp and the span of the intents' keys (`None`
    /// after a restart: unknown, so everywhere).  Readers resolve the
    /// intents by it — only the reads a span covers (§M14.20 W5).
    unresolved: RwLock<BTreeMap<u64, (u64, Option<Spans>)>>,
    /// How many transactions are in `spilling` or `unresolved` (reads skip
    /// intents when none).
    side_users: AtomicUsize,
    /// Transactions begun so far ([`Engine::activity`]).
    begun: AtomicU64,
    /// A paused vacuum sweep ([`Engine::vacuum_paced`]): where it goes on,
    /// the committed transactions it makes versions of, what it removed.
    sweep: Mutex<Option<(Vec<u8>, Vec<(u64, u64)>, u64)>>,
    /// The fresh ranges running transactions write without the WAL
    /// (D16), `[start, end)`; none overlaps another.  No other transaction
    /// may put an entry into one: a spill or a commit with a key in one
    /// conflicts.  Changed under the commit lock (taken first).
    unlogged: Mutex<Vec<(Vec<u8>, Vec<u8>)>>,
    /// Key ranges watched ([`Engine::watch`]), `[start, end)`: a commit
    /// writing into one raises `watched_at`.  Changed under the commit
    /// lock (taken first).
    watched: RwLock<Vec<(Vec<u8>, Vec<u8>)>>,
    /// The highest commit timestamp that wrote into a watched range (or
    /// at which a range began to be watched) — raised under the commit
    /// lock before the commit is visible.
    watched_at: AtomicU64,
    /// Key prefixes mirrored ([`Engine::mirror`]): (source, destination).
    /// Changed under the commit lock (taken first).
    mirrors: RwLock<Arc<Vec<(Vec<u8>, Vec<u8>)>>>,
    /// How many pairs `mirrors` holds (writes look at it first).
    mirroring: AtomicUsize,
    /// The platform (its clock gives commits their times).
    platform: Arc<dyn Platform>,
    /// The last commit time recorded (µs since the Unix epoch): the next
    /// is never earlier.  Changed under the commit lock.
    last_time: AtomicU64,
    /// The log position of the last commit's record.  Changed under the
    /// commit lock ([`Engine::settle`]).
    last_lsn: AtomicU64,
    /// With a retention by time, the oldest snapshot it keeps readable —
    /// the last commit at or before now − retention — as last found, and
    /// when (the platform's monotonic µs; `u64::MAX`: never).
    time_floor: AtomicU64,
    time_floor_found: AtomicU64,
}

/// The thread that checkpoints when the WAL since the last checkpoint
/// passes [`EngineOptions::checkpoint_wal_bytes`] — off the writers' path:
/// a checkpoint's slow half runs while they write (`Store::checkpoint_begin`).
///
/// Beside it, the compactor: the engines' background work
/// (`Store::compact`, an LSM tree's compaction), woken after each
/// checkpoint, on a thread of its own — off the writers' path and off the
/// checkpoints' (PLAN.md §M14.28 L4a).
struct Checkpointer {
    signal: Arc<Signal>,
    thread: Option<std::thread::JoinHandle<()>>,
    compactor: Arc<Signal>,
    compactor_thread: Option<std::thread::JoinHandle<()>>,
}

/// What wakes the checkpointer: (a checkpoint is due, stop).
#[derive(Default)]
struct Signal {
    state: Mutex<(bool, bool)>,
    wake: Condvar,
}

impl Checkpointer {
    fn start(store: Arc<Store>, limit: u64) -> std::io::Result<Self> {
        let compactor = Arc::new(Signal::default());
        let c = Arc::clone(&compactor);
        let st = Arc::clone(&store);
        let compactor_thread = std::thread::Builder::new()
            .name("nucleus-compact".into())
            .spawn(move || {
                while c.wait() {
                    let mut go_on = || !lock(&c.state).1;
                    // A failure leaves the runs as they were (the store
                    // poisoned or not): the next checkpoint tries again.
                    let _ = st.compact(&mut go_on);
                }
            })?;
        let signal = Arc::new(Signal::default());
        let s = Arc::clone(&signal);
        let cs = Arc::clone(&compactor);
        let thread = std::thread::Builder::new()
            .name("nucleus-checkpoint".into())
            .spawn(move || {
                while s.wait() {
                    // A failure poisons the store: every later write and
                    // read reports it.
                    match store.checkpoint_past(limit, unlogged_limit(limit)) {
                        Err(_) => return,
                        Ok(true) => cs.raise(),
                        Ok(false) => {}
                    }
                }
            })?;
        Ok(Self {
            signal,
            thread: Some(thread),
            compactor,
            compactor_thread: Some(compactor_thread),
        })
    }

    fn wake(&self) {
        self.signal.raise();
    }
}

impl Signal {
    /// Something due.
    fn raise(&self) {
        lock(&self.state).0 = true;
        self.wake.notify_one();
    }

    /// Waits until something is due (true) or stop (false).
    fn wait(&self) -> bool {
        let mut st = lock(&self.state);
        while !st.0 && !st.1 {
            st = self.wake.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        st.0 = false;
        !st.1
    }

    fn stop(&self) {
        lock(&self.state).1 = true;
        self.wake.notify_one();
    }
}

impl Drop for Checkpointer {
    /// Stops the thread, after the checkpoint it may be running: the store
    /// (and its directory's lock) goes with the engine, not after it.
    fn drop(&mut self) {
        self.signal.stop();
        self.compactor.stop();
        for t in [self.thread.take(), self.compactor_thread.take()]
            .into_iter()
            .flatten()
        {
            let _ = t.join();
        }
    }
}

/// Takes every commit after `at` out of `store` (a branch's, before its
/// engine starts): versions newer than `at`, the intents of transactions
/// committed after it and their status records, the commit times after
/// it.  What is left is snapshot `at` with its history — idempotent, so a
/// crash midway is finished by doing it again.  The writes are logged
/// under the store's last meta, so the clock stays where it was.
fn roll_back_to(store: &Store, at: u64) -> Result<()> {
    const CHUNK: usize = 4096;
    let meta = store.recovery().meta;
    let mut late = BTreeSet::new();
    {
        let tree = store.read()?;
        for e in tree.range(Bound::Included(&STATUS[..]), Bound::Excluded(&[0, 3][..]))? {
            let (k, v) = e?;
            let (Ok(id), Ok(ts)) = (
                <[u8; 8]>::try_from(&k[2..]).map(u64::from_be_bytes),
                <[u8; 8]>::try_from(&v[..]).map(u64::from_be_bytes),
            ) else {
                return Err(Error::Corrupt("bad status record"));
            };
            if ts > at {
                late.insert(id);
            }
        }
    }
    // Over a range of the store, deleting what `gone` says, a chunk at a
    // time (the tree not held while writing).
    let sweep =
        |start: &[u8], end: Option<&[u8]>, gone: &dyn Fn(&[u8], &[u8]) -> bool| -> Result<()> {
            let mut from = start.to_vec();
            let mut first = true;
            loop {
                let mut ops = Vec::new();
                let mut last = None;
                {
                    let tree = store.read()?;
                    let lo = if first {
                        Bound::Included(&from[..])
                    } else {
                        Bound::Excluded(&from[..])
                    };
                    let hi = end.map_or(Bound::Unbounded, Bound::Excluded);
                    for e in tree.range(lo, hi)?.take(CHUNK) {
                        let (k, v) = e?;
                        if gone(&k, &v) {
                            ops.push((k.clone(), None));
                        }
                        last = Some(k);
                    }
                }
                let Some(last) = last else { return Ok(()) };
                if !ops.is_empty() {
                    store.write(Batch { meta, ops })?;
                }
                from = last;
                first = false;
            }
        };
    let version_gone = |k: &[u8], _: &[u8]| match decode_key(k) {
        Some((_, ts)) => match intent_of(ts) {
            Some(id) => late.contains(&id),
            None => ts > at,
        },
        None => false,
    };
    // Versions and intents: an empty user key's, then every other's.
    sweep(&[0, 0], Some(&[0, 1]), &version_gone)?;
    sweep(&RESERVED_END, None, &version_gone)?;
    // The late transactions' status, the late commits' times.
    sweep(&STATUS, Some(&[0, 3]), &|k, _| {
        <[u8; 8]>::try_from(&k[2..]).is_ok_and(|id| late.contains(&u64::from_be_bytes(id)))
    })?;
    sweep(&COMMIT_TIMES, Some(&[0, 6]), &|k, v| {
        decode_time(k, v).is_some_and(|(ts, _)| ts > at)
    })?;
    Ok(())
}

/// What a branch may write without the WAL ([`Store::write_unlogged`])
/// before a checkpoint is due, beside `wal` bytes of WAL: an eighth of
/// them (128 MiB on 64-bit targets).  The checkpoint a commit of such
/// writes waits for (D16) then finds most of them written — by the
/// checkpointer, while the writes went on (issue #9: a large index
/// build's commit wrote its whole index then, and the load's pages
/// before it, ~half the build).
fn unlogged_limit(wal: u64) -> u64 {
    wal / 8
}

impl Inner {
    /// The one checkpoint trigger (§M14.20 W4), after every write to the
    /// store: past `checkpoint_wal_bytes` of WAL since the last checkpoint,
    /// or [`unlogged_limit`] of writes without it, the checkpointer is
    /// woken; without one — or when it falls behind, past twice the WAL's
    /// limit — the checkpoint runs here (waiting for one running).  Writes
    /// without the WAL never wait for one: a crash replays none of them,
    /// and their commit checkpoints.
    fn checkpoint_if_due(&self) -> Result<()> {
        let limit = self.opts.checkpoint_wal_bytes;
        let over = self.store.wal_since_checkpoint();
        let unlogged = self.store.unlogged_since_checkpoint() > unlogged_limit(limit);
        if over <= limit && !unlogged {
            return Ok(());
        }
        match &self.checkpointer {
            Some(c) if over <= limit.saturating_mul(2) => c.wake(),
            Some(c) => {
                if self.store.checkpoint_past(limit, unlogged_limit(limit))? {
                    c.compactor.raise();
                }
            }
            None if over <= limit => {}
            None => {
                // No thread to spare: the background work here too.
                if self.store.checkpoint_past(limit, unlogged_limit(limit))? {
                    self.store.compact(&mut || true)?;
                }
            }
        }
        Ok(())
    }
}

/// Entries vacuum changes per batch (each under the commit lock).
const VACUUM_CHUNK: usize = 16_384;
/// The threads a whole vacuum sweeps on at most ([`Engine::vacuum`]).
const SWEEPERS: usize = 8;
/// The fewest leaves of a part a whole vacuum sweeps the keyspace in.
const SWEEP_PART_LEAVES: usize = 64;

/// … and in the background ([`Engine::vacuum_paced`]): small batches, so
/// the tree and the commit lock are held only briefly.
pub const BACKGROUND_CHUNK: usize = 2048;

/// Side-area entries moved or deleted per batch.
const SIDE_CHUNK: usize = 4096;
/// Bytes spilled per batch (each applied under the tree's write lock).
const SPILL_CHUNK: usize = 1 << 20;
/// What a write costs in memory beside its key and value (about).
const WRITE_OVERHEAD: usize = 64;

/// A range of keys, both ends included.
type Span = (Vec<u8>, Vec<u8>);

/// How many ranges [`Spans`] keeps.
const SPANS: usize = 8;

/// Up to [`SPANS`] disjoint ranges, in order, covering the keys of a
/// spilled transaction's intents (§M14.20 W5): a read none of them
/// reaches skips the intents' path.  One range from the least key to the
/// greatest would cover whatever lies between — a load writes a catalog
/// key and its table's rows, and every table between them would read
/// through its intents —, so a key past the ranges starts one of its own,
/// and past [`SPANS`] the two neighbours whose facing keys share the
/// longest prefix (likeliest the same table, the same index) become one.
#[derive(Clone, Debug, Default)]
struct Spans {
    ranges: Vec<Span>,
    /// The common prefix's length of each pair of neighbours' facing keys.
    gaps: Vec<usize>,
}

impl Spans {
    /// Covers `key` too (keys in ascending order are the cheap case).
    fn add(&mut self, key: &[u8]) {
        let i = self.ranges.partition_point(|(_, hi)| hi.as_slice() < key);
        if i < self.ranges.len() && self.ranges[i].0.as_slice() <= key {
            return; // inside
        }
        if i == self.ranges.len() && i > 0 {
            // Past the last range.
            let shared = common_prefix(&self.ranges[i - 1].1, key);
            if self.ranges.len() < SPANS {
                self.ranges.push((key.to_vec(), key.to_vec()));
                self.gaps.push(shared);
                return;
            }
            if self.gaps.iter().all(|&g| g <= shared) {
                // Its gap the narrowest: the last range grows.
                self.ranges[i - 1].1 = key.to_vec();
                return;
            }
        }
        self.ranges.insert(i, (key.to_vec(), key.to_vec()));
        self.gaps = self
            .ranges
            .windows(2)
            .map(|w| common_prefix(&w[0].1, &w[1].0))
            .collect();
        if self.ranges.len() > SPANS {
            let j = (0..self.gaps.len())
                .max_by_key(|&j| self.gaps[j])
                .expect("more than one range");
            let (_, hi) = self.ranges.remove(j + 1);
            self.ranges[j].1 = hi;
            self.gaps.remove(j);
        }
    }

    /// Whether a range reaches into the keys from `start` to `end`.
    fn overlaps(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> bool {
        self.ranges.iter().any(|s| overlaps(s, start, end))
    }
}

/// How many bytes `a` and `b` begin with alike.
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// Whether `span` and the keys from `start` to `end` share a key.
fn overlaps(span: &Span, start: Bound<&[u8]>, end: Bound<&[u8]>) -> bool {
    let (lo, hi) = (&span.0[..], &span.1[..]);
    let from = match start {
        Bound::Unbounded => true,
        Bound::Included(s) => hi >= s,
        Bound::Excluded(s) => hi > s,
    };
    let to = match end {
        Bound::Unbounded => true,
        Bound::Included(e) => lo <= e,
        Bound::Excluded(e) => lo < e,
    };
    from && to
}

/// The store's entries in `[from, to)` that are versions: the reserved
/// keys (`00 01` – `00 FF`) skipped.
fn versions<'t>(
    tree: &'t Tables,
    from: Bound<&[u8]>,
    to: Bound<&[u8]>,
) -> Result<Vec<nucleus_storage::engines::Scan<'t>>> {
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

/// Writes mirrored ([`Engine::mirror`]) while it lives.
pub struct Mirror {
    engine: Arc<Inner>,
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
}

impl std::fmt::Debug for Mirror {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mirror")
            .field("pairs", &self.pairs.len())
            .finish()
    }
}

impl Drop for Mirror {
    fn drop(&mut self) {
        let _last = lock(&self.engine.commit);
        let mut mirrors = write(&self.engine.mirrors);
        let all: Vec<_> = mirrors
            .iter()
            .filter(|p| !self.pairs.contains(p))
            .cloned()
            .collect();
        self.engine.mirroring.store(all.len(), Ordering::Release);
        *mirrors = Arc::new(all);
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

/// `m` read: what every read of the side area's statuses takes (shared by
/// the workers reading at once, §M14.20 W6).
fn read<T>(m: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(|e| e.into_inner())
}

/// `m` to change.
fn write<T>(m: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(|e| e.into_inner())
}

impl Engine {
    /// Opens (or creates) the database in `dir` and recovers it.
    pub fn open(platform: Arc<dyn Platform>, dir: &Path, opts: EngineOptions) -> Result<Self> {
        let (store, recovery) = Store::open(Arc::clone(&platform), dir, &opts.store)?;
        Self::on_store(platform, store, recovery, opts, Registry::default())
    }

    /// The engine of `store`'s branch, registered in `branches`.
    fn on_store(
        platform: Arc<dyn Platform>,
        store: Store,
        recovery: Recovery,
        opts: EngineOptions,
        branches: Registry,
    ) -> Result<Self> {
        let threads = platform.parallelism();
        let id = store.info().id;
        // A branch made at a past snapshot, not yet rolled back to it (or
        // a crash interrupted that): first of all, as its tag says.
        if store.tag() != 0 {
            roll_back_to(&store, store.tag() - 1)?;
            store.set_tag(0);
            store.checkpoint()?;
        }
        let store = Arc::new(store);
        // (The simulation's single thread checkpoints where it falls due,
        // so its runs stay deterministic.)
        let checkpointer = if threads > 1 {
            Some(
                Checkpointer::start(Arc::clone(&store), opts.checkpoint_wal_bytes)
                    .map_err(nucleus_storage::Error::Io)?,
            )
        } else {
            None
        };
        let engine = Self {
            inner: Arc::new(Inner {
                store,
                checkpointer,
                opts,
                branches,
                commit: Mutex::new(recovery.meta),
                visible: AtomicU64::new(recovery.meta),
                active: Mutex::new(BTreeMap::new()),
                began: Mutex::new(std::collections::BTreeSet::new()),
                began_seq: AtomicU64::new(0),
                recent: Mutex::new(Recent::default()),
                next_id: AtomicU64::new(1),
                spilling: RwLock::new(BTreeMap::new()),
                unresolved: RwLock::new(BTreeMap::new()),
                side_users: AtomicUsize::new(0),
                begun: AtomicU64::new(0),
                sweep: Mutex::new(None),
                unlogged: Mutex::new(Vec::new()),
                watched: RwLock::new(Vec::new()),
                watched_at: AtomicU64::new(recovery.meta),
                mirrors: RwLock::new(Arc::new(Vec::new())),
                mirroring: AtomicUsize::new(0),
                platform,
                last_time: AtomicU64::new(0),
                last_lsn: AtomicU64::new(0),
                time_floor: AtomicU64::new(0),
                time_floor_found: AtomicU64::new(u64::MAX),
            }),
        };
        engine.inner.recover_side()?;
        if let Some((_, time)) = engine.inner.last_commit_time()? {
            engine.inner.last_time.store(time, Ordering::Release);
        }
        engine.inner.refresh_time_floor()?;
        // The LSM trees' compactions drop what commits drop from the
        // leaves they rewrite (§M14.28 L4d): the versions no snapshot sees
        // as of when a merge begins (the watermark only rises).
        let weak = Arc::downgrade(&engine.inner);
        engine.inner.store.set_compaction_filter(Arc::new(move || {
            let inner = weak.upgrade()?;
            let f: Box<dyn LeafFilter + Send> = Box::new(Collect {
                wm: inner.current_watermark(),
                intents: !read(&inner.unresolved).is_empty(),
            });
            Some(f)
        }));
        let mut all = lock(&engine.inner.branches);
        all.retain(|_, e| e.strong_count() > 0);
        all.insert(id, Arc::downgrade(&engine.inner));
        drop(all);
        Ok(engine)
    }

    /// This engine's branch (PLAN.md §M14.13).
    pub fn branch_info(&self) -> &BranchInfo {
        self.inner.store.info()
    }

    /// Every branch of the database, the main one first.
    pub fn branches(&self) -> Vec<BranchInfo> {
        self.inner.store.branches()
    }

    /// The engine of branch `name` (`nucleus_storage::MAIN_NAME` the
    /// main one): the one already running, else one started — its clock
    /// where the branch's last commit left it, its transactions' leftovers
    /// cleaned as after a crash.
    pub fn branch(&self, name: &str) -> Result<Engine> {
        let store = self.inner.store.branch(name)?;
        self.engine_of(store)
    }

    fn engine_of(&self, store: Store) -> Result<Engine> {
        let id = store.info().id;
        if let Some(inner) = lock(&self.inner.branches)
            .get(&id)
            .and_then(|w| w.upgrade())
        {
            return Ok(Engine { inner });
        }
        let recovery = store.recovery();
        Self::on_store(
            Arc::clone(&self.inner.platform),
            store,
            recovery,
            self.inner.opts.clone(),
            Arc::clone(&self.inner.branches),
        )
    }

    /// Makes branch `name` of this one as it is now — every commit
    /// applied so far (a checkpoint makes them durable first); constant
    /// time and space — and starts its engine: its clock starts at this
    /// one's last commit, its first commit is the next timestamp on
    /// both, and from then on neither sees the other's.  Transactions
    /// running here are not in it (their unlogged writes and intents are
    /// cleaned as after a crash).
    pub fn create_branch(&self, name: &str) -> Result<Engine> {
        let store = self.inner.store.create_branch(name)?;
        self.engine_of(store)
    }

    /// [`create_branch`](Self::create_branch) at a past snapshot `at`
    /// (PLAN.md §M14.13 B4), one the versions kept still hold
    /// ([`EngineOptions::history`], `history_time`; else
    /// [`Error::SnapshotTooOld`]): the branch is this one as it is now,
    /// then every commit after `at` taken out of it — its versions, its
    /// spilled intents and status, its time — as if never made.  That
    /// costs a pass over the branch's entries (the commits after `at`
    /// are not listed anywhere), and is redone after a crash until it is
    /// done (the branch's tag says so).
    pub fn create_branch_at(&self, name: &str, at: Timestamp) -> Result<Engine> {
        // The snapshot readable now, and kept while the branch is made.
        let guard = self.begin_with(TxnOptions {
            at: Some(at),
            read_only: true,
            ..TxnOptions::default()
        })?;
        let store = self.inner.store.create_branch_tagged(name, at.0 + 1)?;
        drop(guard);
        self.engine_of(store)
    }

    /// Drops branch `name` (not the main one, nor one with branches of its
    /// own): its engine's transactions fail from then on.
    pub fn drop_branch(&self, name: &str) -> Result<()> {
        self.inner.store.drop_branch(name)?;
        Ok(())
    }

    /// The user keys whose newest values may differ between this branch
    /// and branch `base` (PLAN.md §M14.18 O3, an incremental backup's):
    /// ranges of them, ascending and apart, from the store's tree walk
    /// ([`Store::diff_ranges`]) — what changed since base's branch point
    /// and little more.  `None` when it cannot tell from there: base is
    /// the main branch, or the two differ in what decides visibility
    /// beyond a key's own versions — spilled writes, their transactions'
    /// status, a running transaction's unlogged ranges (compare all then).
    pub fn changed_since(&self, base: &Engine) -> Result<Option<Vec<UserRange>>> {
        let Some(ranges) = self.inner.store.diff_ranges(&base.inner.store)? else {
            return Ok(None);
        };
        let (tree, base_tree) = (self.inner.store.read()?, base.inner.store.read()?);
        let decides =
            |k: &[u8]| k.starts_with(&SIDE) || k.starts_with(&STATUS) || k.starts_with(&UNLOGGED);
        let mut out: Vec<UserRange> = Vec::new();
        for (lo, hi) in ranges {
            // The reserved keys within: the same in both, or no answer.
            if lo.as_deref().is_none_or(|lo| lo < &RESERVED_END[..]) {
                let start = lo.as_deref().map_or(Bound::Unbounded, Bound::Included);
                let end = match hi.as_deref() {
                    Some(hi) if hi < &RESERVED_END[..] => Bound::Excluded(hi),
                    _ => Bound::Excluded(&RESERVED_END[..]),
                };
                let mut a = tree.range(start, end)?;
                let mut b = base_tree.range(start, end)?;
                loop {
                    let x = a
                        .by_ref()
                        .find(|e| e.as_ref().map_or(true, |(k, _)| decides(k)));
                    let y = b
                        .by_ref()
                        .find(|e| e.as_ref().map_or(true, |(k, _)| decides(k)));
                    match (x.transpose()?, y.transpose()?) {
                        (None, None) => break,
                        (Some(x), Some(y)) if x == y => {}
                        _ => return Ok(None),
                    }
                }
            }
            if hi.as_deref().is_some_and(|hi| hi <= &RESERVED_END[..]) {
                continue;
            }
            // In user keys: from the user key of the lower separator to
            // that of the upper one, both whole (a separator falls between
            // a key's versions).
            let user = |k: &[u8]| decode_key(k).map(|(u, _)| u);
            let start = match lo.as_deref() {
                Some(lo) if lo >= &RESERVED_END[..] => {
                    user(lo).map_or(Bound::Unbounded, Bound::Included)
                }
                _ => Bound::Unbounded,
            };
            let end = hi
                .as_deref()
                .and_then(user)
                .map_or(Bound::Unbounded, Bound::Included);
            match out.last_mut() {
                // Touching the last: one.
                Some((_, last_end)) if touches(last_end, &start) => {
                    if !matches!(last_end, Bound::Unbounded) {
                        *last_end = end;
                    }
                }
                _ => out.push((start, end)),
            }
        }
        Ok(Some(out))
    }

    /// Makes branch `name` (one without branches of its own) its parent as
    /// it is now; the new engine (the old one's transactions fail).
    pub fn reset_branch(&self, name: &str) -> Result<Engine> {
        let store = self.inner.store.reset_branch(name)?;
        self.engine_of(store)
    }

    /// Whether committed transactions have intents vacuum has not made
    /// versions yet.
    pub fn intents_pending(&self) -> bool {
        !read(&self.inner.unresolved).is_empty()
    }

    /// How many transactions have begun: unchanged over a while, with
    /// none [`running`](Self::running), the database was idle.
    pub fn activity(&self) -> u64 {
        self.inner.begun.load(Ordering::Relaxed)
    }

    /// Watches the keys from `start` to `end` (exclusive): from now on a
    /// commit writing one of them raises the mark
    /// [`Transaction::watched_as_at`] reads — what lets a reader keep
    /// what it derived from those keys (a catalog) across transactions.
    pub fn watch(&self, start: &[u8], end: &[u8]) {
        let last = lock(&self.inner.commit);
        let mut watched = write(&self.inner.watched);
        if !watched.iter().any(|(s, e)| s == start && e == end) {
            watched.push((start.to_vec(), end.to_vec()));
            // What was derived before is not known to be current.
            self.inner.watched_at.fetch_max(*last, Ordering::SeqCst);
        }
    }

    /// Mirrors writes (PLAN.md §M14.28 L3b): until the returned guard is
    /// dropped, a transaction writing a key under a pair's source prefix
    /// writes the same under its destination prefix too (the rest of the
    /// key kept) — a table's keys copied to another place while it is
    /// written.  Applies to writes made from now on, in transactions
    /// begun before too; what a transaction wrote before is not mirrored
    /// (the caller makes such a transaction fail, by a key it read
    /// validated).  A mirrored write is not checked for conflicts at its
    /// destination: a newer write of the source wins there as it does at
    /// the source — while a direct write at a destination (a copy's) is
    /// checked as any write is, so it loses to a mirrored one committed
    /// after it began.  No fresh range ([`Transaction::fresh`]) is taken
    /// within a source.  Refused when a prefix meets one mirrored already.
    pub fn mirror(&self, pairs: Vec<(Vec<u8>, Vec<u8>)>) -> Result<Mirror> {
        let _last = lock(&self.inner.commit);
        let mut mirrors = write(&self.inner.mirrors);
        let meets = |a: &[u8], b: &[u8]| a.starts_with(b) || b.starts_with(a);
        for (i, (s, d)) in pairs.iter().enumerate() {
            let taken = mirrors
                .iter()
                .chain(&pairs[..i])
                .flat_map(|(a, b)| [a, b])
                .any(|p| meets(p, s) || meets(p, d));
            if taken || meets(s, d) || s.is_empty() || d.is_empty() {
                return Err(Error::Storage(nucleus_storage::Error::Engine(
                    "a mirrored prefix meets another one".into(),
                )));
            }
        }
        let mut all = Vec::clone(&mirrors);
        all.extend(pairs.iter().cloned());
        self.inner.mirroring.store(all.len(), Ordering::Release);
        *mirrors = Arc::new(all);
        Ok(Mirror {
            engine: Arc::clone(&self.inner),
            pairs,
        })
    }

    /// How many transactions are running.
    pub fn running(&self) -> usize {
        lock(&self.inner.active).values().sum()
    }

    /// How long the oldest running transaction has run (`None`: none
    /// runs) — by the platform's wall clock.
    pub fn oldest_running(&self) -> Option<std::time::Duration> {
        let oldest = lock(&self.inner.began).first()?.0;
        Some(std::time::Duration::from_micros(
            self.inner.wall_micros().saturating_sub(oldest),
        ))
    }

    /// The newest snapshot a transaction would see.
    pub fn visible(&self) -> Timestamp {
        Timestamp(self.inner.visible.load(Ordering::Acquire))
    }

    /// Returns once every commit that has passed its conflict check is
    /// visible.  The visible snapshot lags the commits until they are
    /// durable, so a transaction retried at once after losing to one
    /// begins before it and loses again — for as long as the log takes to
    /// sync, which on a slow disk outlasts any short back-off.  Waiting
    /// here joins that sync (group commit) instead, and the retry sees
    /// the commit it lost to.
    pub fn settle(&self) -> Result<()> {
        let (ts, lsn) = {
            let last = lock(&self.inner.commit);
            (*last, self.inner.last_lsn.load(Ordering::Acquire))
        };
        if self.inner.visible.load(Ordering::Acquire) >= ts {
            return Ok(());
        }
        // Every commit up to `ts` logged its record at or before `lsn`
        // (under the commit lock, in order) and was applied there: durable
        // up to it, they are all visible.
        self.inner.store.wait_durable(lsn)?;
        self.inner.visible.fetch_max(ts, Ordering::AcqRel);
        Ok(())
    }

    /// The snapshot of time `time` — that of the last commit at or before
    /// it, with a retention by time ([`EngineOptions::history_time`]); the
    /// newest one for a time past the last commit.  `SnapshotTooOld` when
    /// no commit at or before it is recorded, or its versions are
    /// collected ([`begin_with`](Self::begin_with) checks the latter).
    pub fn snapshot_at(&self, time: std::time::SystemTime) -> Result<Timestamp> {
        let micros = time
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX));
        let visible = self.inner.visible.load(Ordering::Acquire);
        match self.inner.commit_at_or_before(micros)? {
            Some((ts, _)) => Ok(Timestamp(ts.min(visible))),
            None => Err(Error::SnapshotTooOld),
        }
    }

    /// The time commit `ts` was recorded at (µs since the Unix epoch), if
    /// it was — with a retention by time, and not yet past it.
    pub fn commit_time(&self, ts: Timestamp) -> Result<Option<u64>> {
        let tree = self.inner.store.read()?;
        let d = self.inner.store.deferred();
        Ok(Inner::time_from(&tree, &d, ts.0)?
            .filter(|&(at, _)| at == ts.0)
            .map(|(_, time)| time))
    }

    /// The retention by time, in µs ([`EngineOptions::history_time`]).
    pub fn history_time(&self) -> u64 {
        self.inner.opts.history_time
    }

    /// How far back a time reaches: the oldest snapshot readable whose
    /// commit time is recorded, and that time (µs since the Unix epoch);
    /// `None` without a retention by time or a commit recorded.
    pub fn reach(&self) -> Result<Option<(Timestamp, u64)>> {
        if self.inner.opts.history_time == 0 {
            return Ok(None);
        }
        self.inner.refresh_time_floor()?;
        let wm = self.inner.current_watermark();
        let tree = self.inner.store.read()?;
        let d = self.inner.store.deferred();
        Ok(Inner::time_from(&tree, &d, wm)?.map(|(ts, time)| (Timestamp(ts), time)))
    }

    /// Starts a snapshot-isolation read-write transaction.
    pub fn begin(&self) -> Transaction {
        self.begin_with(TxnOptions::default())
            .expect("a current snapshot is always available")
    }

    /// Starts a transaction.
    pub fn begin_with(&self, opts: TxnOptions) -> Result<Transaction> {
        if opts.at.is_some() {
            self.inner.refresh_time_floor()?;
        }
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
        let began = (
            self.inner.wall_micros(),
            self.inner.began_seq.fetch_add(1, Ordering::Relaxed),
        );
        lock(&self.inner.began).insert(began);
        Ok(Transaction {
            engine: Arc::clone(&self.inner),
            start,
            began,
            writes: WriteSet::default(),
            write_bytes: 0,
            spilled: None,
            span: None,
            spans: Spans::default(),
            read_only: opts.read_only || opts.at.is_some(),
            isolation: opts.isolation,
            done: false,
            reads: Mutex::new(ReadSet::default()),
            fresh: Vec::new(),
            pending: Mutex::new(Pending::default()),
            applying: Mutex::new(None),
            gap: None,
            empty: Mutex::new(Vec::new()),
            mirrors: None,
        })
    }

    /// Makes the database file current (see `Store::checkpoint`); commits
    /// go on while its slow half runs.
    pub fn checkpoint(&self) -> Result<()> {
        Ok(self.inner.store.checkpoint()?)
    }

    /// Removes every version no snapshot can see any more, makes committed
    /// transactions' intents versions at their commit timestamps and
    /// deletes aborted ones'.  Returns how many entries it removed.
    /// Commits also collect the old versions in the leaves they rewrite
    /// (all but a tombstone a leaf ends with); this sweeps the rest.  After a whole sweep, the transactions committed
    /// before it began have no intent left: their status records go.
    pub fn vacuum(&self) -> Result<u64> {
        *lock(&self.inner.sweep) = None;
        let committed = self.inner.unresolved_ids();
        let tails = Self::intent_tails(&committed);
        let workers = self.inner.platform.parallelism().clamp(1, SWEEPERS);
        // The keyspace in parts, each a run of whole user keys: a window
        // of them swept at once, one a worker (the sweep only reads), up
        // to a chunk of writes each, their writes one batch in key order
        // — the parts lie in order (issue #4: the walk was ~45 % of the
        // vacuum after TPC-H's load, on one thread).
        let mut parts = std::collections::VecDeque::from(self.sweep_parts(workers * 8)?);
        let mut removed = 0;
        while !parts.is_empty() {
            let commit = lock(&self.inner.commit);
            let wm = self.inner.current_watermark();
            let window: Vec<(Vec<u8>, Option<Vec<u8>>)> =
                parts.drain(..workers.min(parts.len())).collect();
            let sweep = |(from, to): &(Vec<u8>, Option<Vec<u8>>)| {
                let mut removed = 0;
                self.sweep_chunk(
                    from,
                    to.as_deref(),
                    Some(&committed),
                    VACUUM_CHUNK,
                    wm,
                    &mut removed,
                )
                .map(|s| (s, removed))
            };
            let swept: Vec<Result<(Swept, u64)>> = if window.len() == 1 {
                vec![sweep(&window[0])]
            } else {
                std::thread::scope(|sc| {
                    let handles: Vec<_> = window
                        .iter()
                        .map(|part| sc.spawn(move || sweep(part)))
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| {
                            h.join()
                                .unwrap_or(Err(Error::Corrupt("a sweep worker panicked")))
                        })
                        .collect()
                })
            };
            let (mut ops, mut covered) = (Vec::new(), Vec::new());
            let mut ranges = Vec::new();
            let mut again = Vec::new();
            for ((from, to), s) in window.into_iter().zip(swept) {
                let (s, r) = s?;
                removed += r;
                ops.extend(s.ops);
                covered.extend(s.covered);
                // Above the reserved keys: their ends are not versions'.
                let start = from.as_slice().max(&RESERVED_END[..]).to_vec();
                ranges.push((start, s.next.clone().or_else(|| to.clone())));
                if let Some(next) = s.next {
                    again.push((next, to));
                }
            }
            for part in again.into_iter().rev() {
                parts.push_front(part);
            }
            let done = parts.is_empty();
            if done {
                for &(id, _) in &committed {
                    ops.push((status_key(id), None));
                }
            }
            let rekeying = Rekeying {
                ranges,
                tails: tails.clone(),
            };
            if !ops.is_empty() || !tails.is_empty() {
                let lsn = self.inner.store.write_rekeying(
                    Batch { meta: *commit, ops },
                    &covered,
                    &rekeying,
                )?;
                drop(commit);
                if done {
                    self.forget_swept(&committed, lsn)?;
                }
            }
        }
        Ok(removed)
    }

    /// The committed transactions' intents' store key ends, each made its
    /// version's: `00 00 ‖ id` to `00 00 ‖ MAX - commit ts` — what a
    /// sweep rekeys ([`Store::write_rekeying`]: the leaves written over in
    /// place, not each intent deleted and its version put; issue #4).
    fn intent_tails(committed: &[(u64, u64)]) -> Tails {
        let end = |suffix: u64| {
            let mut t = vec![0, 0];
            t.extend_from_slice(&suffix.to_be_bytes());
            t
        };
        Tails::new(
            committed
                .iter()
                .map(|&(id, cts)| (end(u64::MAX - intent_ts(id)), end(u64::MAX - cts)))
                .collect(),
        )
    }

    /// The keyspace cut into about `n` parts for a sweep ([`vacuum`](Self::vacuum)):
    /// `(from, to)`, `to` exclusive (`None`: the end), each cut a user
    /// key's first version — so a key's versions are all in one part —
    /// where the B+tree's internal nodes cut its leaves evenly
    /// ([`Tables::split_keys`]: one part where another engine holds keys).
    fn sweep_parts(&self, n: usize) -> Result<Vec<(Vec<u8>, Option<Vec<u8>>)>> {
        let tree = self.inner.store.read()?;
        let cuts = if n > 1 {
            tree.split_keys(&RESERVED_END, None, n, SWEEP_PART_LEAVES)?
        } else {
            Vec::new()
        };
        let mut bounds: Vec<Vec<u8>> = Vec::with_capacity(cuts.len());
        let mut k = Vec::new();
        for cut in cuts {
            let mut range = tree.range(Bound::Included(&cut[..]), Bound::Unbounded)?;
            if !range.next_key_into(&mut k)? {
                break;
            }
            let Some((user, _)) = decode_key(&k) else {
                return Err(Error::Corrupt("undecodable version key"));
            };
            let first = mvcc::first_version(&user);
            if bounds.last().is_none_or(|b| *b < first) && first[..] > RESERVED_END[..] {
                bounds.push(first);
            }
        }
        let mut parts = Vec::with_capacity(bounds.len() + 1);
        let mut from = Vec::new();
        for b in bounds {
            parts.push((std::mem::replace(&mut from, b.clone()), Some(b)));
        }
        parts.push((from, None));
        Ok(parts)
    }

    /// After a whole sweep's last batch (`lsn`): the commit times no
    /// snapshot needs trimmed, and the swept transactions' statuses
    /// forgotten once the batch is durable — and with it every batch
    /// before (the log is in order): an intent never outlives its status.
    fn forget_swept(&self, committed: &[(u64, u64)], lsn: nucleus_storage::wal::Lsn) -> Result<()> {
        self.trim_times()?;
        self.inner.store.wait_durable(lsn)?;
        for &(id, _) in committed {
            write(&self.inner.unresolved).remove(&id);
            self.inner.side_users.fetch_sub(1, Ordering::AcqRel);
        }
        Ok(())
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
            let Swept {
                mut ops,
                covered,
                next,
            } = self.sweep_chunk(&from, None, Some(&committed), chunk, wm, &mut removed)?;
            if next.is_none() {
                // The whole keyspace swept: these transactions' intents are
                // all versions now.
                for &(id, _) in &committed {
                    ops.push((status_key(id), None));
                }
            }
            let tails = Self::intent_tails(&committed);
            if !ops.is_empty() || !tails.is_empty() {
                // Above the reserved keys: their ends are not versions'.
                let start = from.as_slice().max(&RESERVED_END[..]).to_vec();
                let rekeying = Rekeying {
                    ranges: vec![(start, next.clone())],
                    tails,
                };
                let lsn = self.inner.store.write_rekeying(
                    Batch { meta: *commit, ops },
                    &covered,
                    &rekeying,
                )?;
                drop(commit);
                if next.is_none() {
                    self.forget_swept(&committed, lsn)?;
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

    /// [`vacuum`](Self::vacuum) over the user keys from `start` to `end`
    /// (exclusive) alone: the versions there no snapshot can see removed;
    /// keys with an intent left as they are (for the whole vacuum).  For a
    /// range written as a queue — keys put, then deleted, never written
    /// again — whose scans would otherwise step over the deleted keys'
    /// tombstones until the whole vacuum runs.  What it removed.
    pub fn vacuum_range(&self, start: &[u8], end: &[u8]) -> Result<u64> {
        self.sweep_range(start, end, false)
    }

    /// Deletes the commit time records older than the oldest snapshot
    /// kept (the one at it stays: it maps the times since it), in chunks.
    fn trim_times(&self) -> Result<()> {
        loop {
            let commit = lock(&self.inner.commit);
            let wm = self.inner.current_watermark();
            let start = time_key(0);
            let end = time_key(wm);
            let ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = {
                // In the tree and deferred (a batch's delete takes both).
                let tree = self.inner.store.read()?;
                let d = self.inner.store.deferred();
                let mut keys: Vec<Vec<u8>> = tree
                    .range(Bound::Included(&start[..]), Bound::Excluded(&end[..]))?
                    .take(VACUUM_CHUNK)
                    .map(|e| e.map(|(k, _)| k))
                    .collect::<std::result::Result<_, _>>()?;
                keys.extend(
                    d.range::<[u8], _>((Bound::Included(&start[..]), Bound::Excluded(&end[..])))
                        .take(VACUUM_CHUNK)
                        .map(|(k, _)| k.clone()),
                );
                keys.sort();
                keys.dedup();
                keys.into_iter().map(|k| (k, None)).collect()
            };
            if ops.is_empty() {
                return Ok(());
            }
            self.inner.store.write(Batch { meta: *commit, ops })?;
        }
    }

    /// The oldest snapshot any transaction reads at (or the visible one,
    /// with none running): versions older than what it sees are garbage
    /// but for the history kept.
    pub fn oldest_snapshot(&self) -> Timestamp {
        let active = lock(&self.inner.active);
        let visible = self.inner.visible.load(Ordering::Acquire);
        Timestamp(
            active
                .keys()
                .next()
                .copied()
                .unwrap_or(visible)
                .min(visible),
        )
    }

    /// [`vacuum_range`](Self::vacuum_range) past the history kept
    /// ([`EngineOptions::history`]): every version no running snapshot can
    /// see goes, however recent — what erasure overrides history for
    /// (PLAN.md §M14.24 R2).  A version a running snapshot sees stays: the
    /// caller waits for [`oldest_snapshot`](Self::oldest_snapshot) to pass
    /// what it erased.  Keys with an intent are left; a whole
    /// [`vacuum`](Self::vacuum) first resolves them.  What it removed.
    pub fn purge_range(&self, start: &[u8], end: &[u8]) -> Result<u64> {
        self.sweep_range(start, end, true)
    }

    fn sweep_range(&self, start: &[u8], end: &[u8], past_history: bool) -> Result<u64> {
        let mut from = first_version(start);
        let to = first_version(end);
        let mut removed = 0;
        loop {
            let commit = lock(&self.inner.commit);
            let wm = if past_history {
                self.oldest_snapshot().0
            } else {
                self.inner.current_watermark()
            };
            let Swept { ops, covered, next } =
                self.sweep_chunk(&from, Some(&to), None, VACUUM_CHUNK, wm, &mut removed)?;
            if !ops.is_empty() {
                self.inner
                    .store
                    .write_covering(Batch { meta: *commit, ops }, &covered)?;
            }
            drop(commit);
            match next {
                Some(n) => from = n,
                None => return Ok(removed),
            }
        }
    }

    /// One chunk of a sweep (the commit lock held) from store key `from`
    /// to `to` (exclusive; `None`: the end), about `chunk` entries: the
    /// writes removing the versions no snapshot can see, the ranges they
    /// cover, and the store key the next chunk starts at (`None`: the
    /// sweep is through).  A key's versions removed are all those from the
    /// first removed on — the older ones a commit's [`Collect`] dropped
    /// without logging too, which replay brings back: the range from the
    /// first removed to the key's last version goes as one on replay
    /// ([`Store::write_covering`]), or a crash would find a version under
    /// a tombstone the sweep removed (a deleted row back).  With
    /// `committed` (the transactions committed when a whole sweep began)
    /// intents are resolved — those committed made versions, aborted ones
    /// deleted; without, a key with an intent is left whole.
    fn sweep_chunk(
        &self,
        from: &[u8],
        to: Option<&[u8]>,
        committed: Option<&[(u64, u64)]>,
        chunk: usize,
        wm: u64,
        removed: &mut u64,
    ) -> Result<Swept> {
        let others = self.inner.others(None);
        let mut ops = Vec::new();
        let mut covered = Vec::new();
        // The committed intents the store makes versions — each key's end
        // rewritten, its row left where it is ([`Store::write_rekeying`],
        // issue #4): a chunk's writes as well.
        let mut rekeys = 0;
        let mut kept_one = false;
        let mut next = None;
        let tree = self.inner.store.read()?;
        let mut pinned = false;
        // This key's removed versions' range recorded.
        let mut covering = false;
        let to = to.map_or(Bound::Unbounded, Bound::Excluded);
        // Keys compared escaped, values read only where needed — an
        // intent's, a tombstone told by its one byte — into buffers kept
        // for the whole chunk: a sweep reads every entry of the store
        // (PLAN.md §M14.33 R4: 3.8 s of a 9.6 s vacuum after TPC-H's load
        // went to an allocation or three per entry and their decoding).
        let (mut k, mut v, mut prev) = (Vec::new(), Vec::new(), Vec::new());
        let undecodable = || Error::Corrupt("undecodable version key");
        // `escaped` ‖ 00 00 ‖ the timestamp: a version's store key.
        let version_key = |escaped: &[u8], ts: u64| {
            let mut key = Vec::with_capacity(escaped.len() + mvcc::OVERHEAD);
            key.extend_from_slice(escaped);
            key.extend_from_slice(&[0, 0]);
            key.extend_from_slice(&(u64::MAX - ts).to_be_bytes());
            key
        };
        'ranges: for mut range in versions(&tree, Bound::Included(from), to)? {
            while range.next_key_into(&mut k)? {
                let n = k.len();
                if n < mvcc::OVERHEAD || k[n - 10] != 0 || k[n - 9] != 0 {
                    return Err(undecodable());
                }
                let ts = u64::MAX - u64::from_be_bytes(k[n - 8..].try_into().expect("eight bytes"));
                let escaped = &k[..n - mvcc::OVERHEAD];
                if !(prev.len() == n && prev.starts_with(escaped)) {
                    if ops.len() + rekeys >= chunk.max(1) {
                        next = Some(version_key(escaped, u64::MAX));
                        break 'ranges;
                    }
                    prev.clear();
                    prev.extend_from_slice(&k);
                    kept_one = false;
                    pinned = false;
                    covering = false;
                }
                if let Some(id) = intent_of(ts) {
                    // A key with an intent keeps its versions this sweep.
                    pinned = true;
                    let Some(committed) = committed else {
                        continue;
                    };
                    if committed.iter().any(|&(u, _)| u == id) {
                        rekeys += 1;
                        *removed += 1;
                    } else if !others.contains(&id)
                        && !read(&self.inner.unresolved).contains_key(&id)
                    {
                        ops.push((k.clone(), None)); // aborted
                        *removed += 1;
                    }
                    continue;
                }
                if ts > wm || pinned {
                    continue; // still needed by someone
                }
                let tombstone = !kept_one
                    && range.small_value_into(&mut v, 1)?
                    && decode_value(&v)
                        .ok_or(Error::Corrupt("bad version value"))?
                        .is_none();
                if kept_one || tombstone {
                    if !covering {
                        covering = true;
                        let mut end = escaped.to_vec();
                        end.extend_from_slice(&[0, 1]); // after_versions
                        covered.push((k.clone(), end));
                    }
                    ops.push((k.clone(), None));
                    *removed += 1;
                }
                kept_one = true;
            }
        }
        Ok(Swept { ops, covered, next })
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
        let by_count = oldest.saturating_sub(self.opts.history);
        if self.opts.history_time == 0 {
            return by_count;
        }
        by_count.min(self.time_floor.load(Ordering::Acquire))
    }

    /// The platform's wall clock, in µs since the Unix epoch.
    fn wall_micros(&self) -> u64 {
        self.platform
            .clock()
            .wall()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
    }

    /// The last commit time recorded: its commit and time.  (Commit
    /// times are deferred puts — logged with their commits, in the tree
    /// in bulk —, so the tree and the store's deferred entries both.)
    fn last_commit_time(&self) -> Result<Option<(u64, u64)>> {
        let tree = self.store.read()?;
        let d = self.store.deferred();
        let end = mvcc::time_key(u64::MAX);
        let in_tree = match tree
            .range_rev(
                Bound::Included(&COMMIT_TIMES[..]),
                Bound::Included(&end[..]),
            )?
            .next()
        {
            Some(e) => {
                let (k, v) = e?;
                decode_time(&k, &v)
            }
            None => None,
        };
        let deferred = d
            .range::<[u8], _>((
                Bound::Included(&COMMIT_TIMES[..]),
                Bound::Included(&end[..]),
            ))
            .next_back()
            .and_then(|(k, v)| decode_time(k, v));
        Ok(in_tree.max(deferred))
    }

    /// The first commit time record at or after commit `ts`, in the tree
    /// or deferred.
    fn time_from(tree: &Tables, d: &Deferred, ts: u64) -> Result<Option<(u64, u64)>> {
        let start = time_key(ts);
        let end = mvcc::time_key(u64::MAX);
        let in_tree = match tree
            .range(Bound::Included(&start[..]), Bound::Included(&end[..]))?
            .next()
        {
            Some(e) => {
                let (k, v) = e?;
                decode_time(&k, &v)
            }
            None => None,
        };
        let deferred = d
            .range::<[u8], _>((Bound::Included(&start[..]), Bound::Included(&end[..])))
            .next()
            .and_then(|(k, v)| decode_time(k, v));
        Ok(match (in_tree, deferred) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        })
    }

    /// The last commit recorded at or before `time` (µs): its timestamp
    /// and time — by binary search over the records, which are monotonic
    /// in both.
    fn commit_at_or_before(&self, time: u64) -> Result<Option<(u64, u64)>> {
        let tree = self.store.read()?;
        let d = self.store.deferred();
        // (No commit past the visible one is read.)
        let (mut lo, mut hi) = (0u64, self.visible.load(Ordering::Acquire));
        let mut found = None;
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            match Self::time_from(&tree, &d, mid)? {
                Some((ts, t)) if ts <= hi && t <= time => {
                    found = Some((ts, t));
                    lo = ts + 1;
                }
                // None at or after `mid` within the bounds, or one later
                // than `time` (and so every one after it).
                _ => {
                    if mid == 0 {
                        break;
                    }
                    hi = mid - 1;
                }
            }
        }
        Ok(found)
    }

    /// Finds again, if it was found more than [`TIME_FLOOR_EVERY`] ago,
    /// the oldest snapshot the retention by time keeps.  Called with no
    /// lock held (it reads the store).
    fn refresh_time_floor(&self) -> Result<()> {
        if self.opts.history_time == 0 {
            return Ok(());
        }
        let now = u64::try_from(self.platform.clock().now().as_micros()).unwrap_or(u64::MAX);
        let found = self.time_floor_found.load(Ordering::Acquire);
        if found != u64::MAX && now.saturating_sub(found) < TIME_FLOOR_EVERY {
            return Ok(());
        }
        let cutoff = self.wall_micros().saturating_sub(self.opts.history_time);
        let floor = match self.commit_at_or_before(cutoff)? {
            Some((ts, _)) => ts,
            // Every commit recorded is within the retention: the first
            // recorded is the oldest a time reaches (none recorded: none).
            None => {
                let tree = self.store.read()?;
                let d = self.store.deferred();
                match Self::time_from(&tree, &d, 0)? {
                    Some((ts, _)) => ts,
                    None => self.visible.load(Ordering::Acquire),
                }
            }
        };
        self.time_floor.fetch_max(floor, Ordering::AcqRel);
        self.time_floor_found.store(now, Ordering::Release);
        Ok(())
    }

    fn current_watermark(&self) -> u64 {
        let active = lock(&self.active);
        self.watermark(&active, self.visible.load(Ordering::Acquire))
    }

    /// The committed transactions whose writes are still in the side area,
    /// with their commit timestamps (none, without taking the lock, when
    /// no transaction uses the side area).
    fn unresolved_ids(&self) -> Vec<(u64, u64)> {
        self.unresolved_over(Bound::Unbounded, Bound::Unbounded)
    }

    /// [`unresolved_ids`](Self::unresolved_ids) with intents of keys in
    /// `[start, end]` (a span overlapping it).
    fn unresolved_over(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Vec<(u64, u64)> {
        if self.side_users.load(Ordering::Acquire) == 0 {
            return Vec::new();
        }
        read(&self.unresolved)
            .iter()
            .filter(|(_, (_, spans))| spans.as_ref().is_none_or(|s| s.overlaps(start, end)))
            .map(|(&id, &(ts, _))| (id, ts))
            .collect()
    }

    /// The running transactions with intents, but `me`.
    fn others(&self, me: Option<u64>) -> Vec<u64> {
        self.other_spans(me).into_iter().map(|(id, _)| id).collect()
    }

    /// [`others`](Self::others), with the spans of their intents' keys.
    fn other_spans(&self, me: Option<u64>) -> Vec<(u64, Spans)> {
        if self.side_users.load(Ordering::Acquire) == 0 {
            return Vec::new();
        }
        read(&self.spilling)
            .iter()
            .filter(|&(&id, _)| Some(id) != me)
            .map(|(&id, span)| (id, span.clone()))
            .collect()
    }

    /// The newest version of `user` visible at `ts` to transaction `me` (its
    /// own intents first): `Some(Some(v))` a value, `Some(None)` a
    /// tombstone, `None` no version at all.
    fn read_at(&self, user: &[u8], ts: u64, me: Option<u64>) -> Result<Option<Option<Vec<u8>>>> {
        Ok(self.look(user, ts, me, false, None)?.1)
    }

    /// [`read_at`](Self::read_at) for each of `users` (a group of up to
    /// [`nucleus_storage::btree::GROUP`]), their values (`None`: no
    /// version, or a tombstone) into `out`.  Where no intent is in sight
    /// the group's newest versions at `ts` are sought together
    /// ([`Tables::seek_group`]: their descents' misses overlap); a key
    /// whose first version there is not one `ts` sees, or lies past its
    /// leaf, is looked at alone.
    fn read_many(
        &self,
        users: &[&[u8]],
        ts: u64,
        me: Option<u64>,
        out: &mut Vec<Option<Vec<u8>>>,
    ) -> Result<()> {
        out.clear();
        let all = (Bound::Unbounded, Bound::Unbounded);
        let quiet = me.is_none()
            && self.unresolved_over(all.0, all.1).is_empty()
            && self.other_spans(None).is_empty();
        if !quiet {
            for u in users {
                out.push(self.read_at(u, ts, me)?.flatten());
            }
            return Ok(());
        }
        thread_local! {
            static GROUP: std::cell::RefCell<(Vec<Vec<u8>>, Vec<nucleus_storage::btree::Found>)> =
                const { std::cell::RefCell::new((Vec::new(), Vec::new())) };
        }
        let tree = self.store.read()?;
        GROUP.with(|g| {
            let mut fresh = Default::default();
            let mut held = g.try_borrow_mut();
            let (froms, found) = match held.as_deref_mut() {
                Ok(b) => b,
                Err(_) => &mut fresh,
            };
            froms.resize_with(users.len().max(froms.len()), Vec::new);
            for (from, user) in froms.iter_mut().zip(users) {
                from.clear();
                mvcc::escape_onto(user, from);
                from.extend_from_slice(&[0, 0]);
                from.extend_from_slice(&(u64::MAX - ts).to_be_bytes());
            }
            let keys: Vec<&[u8]> = froms[..users.len()].iter().map(Vec::as_slice).collect();
            if !tree.seek_group(&keys, found)? {
                for u in users {
                    out.push(versions_at(&tree, u, ts, false)?.1.flatten());
                }
                return Ok(());
            }
            for (j, user) in users.iter().enumerate() {
                let (from, f) = (&froms[j], &found[j]);
                // `user`'s versions start with `from` but its timestamp.
                let own = from.len() - 8;
                let mine = f.at && f.key.starts_with(&from[..own]);
                if mine && f.key.len() != from.len() {
                    return Err(Error::Corrupt("undecodable version key"));
                }
                if f.at && !mine {
                    // The first entry there is another key's: no version
                    // `ts` sees (each newer one sorts before `from`).
                    out.push(None);
                    continue;
                }
                if mine {
                    let vts = u64::MAX
                        - u64::from_be_bytes(f.key[own..].try_into().expect("eight bytes"));
                    if intent_of(vts).is_none() && vts <= ts {
                        let v =
                            decode_value(&f.value).ok_or(Error::Corrupt("bad version value"))?;
                        out.push(v.map(<[u8]>::to_vec));
                        continue;
                    }
                }
                out.push(versions_at(&tree, user, ts, false)?.1.flatten());
            }
            Ok(())
        })
    }

    /// One look at `user`'s versions for transaction `me` reading at `ts`:
    /// whether writing it conflicts — a newer commit wrote it (a version,
    /// or an intent committed after `ts`), or another running transaction
    /// has an intent of it — and the value `me` sees (its own intent, else
    /// the newest of the committed intents and versions at or below `ts`).
    /// `conflict_only`: the value is not wanted (the walk stops sooner).
    /// `gap`: when `user` has no entry at all — no version, no intent — set
    /// to the keys from it up to the tree's next entry, none of which has
    /// one either.
    #[allow(clippy::type_complexity)]
    fn look(
        &self,
        user: &[u8],
        ts: u64,
        me: Option<u64>,
        conflict_only: bool,
        gap: Option<&mut Option<Gap>>,
    ) -> Result<(bool, Option<Option<Vec<u8>>>)> {
        let at = Bound::Included(user);
        let unresolved = self.unresolved_over(at, at);
        let others: Vec<u64> = self
            .other_spans(me)
            .into_iter()
            .filter(|(_, spans)| spans.overlaps(at, at))
            .map(|(id, _)| id)
            .collect();
        let tree = self.store.read()?;
        if unresolved.is_empty() && others.is_empty() && me.is_none() && gap.is_none() {
            return versions_at(&tree, user, ts, conflict_only);
        }
        let decode = |v: &[u8]| -> Result<Option<Vec<u8>>> {
            Ok(decode_value(v)
                .ok_or(Error::Corrupt("bad version value"))?
                .map(<[u8]>::to_vec))
        };
        // With a gap wanted, the walk goes on to the entry past `user`'s.
        let to = after_versions(user);
        let end = match gap {
            Some(_) => Bound::Unbounded,
            None => Bound::Excluded(to.as_slice()),
        };
        let mut own = false; // an entry of `user` met
        let mut next = None; // the entry past them
        if unresolved.is_empty() && others.is_empty() && me.is_none() {
            // No intent anyone sees: the versions alone (an intent left by
            // an aborted transaction sorts first and is passed over).
            let from = if conflict_only || gap.is_some() {
                first_version(user)
            } else {
                encode_key(user, ts)
            };
            for entry in tree.range(Bound::Included(&from), end)? {
                let (k, v) = entry?;
                if k >= to {
                    next = Some(k);
                    break;
                }
                own = true;
                let (_, vts) = decode_key(&k).ok_or(Error::Corrupt("undecodable version key"))?;
                if intent_of(vts).is_some() || (!conflict_only && vts > ts) {
                    continue;
                }
                if conflict_only {
                    return Ok((vts > ts, None));
                }
                return Ok((false, Some(decode(&v)?)));
            }
            Gap::found(gap, own, user, next.as_deref());
            return Ok((false, None));
        }
        let mut conflict = false;
        let mut best: Option<(u64, Vec<u8>)> = None;
        let mut regular = false;
        for entry in tree.range(Bound::Included(&first_version(user)), end)? {
            let (k, v) = entry?;
            if k >= to {
                next = Some(k);
                break;
            }
            own = true;
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
        Gap::found(gap, own, user, next.as_deref());
        let value = match best {
            None => None,
            Some((_, v)) => Some(decode(&v)?),
        };
        Ok((conflict, value))
    }

    /// Whether writing `user` conflicts for a transaction that read at
    /// `start` (`me`, if it has intents): see [`Inner::look`].
    fn conflicts(&self, user: &[u8], start: u64, me: Option<u64>) -> Result<bool> {
        Ok(self.look(user, start, me, true, None)?.0)
    }

    /// [`conflicts`](Self::conflicts), answered without a look at the tree
    /// when what recent commits wrote tells: `recent` holds every key a
    /// commit after a running transaction's snapshot wrote (with the
    /// newest such commit), so a key there newer than `start` conflicts and
    /// one absent does not — unless another running transaction has
    /// intents (only the tree shows them) or a spilled commit's span covers
    /// the key (a span is coarser than its keys); then the tree decides.
    fn write_conflicts(&self, user: &[u8], start: u64, me: Option<u64>) -> Result<bool> {
        if self.others(me).is_empty() {
            let recent = lock(&self.recent);
            let since = recent.since(start);
            if since.wrote(user) {
                return Ok(true);
            }
            if !since.spanned(user) {
                return Ok(false);
            }
        }
        self.conflicts(user, start, me)
    }

    /// The transactions as the engine found them on opening: those with a
    /// status record committed — their intents read as versions at its
    /// timestamp until vacuum makes them versions —, every other one's
    /// intents aborted (passed over, and swept by vacuum); the next id
    /// past every id used.  An older engine's side area: the writes of
    /// transactions that never committed dropped, those of committed ones
    /// moved into versions.
    fn recover_side(&self) -> Result<()> {
        // Fresh ranges written without the WAL by transactions that never
        // committed (D16): what of them a checkpoint holds goes.
        loop {
            let mark = {
                let tree = self.store.read()?;
                match tree
                    .range(Bound::Included(&UNLOGGED[..]), Bound::Excluded(&[0, 5][..]))?
                    .next()
                {
                    None => None,
                    Some(e) => {
                        let (k, end) = e?;
                        Some((k[2..].to_vec(), end))
                    }
                }
            };
            let Some((start, end)) = mark else { break };
            self.drop_unlogged(&start, &end)?;
        }
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
            write(&self.unresolved).insert(id, (ts, None));
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
                self.store.write(Batch { meta: *last, ops })?;
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
            self.store.write(Batch { meta: *last, ops })?;
        }
    }

    /// Deletes every entry of the fresh range `[start, end)` written without
    /// the WAL by a transaction that did not commit, a batch at a time,
    /// then its mark (the last batch) — logged deletes: replayed after a
    /// crash, they delete what the checkpoint holds of them; a crash
    /// before the mark goes, and the next opening deletes again.  The
    /// range is then free for others.
    fn drop_unlogged(&self, start: &[u8], end: &[u8]) -> Result<()> {
        let (from, to) = (first_version(start), first_version(end));
        loop {
            let last = lock(&self.commit);
            let mut ops = Vec::new();
            {
                let tree = self.store.read()?;
                for e in versions(&tree, Bound::Included(&from), Bound::Excluded(&to))?
                    .into_iter()
                    .flatten()
                {
                    let (k, _) = e?;
                    ops.push((k, None));
                    if ops.len() == SIDE_CHUNK {
                        break;
                    }
                }
            }
            let done = ops.is_empty();
            if done {
                ops.push((unlogged_key(start), None));
            }
            self.store.write(Batch { meta: *last, ops })?;
            if done {
                lock(&self.unlogged).retain(|(s, _)| s != start);
                return Ok(());
            }
        }
    }

    /// The first key of `writes` inside a fresh range another running
    /// transaction writes without the WAL (`own`: the starts of the
    /// caller's): a conflict — its abort would delete the range whole.
    /// Called under the commit lock.
    fn unlogged_conflict(&self, writes: &WriteSet, own: &[&[u8]]) -> Option<Vec<u8>> {
        let ranges = lock(&self.unlogged);
        ranges
            .iter()
            .filter(|(s, _)| !own.contains(&s.as_slice()))
            .find_map(|(s, e)| {
                writes
                    .range(Bound::Included(s), Bound::Excluded(e))
                    .next()
                    .map(|(k, _)| k.clone())
            })
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
            self.store.write(Batch { meta: *last, ops })?;
            if done {
                write(&self.unresolved).remove(&id);
                self.side_users.fetch_sub(1, Ordering::AcqRel);
                return Ok(());
            }
        }
    }

    /// A commit's check of the keys it writes: the first a newer commit
    /// wrote — or another running transaction has an intent of — is a
    /// conflict.  Answered from what recent commits wrote
    /// ([`write_conflicts`](Self::write_conflicts)) where that tells; the
    /// rest (a spilled commit's span covers them, or another transaction
    /// has intents) by the tree, in one walk with one cursor.  Fresh keys
    /// ([`Transaction::fresh`]) are skipped.
    fn commit_conflict(
        &self,
        keys: &mut dyn Iterator<Item = (&[u8], bool)>,
        start: u64,
        me: Option<u64>,
    ) -> Result<Option<Vec<u8>>> {
        let spans = self.other_spans(me);
        let others: Vec<u64> = spans.iter().map(|&(id, _)| id).collect();
        let mut look: Vec<&[u8]> = Vec::new();
        let keys = keys.filter(|&(_, fresh)| !fresh).map(|(user, _)| user);
        {
            let recent = lock(&self.recent);
            let since = recent.since(start);
            for user in keys {
                if since.wrote(user) {
                    return Ok(Some(user.to_vec()));
                }
                // A key a span covers — a large commit's, or another's
                // running intents' — is for the tree to decide.
                let at = Bound::Included(user);
                if since.spanned(user) || spans.iter().any(|(_, s)| s.overlaps(at, at)) {
                    look.push(user);
                }
            }
        }
        if look.is_empty() {
            return Ok(None);
        }
        let unresolved = self.unresolved_ids();
        let tree = self.store.read()?;
        // The keys come in order: most seeks stay in the leaf the last one
        // ended in.
        let mut cursor = tree.cursor()?;
        let (mut k, mut v) = (Vec::new(), Vec::new());
        for user in look {
            // Intents first: another's running, or committed after the
            // snapshot, conflicts.  Then the newest version decides.
            let end = after_versions(user);
            cursor.seek_near(&first_version(user))?;
            while cursor.next_within(&mut k, &mut v, &|key| key < &end[..])? {
                let (_, vts) = decode_key(&k).ok_or(Error::Corrupt("undecodable version key"))?;
                if let Some(id) = intent_of(vts) {
                    let committed_after = unresolved.iter().any(|&(u, cts)| u == id && cts > start);
                    if Some(id) != me && (committed_after || others.contains(&id)) {
                        return Ok(Some(user.to_vec()));
                    }
                    continue;
                }
                if vts > start {
                    return Ok(Some(user.to_vec()));
                }
                break;
            }
        }
        Ok(None)
    }
}

/// What a commit's leaf rewrites drop ([`LeafFilter`]): of every key in
/// the leaves the commit writes anyway, the versions no snapshot can see
/// any more — at or below the watermark `wm`, under the newest such one
/// (or that one too, a tombstone) — as vacuum would.
///
/// A leaf may hold only part of a key's versions, and it is enough: a
/// version is garbage under any version at or below `wm` newer than it, so
/// the newer ones in an earlier leaf only make one more of them (the one
/// kept here) garbage too.  Intents do not matter: they rank above every
/// version (resolved, a key's newest), so whatever becomes of them, no
/// snapshot reads under the newest version at or below `wm`.  Only a
/// tombstone the leaf ends with stays — older versions of its key may
/// follow in the next leaf, and would read as live without it.
///
/// Not logged: after a crash, replay brings what it dropped back — unseen
/// by any snapshot still, and vacuum's to sweep.
/// A batch's writes.
type Ops = Vec<(Vec<u8>, Option<Vec<u8>>)>;

/// One chunk of a sweep ([`Engine::sweep_chunk`]).
struct Swept {
    /// The writes.
    ops: Ops,
    /// The key ranges they cover ([`Store::write_covering`]).
    covered: Vec<(Vec<u8>, Vec<u8>)>,
    /// The store key the next chunk starts at (`None`: through).
    next: Option<Vec<u8>>,
}

struct Collect {
    wm: u64,
    /// Whether committed transactions have intents vacuum has not made
    /// versions yet: a key's newest tombstone may then be all that hides
    /// one committed before it — in this leaf, or the one before.
    intents: bool,
}

impl LeafFilter for Collect {
    fn drops(&self, entries: &[LeafEntry], drop: &mut [bool]) {
        self.mark(entries, drop, true);
    }

    /// Above an LSM tree's last level (§M14.28 L4d): older versions of a
    /// key may be in runs not merged, so a key's newest tombstone at or
    /// below the watermark stays — dropped, they would read as live.
    fn drops_above(&self, entries: &[LeafEntry], drop: &mut [bool]) {
        self.mark(entries, drop, false);
    }
}

impl Collect {
    /// [`LeafFilter::drops`]; a key's newest tombstone at or below the
    /// watermark goes only when `whole` (nothing older of the key is
    /// elsewhere).
    fn mark(&self, entries: &[LeafEntry], drop: &mut [bool], whole: bool) {
        // `escape(user) 00 00`: what a key's versions share.
        fn run_of(key: &[u8]) -> Option<&[u8]> {
            let n = key.len().checked_sub(mvcc::OVERHEAD)?;
            (key[n..n + 2] == [0, 0] && !mvcc::is_reserved(key)).then(|| &key[..n + 2])
        }
        let mut i = 0;
        while i < entries.len() {
            let Some(run) = run_of(entries[i].key) else {
                i += 1;
                continue;
            };
            let mut end = i + 1;
            while end < entries.len()
                && entries[end].key.len() == run.len() + 8
                && entries[end].key.starts_with(run)
            {
                end += 1;
            }
            let mut kept = false;
            // A tombstone the newest here may go only when nothing it
            // hides can come back: no committed intent of the key unmade
            // (one sorts before its versions — maybe in the leaf before,
            // for the leaf's first key).
            let hidden = self.intents
                && (i == 0
                    || entries[i..end].iter().any(|e| {
                        let at =
                            u64::from_be_bytes(e.key[run.len()..].try_into().expect("8 bytes"));
                        mvcc::intent_of(u64::MAX - at).is_some()
                    }));
            for (j, e) in entries[..end].iter().enumerate().skip(i) {
                let at = u64::from_be_bytes(e.key[run.len()..].try_into().expect("8 bytes"));
                // An intent's timestamp is above any watermark.
                if u64::MAX - at <= self.wm {
                    let tombstone = e.inline == Some(&[0][..]);
                    if kept || (whole && tombstone && end < entries.len() && !hidden) {
                        drop[j] = true;
                    }
                    kept = true;
                }
            }
            i = end;
        }
    }
}

/// User keys a transaction found no entry for — no version, no intent
/// of anyone — in the tree as it looked after the transaction began: from
/// `from` up to `to` (excluded; `None`: past every key).  Inside it, a
/// key's own commit decides: what was committed before the snapshot was
/// in the tree then, and a write after it conflicts at commit as it
/// would have when looked at.  A bulk insert of new keys in key order
/// looks at the tree once, not once a key.
#[derive(Debug)]
struct Gap {
    from: Vec<u8>,
    to: Option<Vec<u8>>,
}

impl Gap {
    /// Sets `gap` (where wanted) to the keys from `user` to the store key
    /// `next` (`None`: the tree's end), when `user` had no entry (`own`).
    fn found(gap: Option<&mut Option<Gap>>, own: bool, user: &[u8], next: Option<&[u8]>) {
        let Some(gap) = gap else { return };
        if own {
            return;
        }
        let to = match next {
            None => None,
            // A reserved key past `user` (only the empty key sorts below
            // them): no gap.
            Some(k) => match decode_key(k) {
                Some((u, _)) if u.as_slice() > user => Some(u),
                _ => return,
            },
        };
        *gap = Some(Gap {
            from: user.to_vec(),
            to,
        });
    }

    fn holds(&self, user: &[u8]) -> bool {
        user >= self.from.as_slice() && self.to.as_deref().is_none_or(|t| user < t)
    }
}

/// A transaction.  Dropping it without committing rolls it back.
pub struct Transaction {
    engine: Arc<Inner>,
    start: u64,
    /// The writes held in memory (the newest of each key).
    writes: WriteSet,
    /// About how much memory `writes` takes.
    write_bytes: usize,
    /// The transaction's id in the side area, once its writes spilled.
    spilled: Option<u64>,
    /// The lowest and highest key spilled.
    span: Option<(Vec<u8>, Vec<u8>)>,
    /// The ranges of the keys spilled ([`Spans`]).
    spans: Spans,
    read_only: bool,
    isolation: Isolation,
    done: bool,
    /// When it began, in [`Inner::began`].
    began: (u64, u64),
    /// What was read, for validation at commit: every read of a
    /// serializable transaction, and the validated reads
    /// ([`Transaction::get_validated`], [`Transaction::scan_validated`]) of
    /// any transaction.
    reads: Mutex<ReadSet>,
    /// Key ranges declared fresh ([`Transaction::fresh`]).
    fresh: Vec<Fresh>,
    /// Writes into fresh ranges to go into the store without the WAL
    /// (D16), gathered into batches; reads write them first.
    pending: Mutex<Pending>,
    /// The batch of unlogged writes going into the store on a thread of
    /// its own while the next is gathered ([`flush_unlogged_behind`]).
    ///
    /// [`flush_unlogged_behind`]: Transaction::flush_unlogged_behind
    applying: Mutex<Option<std::thread::JoinHandle<Result<()>>>>,
    /// The last run of keys [`put_new`](Transaction::put_new) found
    /// without an entry in the tree (until this transaction's own intents
    /// enter it).
    gap: Option<Gap>,
    /// Ranges in which the store shows this transaction nothing — found by
    /// [`first`](Transaction::first), kept until its own intents enter the
    /// store (a snapshot's view of the store changes only by them); inside
    /// one, its own writes alone decide a read.  `put_new`'s [`Gap`] is
    /// the stronger fact (no entry of anyone), for writes.
    empty: Mutex<Vec<(Bound<Vec<u8>>, Bound<Vec<u8>>)>>,
    /// The mirrored prefixes its writes met ([`Engine::mirror`]), as they
    /// were at its first such write.
    mirrors: Option<Arc<Vec<(Vec<u8>, Vec<u8>)>>>,
}

/// Ranges a transaction remembers the store as empty in, at most.
const EMPTY_RANGES: usize = 8;

/// A range a transaction declared fresh.
#[derive(Debug)]
struct Fresh {
    start: Vec<u8>,
    end: Vec<u8>,
    /// How its values are written.
    mode: FreshMode,
    /// The highest key this transaction wrote in it (by either path): a
    /// key above it has no entry anywhere, so it may be written without
    /// the WAL — keys in order, as a load or an index build writes them.
    high: Option<Vec<u8>>,
}

#[derive(Debug, PartialEq)]
enum FreshMode {
    /// Not written yet (or the WAL is not minimal).
    Untried,
    /// Through the write set and the WAL: the store held an entry in it,
    /// or another transaction writes an overlapping range unlogged.
    Logged,
    /// Values above `high` go into the store without the WAL; the range
    /// is marked and registered ([`Inner::unlogged`]).
    Unlogged,
}

/// Unlogged writes not in the store yet.
#[derive(Default)]
struct Pending {
    ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    bytes: usize,
}

/// What [`Transaction::put_unlogged`] did.
enum Unlogged {
    /// Written (pending).
    Written,
    /// At or below what this transaction wrote in the unlogged range
    /// before: it may have an entry — the regular path, which sees it.
    Behind,
    /// Not an unlogged write: the regular path.
    No,
}

/// Whether the range `gap` holds the range `[start, end]` (as bounds).
fn covers(gap: &(Bound<Vec<u8>>, Bound<Vec<u8>>), start: Bound<&[u8]>, end: Bound<&[u8]>) -> bool {
    let lower = match (&gap.0, start) {
        (Bound::Unbounded, _) => true,
        (_, Bound::Unbounded) => false,
        (Bound::Included(g), Bound::Included(s) | Bound::Excluded(s)) => g.as_slice() <= s,
        (Bound::Excluded(g), Bound::Included(s)) => g.as_slice() < s,
        (Bound::Excluded(g), Bound::Excluded(s)) => g.as_slice() <= s,
    };
    let upper = match (&gap.1, end) {
        (Bound::Unbounded, _) => true,
        (_, Bound::Unbounded) => false,
        (Bound::Excluded(g), Bound::Included(e)) => e < g.as_slice(),
        (Bound::Excluded(g) | Bound::Included(g), Bound::Excluded(e)) => e <= g.as_slice(),
        (Bound::Included(g), Bound::Included(e)) => e <= g.as_slice(),
    };
    lower && upper
}

impl std::fmt::Debug for Transaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("start", &self.start)
            .field("writes", &self.writes.len())
            .finish_non_exhaustive()
    }
}

/// A serialization conflict on `key`, counted.
fn counted_conflict(key: Vec<u8>) -> Error {
    crate::metrics::CONFLICTS.inc();
    Error::Conflict { key }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if !self.done {
            // Neither committed nor rolled back: dropped.
            crate::metrics::ROLLBACKS.inc();
        }
        // Fresh ranges written without the WAL: deleted (on a failure here,
        // by the next opening, which finds their marks).
        let unlogged: Vec<_> = self
            .fresh
            .iter()
            .filter(|f| f.mode == FreshMode::Unlogged)
            .map(|f| (f.start.clone(), f.end.clone()))
            .collect();
        if !unlogged.is_empty() {
            // A batch going in finishes first (its error: the store's
            // poisoned, the next opening deletes the ranges).
            let _ = self.settle_unlogged();
            self.pending
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .ops
                .clear();
            for (start, end) in unlogged {
                if self.engine.drop_unlogged(&start, &end).is_err() {
                    // (The store is poisoned.)
                    let _last = lock(&self.engine.commit);
                    lock(&self.engine.unlogged).retain(|(s, _)| *s != start);
                }
            }
        }
        // The intents of a transaction that did not commit: aborted once it
        // stops running (no reader sees them, no writer conflicts with
        // them), then deleted (on a failure here, by vacuum).
        if let Some(id) = self.spilled.take() {
            write(&self.engine.spilling).remove(&id);
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
        drop(active);
        lock(&self.engine.began).remove(&self.began);
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

    /// Whether this transaction sees the watched keys ([`Engine::watch`])
    /// as a snapshot at `at` saw them: no commit since the later of the
    /// two wrote one, nor did this transaction.  False for a serializable
    /// transaction that writes (its reads must be recorded to validate).
    pub fn watched_as_at(&self, at: Timestamp) -> bool {
        if self.isolation == Isolation::Serializable && !self.read_only {
            return false;
        }
        self.engine.watched_at.load(Ordering::SeqCst) <= self.start.min(at.0)
            && !self.wrote_watched()
    }

    /// Whether every read of this transaction is its snapshot's alone: it
    /// wrote nothing (no write, no intent spilled, no range deleted, none
    /// declared fresh) and its reads need not be recorded (not a
    /// serializable transaction that writes) — so what is derived from a
    /// read holds for any such transaction at the same snapshot (a new
    /// snapshot starts where every commit before it is applied).
    pub fn reads_snapshot(&self) -> bool {
        !(self.isolation == Isolation::Serializable && !self.read_only)
            && self.writes.is_empty()
            && self.spilled.is_none()
            && self.spans.ranges.is_empty()
            && self.fresh.is_empty()
    }

    /// Whether this transaction wrote into a watched range.
    fn wrote_watched(&self) -> bool {
        // Nothing written: nothing to look for (and no lock taken — a
        // read asks this at each catalog lookup).
        if self.writes.is_empty() && self.spans.ranges.is_empty() && self.fresh.is_empty() {
            return false;
        }
        let watched = read(&self.engine.watched);
        watched.iter().any(|(s, e)| {
            let (start, end) = (Bound::Included(s.as_slice()), Bound::Excluded(e.as_slice()));
            self.writes.range(start, end).next().is_some()
                || self.spans.overlaps(start, end)
                || self.fresh.iter().any(|f| f.start < *e && *s < f.end)
        })
    }

    /// The value of `key` in this transaction (its own writes included).
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(w) = self.writes.get(key) {
            return Ok(w.clone());
        }
        self.flush_unlogged()?;
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

    /// [`get`](Self::get) for each of `keys` (a few: a group of up to
    /// [`nucleus_storage::btree::GROUP`] is sought together), their values
    /// into `out` in their order.
    pub fn get_many(&self, keys: &[&[u8]], out: &mut Vec<Option<Vec<u8>>>) -> Result<()> {
        if self.isolation == Isolation::Serializable
            || keys.iter().any(|k| self.writes.get(k).is_some())
        {
            out.clear();
            for k in keys {
                out.push(self.get(k)?);
            }
            return Ok(());
        }
        self.flush_unlogged()?;
        self.engine.read_many(keys, self.start, self.spilled, out)
    }

    /// [`scan`](Self::scan), validated as [`get_validated`](Self::get_validated):
    /// the commit fails if a later commit wrote inside the part of the range
    /// that was consumed.
    pub fn scan_validated(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Scan<'_>> {
        let mut scan = self.scan(start, end)?;
        scan.validated = true;
        Ok(scan)
    }

    /// Validates `[start, end)` as a [`scan_validated`](Self::scan_validated)
    /// read to its end would, without reading it: the commit fails if a
    /// later commit wrote inside the range.
    pub fn validate_range(&self, start: &[u8], end: &[u8]) {
        lock(&self.reads).ranges.push((
            Bound::Included(start.to_vec()),
            Bound::Excluded(end.to_vec()),
        ));
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
        // own: intents, once it spilled, and what it wrote without the WAL).
        if let Some(i) = self.fresh_index(key) {
            match self.put_unlogged(i, key, value)? {
                Unlogged::Written => return Ok(true),
                Unlogged::Behind => {}
                Unlogged::No if self.spilled.is_none() => {
                    self.record(key, Some(value))?;
                    return Ok(true);
                }
                Unlogged::No => {}
            }
        }
        // What the tree is looked at for has to be in it (a gap found
        // before would not know it).
        if !lock(&self.pending).ops.is_empty() {
            self.flush_unlogged()?;
            self.gap = None;
        }
        if self.isolation == Isolation::Serializable {
            lock(&self.reads).keys.insert(key.to_vec());
        }
        if self.gap.as_ref().is_some_and(|g| g.holds(key)) {
            self.record(key, Some(value))?;
            return Ok(true);
        }
        let (conflict, seen) =
            self.engine
                .look(key, self.start, self.spilled, false, Some(&mut self.gap))?;
        if seen.flatten().is_some() {
            return Ok(false);
        }
        if conflict && !self.is_fresh(key) {
            return Err(counted_conflict(key.to_vec()));
        }
        self.record(key, Some(value))?;
        Ok(true)
    }

    /// Deletes `key`.
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.write(key, None)
    }

    /// The engine routed a range meeting the user keys `[start, end)`
    /// (PLAN.md §M14.28 L3), if one is: the B+tree holds the rest.
    pub fn engine_of(&self, start: &[u8], end: &[u8]) -> Result<Option<EngineKind>> {
        let (start, end) = (mvcc::escape(start), mvcc::escape(end));
        Ok(self
            .engine
            .store
            .routes()?
            .into_iter()
            .find(|r| start < r.end && r.start < end)
            .map(|r| r.kind))
    }

    /// Routes the user keys `[start, end)` to a new engine of `kind` on
    /// every branch ([`Store::add_engine_if_free`]); false, nothing done,
    /// if a version of one of them is stored on some branch or the range
    /// meets another route.  Durable at once, whatever becomes of this
    /// transaction — a table's range given an engine the transaction that
    /// made the table then rolls back stays routed, empty, for the next
    /// table of that id.  The escaping of user keys keeps their prefixes
    /// and order, so every version of every key in the range is in it.
    pub fn add_engine(&self, start: &[u8], end: &[u8], kind: EngineKind) -> Result<bool> {
        Ok(self
            .engine
            .store
            .add_engine_if_free(&mvcc::escape(start), &mvcc::escape(end), kind)?
            .is_some())
    }

    /// Declares the keys in `[start, end)` fresh: none has a version, no
    /// other transaction writes one before this one ends, and none reads
    /// them before it commits (a new index's keys).  Their writes skip what guards against other transactions —
    /// the check for a newer version as they are written, spill and
    /// commit — which for a fresh range would find nothing.  The caller answers for the claim.
    ///
    /// With [`EngineOptions::wal_minimal`], values written into it in key
    /// order skip the WAL too (D16): they go into the store as versions at
    /// this transaction's snapshot — in a range no one else reads before
    /// the commit — and the commit checkpoints before its commit record;
    /// a rollback, or a crash before the commit, deletes the whole range.
    /// Declined (the writes logged) when the store holds an entry in the
    /// range, or another running transaction writes an overlapping one so.
    pub fn fresh(&mut self, start: &[u8], end: &[u8]) {
        // Not within a mirrored prefix: its writes go through the write
        // set, where they are mirrored.
        if self.engine.mirroring.load(Ordering::Acquire) > 0
            && read(&self.engine.mirrors).iter().any(|(from, _)| {
                start.starts_with(from) || (start <= from.as_slice() && from.as_slice() < end)
            })
        {
            return;
        }
        self.fresh.push(Fresh {
            start: start.to_vec(),
            end: end.to_vec(),
            mode: FreshMode::Untried,
            high: None,
        });
    }

    fn is_fresh(&self, key: &[u8]) -> bool {
        self.fresh_index(key).is_some()
    }

    fn fresh_index(&self, key: &[u8]) -> Option<usize> {
        self.fresh
            .iter()
            .position(|f| key >= f.start.as_slice() && key < f.end.as_slice())
    }

    /// A value for fresh range `i`, written without the WAL if it may be
    /// (the range registered on its first write).
    fn put_unlogged(&mut self, i: usize, key: &[u8], value: &[u8]) -> Result<Unlogged> {
        if !self.engine.opts.wal_minimal {
            return Ok(Unlogged::No);
        }
        if self.fresh[i].mode == FreshMode::Untried {
            self.fresh[i].mode = if self.register_unlogged(i)? {
                FreshMode::Unlogged
            } else {
                FreshMode::Logged
            };
        }
        let f = &mut self.fresh[i];
        if f.mode != FreshMode::Unlogged {
            return Ok(Unlogged::No);
        }
        if f.high.as_deref().is_some_and(|h| key <= h) {
            return Ok(Unlogged::Behind);
        }
        let high = f.high.get_or_insert_with(Vec::new);
        high.clear();
        high.extend_from_slice(key);
        // What this transaction knew of the store around the key no longer
        // holds.
        if self.gap.as_ref().is_some_and(|g| g.holds(key)) {
            self.gap = None;
        }
        let empty = self.empty.get_mut().unwrap_or_else(|e| e.into_inner());
        if !empty.is_empty() {
            empty.retain(|g| !covers(g, Bound::Included(key), Bound::Included(key)));
        }
        let pending = self.pending.get_mut().unwrap_or_else(|e| e.into_inner());
        pending.bytes += key.len() + value.len() + 16;
        pending
            .ops
            .push((encode_key(key, self.start), Some(encode_value(Some(value)))));
        if pending.bytes >= SPILL_CHUNK {
            self.flush_unlogged_behind()?;
        }
        Ok(Unlogged::Written)
    }

    /// Registers fresh range `i` to be written without the WAL: under the
    /// commit lock, unless another running transaction so writes an
    /// overlapping range or the store holds an entry in it (a commit after
    /// this snapshot's, or anyone's intent: a rollback deletes the range
    /// whole), its mark logged.  Whether it was.
    fn register_unlogged(&mut self, i: usize) -> Result<bool> {
        let engine = Arc::clone(&self.engine);
        let (start, end) = (&self.fresh[i].start, &self.fresh[i].end);
        let last = lock(&engine.commit);
        let mut ranges = lock(&engine.unlogged);
        if ranges
            .iter()
            .any(|(s, e)| s.as_slice() < end.as_slice() && start.as_slice() < e.as_slice())
        {
            return Ok(false);
        }
        {
            let tree = engine.store.read()?;
            let (from, to) = (first_version(start), first_version(end));
            for mut r in versions(&tree, Bound::Included(&from), Bound::Excluded(&to))? {
                if let Some(e) = r.next() {
                    e?;
                    return Ok(false);
                }
            }
        }
        engine.store.write(Batch {
            meta: *last,
            ops: vec![(unlogged_key(start), Some(end.clone()))],
        })?;
        ranges.push((start.clone(), end.clone()));
        Ok(true)
    }

    /// The batch of unlogged writes going into the store on its own
    /// thread, in: its error, if it failed.
    fn settle_unlogged(&self) -> Result<()> {
        match lock(&self.applying).take() {
            Some(h) => h
                .join()
                .unwrap_or(Err(Error::Corrupt("an unlogged batch's writer panicked"))),
            None => Ok(()),
        }
    }

    /// [`flush_unlogged`](Self::flush_unlogged) on a thread of its own,
    /// where there are threads: the store takes the batch while the next
    /// is gathered (the one before it waited for first: they go in in
    /// order).  Every other flush, a read's, the commit's, waits for it.
    /// Issue #9: a large index build's writes were the encoding of its
    /// entries and the tree's apply of them, one after the other.
    fn flush_unlogged_behind(&self) -> Result<()> {
        if self.engine.platform.parallelism() < 2 {
            return self.flush_unlogged();
        }
        self.settle_unlogged()?;
        let ops = {
            let mut pending = lock(&self.pending);
            pending.bytes = 0;
            std::mem::take(&mut pending.ops)
        };
        if ops.is_empty() {
            return Ok(());
        }
        let engine = Arc::clone(&self.engine);
        let h = std::thread::Builder::new()
            .name("nucleus unlogged".into())
            .spawn(move || {
                engine.store.write_unlogged(Batch { meta: 0, ops })?;
                engine.checkpoint_if_due()
            })
            .map_err(|_| Error::Corrupt("no thread for an unlogged batch"))?;
        *lock(&self.applying) = Some(h);
        Ok(())
    }

    /// Writes the pending unlogged writes into the store.
    fn flush_unlogged(&self) -> Result<()> {
        // None without a fresh range (only writes into one are pending):
        // no lock a read the workers share would contend for.
        if self.fresh.is_empty() {
            return Ok(());
        }
        self.settle_unlogged()?;
        let mut pending = lock(&self.pending);
        if pending.ops.is_empty() {
            return Ok(());
        }
        let ops = std::mem::take(&mut pending.ops);
        pending.bytes = 0;
        self.engine.store.write_unlogged(Batch { meta: 0, ops })?;
        self.engine.checkpoint_if_due()
    }

    /// The starts of the fresh ranges this transaction writes unlogged.
    fn unlogged_starts(&self) -> Vec<&[u8]> {
        self.fresh
            .iter()
            .filter(|f| f.mode == FreshMode::Unlogged)
            .map(|f| f.start.as_slice())
            .collect()
    }

    fn write(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        check_key(key)?;
        if let Some(v) = value
            && !self.fresh.is_empty()
            && let Some(i) = self.fresh_index(key)
            && let Unlogged::Written = self.put_unlogged(i, key, v)?
        {
            return Ok(());
        }
        // Fail early when a newer version exists: the commit would fail
        // anyway (first committer wins), and the client learns at the
        // statement, as PostgreSQL reports it.  The commit check stays the
        // guarantee — a conflicting commit can still land after this.
        // (A fresh key first: no one else can have written it, and that
        // test is cheaper than the write set's lookup.)
        if !self.is_fresh(key)
            && !self.writes.contains_key(key)
            && self.engine.write_conflicts(key, self.start, self.spilled)?
        {
            return Err(counted_conflict(key.to_vec()));
        }
        self.record(key, value)
    }

    /// Holds a write (checked already) — and its mirror's, under a
    /// mirrored prefix ([`Engine::mirror`]) — spilling when past the
    /// memory allowed (after both: a spill checks the pair together).
    fn record(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        self.hold(key, value);
        if self.engine.mirroring.load(Ordering::Acquire) > 0
            && let Some(to) = self.mirrored(key)
        {
            self.hold(&to, value);
        }
        if self.write_bytes > self.engine.opts.txn_memory {
            self.spill()?;
        }
        Ok(())
    }

    /// `key`'s mirror, if it is under a mirrored source prefix.
    fn mirrored(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let now = Arc::clone(&read(&self.engine.mirrors));
        let (from, to) = now.iter().find(|(from, _)| key.starts_with(from))?;
        let mirror = [to.as_slice(), &key[from.len()..]].concat();
        if !self.mirrors.as_ref().is_some_and(|m| Arc::ptr_eq(m, &now)) {
            self.mirrors = Some(now);
        }
        Some(mirror)
    }

    /// Whether `key` is a mirrored write of this transaction's — under a
    /// destination prefix, its source written too — which no conflict
    /// check looks at ([`Engine::mirror`]).
    fn is_mirror(&self, key: &[u8]) -> bool {
        self.mirrors.as_ref().is_some_and(|m| {
            m.iter().any(|(from, to)| {
                key.starts_with(to)
                    && self
                        .writes
                        .contains_key(&[from.as_slice(), &key[to.len()..]].concat())
            })
        })
    }

    /// Whether `key`'s write skips the conflict checks: fresh, or a mirror.
    fn unchecked(&self, key: &[u8]) -> bool {
        self.is_fresh(key) || (self.mirrors.is_some() && self.is_mirror(key))
    }

    /// Holds a write in the write set.
    fn hold(&mut self, key: &[u8], value: Option<&[u8]>) {
        if !self.fresh.is_empty()
            && let Some(i) = self.fresh_index(key)
        {
            let f = &mut self.fresh[i];
            if f.high.as_deref().is_none_or(|h| key > h) {
                let high = f.high.get_or_insert_with(Vec::new);
                high.clear();
                high.extend_from_slice(key);
            }
        }
        let size = key.len() + value.map_or(0, <[u8]>::len) + WRITE_OVERHEAD;
        match self.writes.insert(key, value.map(<[u8]>::to_vec)) {
            Some(old) => {
                let was = key.len() + old.map_or(0, |v| v.len()) + WRITE_OVERHEAD;
                self.write_bytes = (self.write_bytes + size).saturating_sub(was);
            }
            None => self.write_bytes += size,
        }
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
        let mut keys = self
            .writes
            .keys()
            .map(|k| (k.as_slice(), self.unchecked(k)));
        let conflict = engine.commit_conflict(&mut keys, self.start, Some(id))?;
        drop(keys);
        if let Some(key) = conflict {
            return Err(counted_conflict(key));
        }
        if let Some(key) = engine.unlogged_conflict(&self.writes, &self.unlogged_starts()) {
            return Err(counted_conflict(key));
        }
        self.gap = None; // its intents enter the tree
        lock(&self.empty).clear();
        if self.spilled.is_none() {
            engine.side_users.fetch_add(1, Ordering::AcqRel);
            self.spilled = Some(id);
        }
        if let (Some(lo), Some(hi)) = (self.writes.first_key(), self.writes.last_key()) {
            self.span = Some(match self.span.take() {
                None => (lo.clone(), hi.clone()),
                Some((a, b)) => (a.min(lo.clone()), b.max(hi.clone())),
            });
        }
        // Before its intents enter the tree (under the commit lock).
        for key in self.writes.keys() {
            self.spans.add(key);
        }
        write(&engine.spilling).insert(id, self.spans.clone());
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
                engine.store.write(Batch {
                    meta: *last,
                    ops: std::mem::take(&mut ops),
                })?;
                bytes = 0;
            }
        }
        if !ops.is_empty() {
            engine.store.write(Batch { meta: *last, ops })?;
        }
        self.write_bytes = 0;
        engine.checkpoint_if_due()
    }

    /// Up to `parts - 1` user keys inside `[start, end)` cutting it into
    /// parts of about equal size in the store, each of at least
    /// `min_leaves` of its leaves; ascending ([`nucleus_storage::BTree::split_keys`]).
    pub fn split_points(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        parts: usize,
        min_leaves: usize,
    ) -> Result<Vec<Vec<u8>>> {
        self.flush_unlogged()?;
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
        self.flush_unlogged()?;
        let store_end = match end {
            Bound::Unbounded => None,
            Bound::Included(k) => Some(after_versions(k)),
            Bound::Excluded(k) => Some(first_version(k)),
        };
        let writes = self.writes.range(start, end).peekable();
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
            count_only: false,
            escaped_keys: false,
        })
    }

    /// The first entry of `[start, end)` as [`first`](Self::first) finds
    /// it, without read-ahead — and without remembering an empty range:
    /// for the threads that share this transaction's reads, which would
    /// contend for the memory (PLAN.md §M14.20 W6).
    pub fn first_shared(
        &self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let mut scan = self.scan(start, end)?;
        scan.batch = 1;
        scan.next().transpose()
    }

    /// The first visible entry in the range, read without reading ahead.
    ///
    /// A range the store was found empty in before (and that no own intent
    /// entered since) is answered from this transaction's own writes: a
    /// load's look for "anything at or after this key" — a column table's
    /// directory and delta store, keys in ascending order — reads the store
    /// once, not once a key.  (Not under serializable isolation, whose
    /// reads are each recorded.)
    pub fn first(
        &self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let remember = self.isolation != Isolation::Serializable;
        if remember && lock(&self.empty).iter().any(|g| covers(g, start, end)) {
            return Ok(self
                .writes
                .range(start, end)
                .find_map(|(k, v)| v.as_ref().map(|v| (k.clone(), v.clone()))));
        }
        let got = self.first_shared(start, end)?;
        // The store showed nothing before the entry found (or in the whole
        // range) — known for sure where none of this transaction's own
        // writes lie, which could have hidden one of its entries.
        if remember {
            let to = match &got {
                Some((k, _)) => Bound::Excluded(k.as_slice()),
                None => end,
            };
            if self.writes.range(start, to).next().is_none() {
                let mut empty = lock(&self.empty);
                if empty.len() == EMPTY_RANGES {
                    empty.remove(0);
                }
                empty.push((start.map(<[u8]>::to_vec), to.map(<[u8]>::to_vec)));
            }
        }
        Ok(got)
    }

    /// Commits; returns the commit timestamp (the snapshot for a transaction
    /// that wrote nothing).  On [`Error::Conflict`] nothing was written.
    pub fn commit(self) -> Result<Timestamp> {
        let r = self.commit_counted();
        match r {
            Ok(_) => crate::metrics::COMMITS.inc(),
            Err(_) => crate::metrics::ROLLBACKS.inc(),
        }
        r
    }

    fn commit_counted(mut self) -> Result<Timestamp> {
        self.done = true;
        let unlogged = self.fresh.iter().any(|f| f.mode == FreshMode::Unlogged);
        if self.writes.is_empty() && self.spilled.is_none() && !unlogged {
            return Ok(Timestamp(self.start));
        }
        let engine = Arc::clone(&self.engine);
        if unlogged {
            // What was written without the WAL is durable before the
            // commit record is (D16).  Commits go on meanwhile.
            self.flush_unlogged()?;
            engine.store.checkpoint()?;
        }
        let spilled = self.spilled;
        engine.refresh_time_floor()?;
        let (ts, lsn) = {
            let mut last = lock(&engine.commit);
            let wm = engine.current_watermark();
            let mut ops = Vec::with_capacity(self.writes.len() + 1);
            // Each key's conflict (spilled keys were checked as they
            // spilled, and every commit since has checked its keys against
            // them).
            let mut keys = self
                .writes
                .keys()
                .map(|k| (k.as_slice(), self.unchecked(k)));
            let conflict = engine.commit_conflict(&mut keys, self.start, spilled)?;
            drop(keys);
            if let Some(key) = conflict {
                return Err(counted_conflict(key));
            }
            if let Some(key) = engine.unlogged_conflict(&self.writes, &self.unlogged_starts()) {
                return Err(counted_conflict(key));
            }
            // Serializable transactions record every read; others only
            // their validated ones.
            self.validate_reads(&engine)?;
            let ts = *last + 1;
            if self.wrote_watched() {
                // Before the commit is visible: a snapshot that sees it
                // sees the mark.
                engine.watched_at.fetch_max(ts, Ordering::SeqCst);
            }
            // In key order, so the store applies the batch as it is (the
            // leaves it rewrites lose what no snapshot can see any more):
            // built in it, sorted only should that fail.
            if let Some(id) = spilled {
                // The commit: this transaction's intents are versions at
                // `ts` (and become them, as vacuum rewrites them).
                ops.push((status_key(id), Some(ts.to_be_bytes().to_vec())));
            }
            // The unlogged ranges' marks go with the commit record.
            for f in self.fresh.iter().filter(|f| f.mode == FreshMode::Unlogged) {
                ops.push((unlogged_key(&f.start), None));
            }
            // Its time, with a retention by time (never before the last):
            // a deferred put — logged with the commit, into the tree with
            // thousands of others —, so a commit rewrites no leaf of times
            // (measured: 8 threads' one-row commits 28% slower with one).
            let mut deferred = Vec::new();
            let time = (engine.opts.history_time > 0).then(|| {
                let time = engine
                    .wall_micros()
                    .max(engine.last_time.load(Ordering::Acquire));
                deferred.push((time_key(ts), time.to_be_bytes().to_vec()));
                time
            });
            for (key, value) in self.writes.iter() {
                if let Some(id) = spilled
                    && self
                        .span
                        .as_ref()
                        .is_some_and(|(lo, hi)| lo <= key && key <= hi)
                {
                    // Maybe written again after it spilled: this write is
                    // the one.  (An intent sorts before the versions.)
                    ops.push((encode_key(key, intent_ts(id)), None));
                }
                ops.push((encode_key(key, ts), Some(encode_value(value.as_deref()))));
            }
            if !ops.is_sorted_by(|a, b| a.0 < b.0) {
                ops.sort_by(|a, b| a.0.cmp(&b.0));
            }
            let intents = !read(&engine.unresolved).is_empty();
            let lsn = if deferred.is_empty() {
                engine
                    .store
                    .write_filtered(Batch { meta: ts, ops }, &Collect { wm, intents })?
            } else {
                engine.store.write_deferring(
                    Batch { meta: ts, ops },
                    deferred,
                    &Collect { wm, intents },
                )?
            };
            *last = ts;
            engine.last_lsn.store(lsn, Ordering::Release);
            if let Some(time) = time {
                engine.last_time.store(time, Ordering::Release);
            }
            if unlogged {
                let mut ranges = lock(&engine.unlogged);
                for f in self
                    .fresh
                    .iter_mut()
                    .filter(|f| f.mode == FreshMode::Unlogged)
                {
                    ranges.retain(|(s, _)| *s != f.start);
                    f.mode = FreshMode::Logged; // committed: nothing to delete
                }
            }
            if let Some(id) = spilled {
                // Committed before it stops running: a writer never finds
                // its intents neither (a reader at `ts` starts after both).
                write(&engine.unresolved).insert(id, (ts, Some(std::mem::take(&mut self.spans))));
                write(&engine.spilling).remove(&id);
                self.spilled = None; // its intents are the commit's now
            }
            let mut recent = lock(&engine.recent);
            // The write set is done with: its keys move (in order).
            let keys = std::mem::take(&mut self.writes)
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            recent.add(keys, ts);
            if let Some((lo, hi)) = self.span.take() {
                recent.add_span(lo, hi, ts);
            }
            if recent.wants_prune() {
                // Nobody started before the oldest running transaction can
                // still commit, so older writes cannot conflict any more —
                // nor can a transaction begin before the visible snapshot
                // (which lags the commits until they are durable).
                let visible = engine.visible.load(Ordering::Acquire);
                let oldest = lock(&engine.active)
                    .keys()
                    .next()
                    .copied()
                    .unwrap_or(visible)
                    .min(visible);
                recent.prune(oldest);
            }
            drop(recent);
            (ts, lsn)
        };
        engine.store.wait_durable(lsn)?;
        engine.visible.fetch_max(ts, Ordering::AcqRel);
        engine.checkpoint_if_due()?;
        Ok(Timestamp(ts))
    }

    /// Read validation: no key or range in the read set was written by a
    /// commit after this transaction's snapshot.  Called under the commit
    /// lock.
    fn validate_reads(&self, engine: &Inner) -> Result<()> {
        let reads = lock(&self.reads);
        let recent = lock(&engine.recent);
        let since = recent.since(self.start);
        for k in &reads.keys {
            if since.wrote(k) || since.spanned(k) {
                return Err(counted_conflict(k.clone()));
            }
        }
        for (s, e) in &reads.ranges {
            // A span a large commit wrote, overlapping a range read.
            let below_end = |k: &[u8]| match e {
                Bound::Unbounded => true,
                Bound::Included(e) => k <= e.as_slice(),
                Bound::Excluded(e) => k < e.as_slice(),
            };
            let above_start = |k: &[u8]| match s {
                Bound::Unbounded => true,
                Bound::Included(s) => k >= s.as_slice(),
                Bound::Excluded(s) => k > s.as_slice(),
            };
            if let Some((lo, _)) = since
                .spans()
                .iter()
                .find(|(lo, hi)| below_end(lo) && above_start(hi))
            {
                return Err(counted_conflict(lo.to_vec()));
            }
        }
        for (s, e) in &reads.ranges {
            if !valid_range(s, e) {
                continue;
            }
            let bounds = (s.as_ref().map(Vec::as_slice), e.as_ref().map(Vec::as_slice));
            if let Some(key) = since.wrote_in(bounds) {
                return Err(counted_conflict(key));
            }
        }
        Ok(())
    }

    /// Discards the transaction's writes.
    pub fn rollback(mut self) {
        self.done = true;
        crate::metrics::ROLLBACKS.inc();
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
    bufs: [Vec<u8>; 3],
    writes: std::iter::Peekable<crate::writes::Range<'t>>,
    /// The requested range, and how far it was read: a serializable
    /// transaction depends on the part it consumed.
    range: (Bound<Vec<u8>>, Bound<Vec<u8>>),
    last: Option<Vec<u8>>,
    done: bool,
    /// Whether the consumed range joins the read set.
    validated: bool,
    /// The next refill's entries.
    batch: usize,
    /// Counting only ([`Scan::count_only`]): neither the store's keys
    /// decoded nor its values read, where nothing needs them.
    count_only: bool,
    /// Its chunks' keys may come escaped ([`Scan::escaped_keys`]).
    escaped_keys: bool,
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
    /// Its keys are user keys escaped, as the store keeps them — not
    /// decoded ([`Scan::escaped_keys`]; [`Chunk::user_key_into`] decodes
    /// one).
    escaped: bool,
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
        self.escaped = false;
    }

    /// Whether its keys are escaped ([`Scan::escaped_keys`]).
    #[inline]
    pub fn escaped(&self) -> bool {
        self.escaped
    }

    /// Entry `i`'s user key into `out` (replacing what it held): decoded
    /// when the chunk's keys are escaped.
    pub fn user_key_into(&self, i: usize, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        if self.escaped {
            crate::mvcc::unescape_onto(self.key(i), out)
                .ok_or(Error::Corrupt("undecodable escaped key"))
        } else {
            out.extend_from_slice(self.key(i));
            Ok(())
        }
    }

    /// `key` (a user key) as this chunk's keys are: escaped when they are.
    fn push_key_as(&mut self, key: &[u8], escaped: bool, value: &[u8]) -> Result<()> {
        if self.escaped == escaped {
            self.push(key, value);
        } else if self.escaped {
            self.push(&crate::mvcc::escape(key), value);
        } else {
            let mut user = Vec::with_capacity(key.len());
            crate::mvcc::unescape_onto(key, &mut user)
                .ok_or(Error::Corrupt("undecodable escaped key"))?;
            self.push(&user, value);
        }
        Ok(())
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
        self.refill_where(max, bytes, None, &mut 0, usize::MAX)
    }

    /// [`refill`](Self::refill) for [`escaped_keys`](Self::escaped_keys),
    /// the store read alone: each version's key escaped as the store keeps
    /// it (its last ten bytes, `0 0` and the timestamp, cut), a leaf's
    /// entries at a time under one latch ([`Tables`]' `leaf_entries`; a
    /// value not in its cell read on its own, as are the entries of
    /// engines that give one at a time).  A key's older versions are told
    /// by the entry before.
    fn refill_escaped(
        &mut self,
        tree: &Tables,
        seek: &[u8],
        max: usize,
        bytes: usize,
        mut sink: Option<&mut dyn FnMut(&[u8], &[u8]) -> bool>,
    ) -> Result<usize> {
        let txn = self.txn;
        let store_end = self.store_end.take();
        let end = match &store_end {
            Some(e) => Bound::Excluded(e.as_slice()),
            None => Bound::Unbounded,
        };
        let undecodable = || Error::Corrupt("undecodable version key");
        let user_of = |k: &[u8]| decode_key(k).map(|(u, _)| u).ok_or_else(undecodable);
        let [k, v, prev] = &mut self.bufs;
        let (buf, live) = (&mut self.buffer, &mut self.live);
        buf.escaped = sink.is_none();
        prev.clear();
        // Entries given: into the buffer (a deletion's too), or to `sink`.
        let mut given = 0;
        // Whether a version of the previous entry's user key was taken.
        let mut taken = false;
        let mut upper: Option<Vec<u8>> = None;
        let mut passed = 0;
        let stopped = std::cell::Cell::new(false);
        // Set when an entry's value is not in its cell: read on its own.
        let large = std::cell::Cell::new(false);
        // An entry: its key, the one before, its value when in its cell;
        // false: the batch ends before it (or its value is to be read).
        let mut step = |prev: &[u8], k: &[u8], value: Option<&[u8]>| -> Result<bool> {
            let n = k.len();
            if n < 10 || k[n - 10] != 0 || k[n - 9] != 0 {
                return Err(undecodable());
            }
            let ts = u64::MAX - u64::from_be_bytes(k[n - 8..].try_into().expect("eight bytes"));
            let escaped = &k[..n - 10];
            if !(prev.len() == n && same_from_end(&prev[..n - 10], escaped)) {
                taken = false;
            }
            let seen = taken;
            if !seen && given >= max {
                upper = Some(user_of(k)?);
                stopped.set(true);
                return Ok(false);
            }
            if !seen && ts > txn.start {
                // Too new: passed over (many of them end the batch, the
                // tree's lock let go).
                passed += 1;
                if passed >= PASS_LIMIT {
                    upper = Some(user_of(k)?);
                    stopped.set(true);
                    return Ok(false);
                }
            }
            if ts > txn.start || seen {
                return Ok(true); // too new, or an older version of a key taken
            }
            if buf.data.len() >= bytes {
                upper = Some(user_of(k)?);
                stopped.set(true);
                return Ok(false);
            }
            let Some(stored) = value else {
                large.set(true);
                return Ok(false);
            };
            let value = decode_value(stored).ok_or(Error::Corrupt("bad version value"))?;
            if let Some(sink) = sink.as_mut() {
                // A deletion is given to no one; a refused entry comes next.
                if let Some(value) = value {
                    if !sink(escaped, value) {
                        upper = Some(user_of(k)?);
                        stopped.set(true);
                        return Ok(false);
                    }
                    given += 1;
                }
                taken = true;
                return Ok(true);
            }
            given += 1;
            let at = buf.data.len();
            buf.data.extend_from_slice(escaped);
            buf.ents.push((at, buf.data.len()));
            buf.data.extend_from_slice(value.unwrap_or_default());
            live.push(value.is_some());
            taken = true;
            Ok(true)
        };
        let failed: std::cell::Cell<Option<Error>> = std::cell::Cell::new(None);
        let mut each = |prev: &[u8], k: &[u8], value: Option<&[u8]>| match step(prev, k, value) {
            Ok(go) => Ok(go),
            Err(e) => {
                failed.set(Some(e));
                stopped.set(true);
                Ok(false)
            }
        };
        'ranges: for mut range in versions(tree, Bound::Included(seek), end)? {
            'leaves: loop {
                let leaf = range.leaf_entries(k, prev, usize::MAX, &mut each)?;
                if stopped.get() {
                    break 'ranges;
                }
                match leaf {
                    Some(true) if !large.get() => {}
                    Some(false) => break,
                    // An entry at a time: the one whose value is not in its
                    // cell, or every one where the engine gives no leaves.
                    Some(true) | None => {
                        let one = leaf.is_some();
                        large.set(false);
                        loop {
                            std::mem::swap(k, prev);
                            if !range.next_into(k, v)? {
                                break 'leaves;
                            }
                            if !each(prev, k, Some(v))? {
                                break 'ranges;
                            }
                            if one {
                                break;
                            }
                        }
                    }
                }
            }
        }
        drop(each);
        if let Some(e) = failed.take() {
            return Err(e);
        }
        self.store_end = store_end;
        if let Some(u) = upper {
            self.from = Some(Bound::Included(u));
        }
        Ok(given)
    }

    /// [`refill_escaped`](Self::refill_escaped) where side areas are merged
    /// (`sides`, each with its rank, [`sides`](Self::sides)): a user key's
    /// versions come together, its intents first, and its entry is the
    /// highest ranked of them — an intent of a side area at the area's
    /// rank (others' passed over: running, aborted or committed later),
    /// its first version at or below the snapshot at its timestamp —, its
    /// value replaced in the batch when a better ranked one follows.  A
    /// load's spilled rows, committed and not yet vacuumed, so read as the
    /// store's are, a leaf at a time, their keys left escaped (the merge
    /// below decoded each: a sum over ten million rows so loaded 203 ms,
    /// 75 vacuumed).
    fn refill_escaped_merged(
        &mut self,
        tree: &Tables,
        seek: &[u8],
        max: usize,
        bytes: usize,
        sides: &[(u64, u64)],
    ) -> Result<()> {
        let txn = self.txn;
        let store_end = self.store_end.take();
        let end = match &store_end {
            Some(e) => Bound::Excluded(e.as_slice()),
            None => Bound::Unbounded,
        };
        let undecodable = || Error::Corrupt("undecodable version key");
        let user_of = |k: &[u8]| decode_key(k).map(|(u, _)| u).ok_or_else(undecodable);
        let rank_of = |id: u64| sides.iter().find(|&&(i, _)| i == id).map(|&(_, r)| r);
        let [k, v, prev] = &mut self.bufs;
        let (buf, live) = (&mut self.buffer, &mut self.live);
        buf.escaped = true;
        prev.clear();
        // The rank of the version the previous entry's user key has in the
        // batch (`None`: none yet), and whether its first version at or
        // below the snapshot was seen (the older ones after it pass).
        let mut best: Option<u64> = None;
        let mut versioned = false;
        let mut upper: Option<Vec<u8>> = None;
        let mut passed = 0;
        let stopped = std::cell::Cell::new(false);
        let large = std::cell::Cell::new(false);
        let mut step = |prev: &[u8], k: &[u8], value: Option<&[u8]>| -> Result<bool> {
            let n = k.len();
            if n < 10 || k[n - 10] != 0 || k[n - 9] != 0 {
                return Err(undecodable());
            }
            let ts = u64::MAX - u64::from_be_bytes(k[n - 8..].try_into().expect("eight bytes"));
            let escaped = &k[..n - 10];
            if !(prev.len() == n && same_from_end(&prev[..n - 10], escaped)) {
                best = None;
                versioned = false;
            }
            let rank = match intent_of(ts) {
                Some(id) => rank_of(id),
                None if ts <= txn.start && !versioned => {
                    versioned = true;
                    Some(ts)
                }
                None => None,
            };
            if best.is_none() && (buf.ents.len() >= max || buf.data.len() >= bytes) {
                // The batch is full: it ends at this key (its versions
                // read again from the first next time).
                upper = Some(user_of(k)?);
                stopped.set(true);
                return Ok(false);
            }
            let Some(rank) = rank else {
                if best.is_none() {
                    // Too new, or another's intent: many end the batch.
                    passed += 1;
                    if passed >= PASS_LIMIT {
                        upper = Some(user_of(k)?);
                        stopped.set(true);
                        return Ok(false);
                    }
                }
                return Ok(true);
            };
            if best.is_some_and(|b| rank <= b) {
                return Ok(true);
            }
            let Some(stored) = value else {
                // Its value read on its own: this entry again, as it was.
                if intent_of(ts).is_none() {
                    versioned = false;
                }
                large.set(true);
                return Ok(false);
            };
            let value = decode_value(stored).ok_or(Error::Corrupt("bad version value"))?;
            if best.is_none() {
                let at = buf.data.len();
                buf.data.extend_from_slice(escaped);
                buf.ents.push((at, buf.data.len()));
                live.push(value.is_some());
            } else {
                let (_, ke) = *buf.ents.last().expect("its entry");
                buf.data.truncate(ke);
                *live.last_mut().expect("its entry") = value.is_some();
            }
            buf.data.extend_from_slice(value.unwrap_or_default());
            best = Some(rank);
            Ok(true)
        };
        let failed: std::cell::Cell<Option<Error>> = std::cell::Cell::new(None);
        let mut each = |prev: &[u8], k: &[u8], value: Option<&[u8]>| match step(prev, k, value) {
            Ok(go) => Ok(go),
            Err(e) => {
                failed.set(Some(e));
                stopped.set(true);
                Ok(false)
            }
        };
        'ranges: for mut range in versions(tree, Bound::Included(seek), end)? {
            'leaves: loop {
                let leaf = range.leaf_entries(k, prev, usize::MAX, &mut each)?;
                if stopped.get() {
                    break 'ranges;
                }
                match leaf {
                    Some(true) if !large.get() => {}
                    Some(false) => break,
                    Some(true) | None => {
                        let one = leaf.is_some();
                        large.set(false);
                        loop {
                            std::mem::swap(k, prev);
                            if !range.next_into(k, v)? {
                                break 'leaves;
                            }
                            if !each(prev, k, Some(v))? {
                                break 'ranges;
                            }
                            if one {
                                break;
                            }
                        }
                    }
                }
            }
        }
        drop(each);
        if let Some(e) = failed.take() {
            return Err(e);
        }
        self.store_end = store_end;
        if let Some(u) = upper {
            self.from = Some(Bound::Included(u));
        }
        Ok(())
    }

    /// [`refill`](Self::refill) for [`count_only`](Self::count_only), the
    /// store read alone: an entry a live user key, its key and value
    /// empty.  A version's user key is its store key but the last ten
    /// bytes (`0 0` and the timestamp), escaped — compared so, decoded only
    /// where the batch ends (the next one starts there).
    fn refill_counting(&mut self, tree: &Tables, seek: &[u8], max: usize) -> Result<()> {
        let txn = self.txn;
        let store_end = self.store_end.take();
        let end = match &store_end {
            Some(e) => Bound::Excluded(e.as_slice()),
            None => Bound::Unbounded,
        };
        let undecodable = || Error::Corrupt("undecodable version key");
        let user_of = |k: &[u8]| decode_key(k).map(|(u, _)| u).ok_or_else(undecodable);
        let [k, v, prev] = &mut self.bufs;
        let (buf, live) = (&mut self.buffer, &mut self.live);
        // Whether a version of the previous entry's user key was taken:
        // the older versions after it are passed over (the entries are in
        // order, a key's versions together — compared with the entry
        // before, no key kept).
        let mut taken = false;
        let mut upper: Option<Vec<u8>> = None;
        let mut passed = 0;
        // Set when the batch ends (`upper` its first key not taken).
        let stopped = std::cell::Cell::new(false);
        // An entry: its key, and its value when that is one byte or less
        // (a tombstone's); false: the batch ends before it.
        let mut step = |prev: &[u8], k: &[u8], small: Option<&[u8]>| -> Result<bool> {
            let n = k.len();
            if n < 10 || k[n - 10] != 0 || k[n - 9] != 0 {
                return Err(undecodable());
            }
            let ts = u64::MAX - u64::from_be_bytes(k[n - 8..].try_into().expect("eight bytes"));
            let escaped = &k[..n - 10];
            let same = prev.len() == n && prev.starts_with(escaped);
            if !same {
                taken = false;
            }
            let seen = taken;
            if !seen && buf.ents.len() >= max {
                upper = Some(user_of(k)?);
                stopped.set(true);
                return Ok(false);
            }
            if ts > txn.start {
                // Too new: passed over (many of them end the batch, the
                // tree's lock let go).
                if !seen {
                    passed += 1;
                    if passed >= PASS_LIMIT {
                        upper = Some(user_of(k)?);
                        stopped.set(true);
                        return Ok(false);
                    }
                }
                return Ok(true);
            }
            if seen {
                return Ok(true); // an older version of a key taken
            }
            // Live unless its value is the one-byte tombstone.
            let alive = match small {
                None => true,
                Some(v) => decode_value(v)
                    .ok_or(Error::Corrupt("bad version value"))?
                    .is_some(),
            };
            let at = buf.data.len();
            buf.ents.push((at, at));
            live.push(alive);
            taken = true;
            Ok(true)
        };
        // The storage's entries give its errors: this one's are kept aside.
        let failed: std::cell::Cell<Option<Error>> = std::cell::Cell::new(None);
        let mut each = |prev: &[u8], k: &[u8], small: Option<&[u8]>| match step(prev, k, small) {
            Ok(go) => Ok(go),
            Err(e) => {
                failed.set(Some(e));
                stopped.set(true);
                Ok(false)
            }
        };
        'ranges: for mut range in versions(tree, Bound::Included(seek), end)? {
            // A leaf at a time where the engine gives them (PLAN.md §M14.33
            // R3b); else an entry at a time.
            loop {
                match range.leaf_entries(k, prev, 1, &mut each)? {
                    Some(more) => {
                        if stopped.get() {
                            break 'ranges;
                        }
                        if !more {
                            break;
                        }
                    }
                    None => {
                        loop {
                            std::mem::swap(k, prev);
                            if !range.next_key_into(k)? {
                                break;
                            }
                            let small = range.small_value_into(v, 1)?.then_some(&v[..]);
                            if !each(prev, k, small)? {
                                break 'ranges;
                            }
                        }
                        break;
                    }
                }
            }
        }
        drop(each);
        if let Some(e) = failed.take() {
            return Err(e);
        }
        self.store_end = store_end;
        if let Some(u) = upper {
            self.from = Some(Bound::Included(u));
        }
        Ok(())
    }

    /// [`refill`](Self::refill), passing over the user keys `want` refuses
    /// — by their keys, their values not read — and counting them in
    /// `skipped`, the batch ending once that reaches `most` (where no side
    /// area is merged; there every key is read).
    fn refill_where(
        &mut self,
        max: usize,
        bytes: usize,
        want: Option<&dyn Fn(&[u8]) -> bool>,
        skipped: &mut usize,
        most: usize,
    ) -> Result<()> {
        let Some(from) = self.from.take() else {
            return Ok(());
        };
        let txn = self.txn;
        let engine = &txn.engine;
        let sides = self.sides(&from);
        let tree = engine.store.read()?;
        let seek = seek_of(&from);
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
            if self.count_only && want.is_none() && !self.validated && self.writes.peek().is_none()
            {
                return self.refill_counting(&tree, &seek, max);
            }
            // The keys left escaped when the reader takes them so and
            // nothing compares them with user keys: a leaf at a time.
            if self.escaped_keys
                && want.is_none()
                && !self.validated
                && self.writes.peek().is_none()
            {
                return self
                    .refill_escaped(&tree, &seek, max, bytes, None)
                    .map(|_| ());
            }
            let [k, v, _] = &mut self.bufs;
            let buf = &mut self.buffer;
            let mut passed = 0;
            'ranges: for mut range in versions(&tree, Bound::Included(&seek), end)? {
                // With keys to pass over, each version's key first: its
                // value read only for the version the batch takes; else
                // both at once, as a plain scan reads.
                let keyed = want.is_some();
                while if keyed {
                    range.next_key_into(k)?
                } else {
                    range.next_into(k, v)?
                } {
                    let at = buf.data.len();
                    let ts = decode_key_onto(k, &mut buf.data)
                        .ok_or(Error::Corrupt("undecodable version key"))?;
                    let user = &buf.data[at..];
                    let seen = buf
                        .ents
                        .last()
                        .is_some_and(|&(kk, vv)| buf.data[kk..vv] == *user);
                    if !seen && buf.ents.len() >= max {
                        // The batch is full: it ends at the next key.
                        upper = Some(user.to_vec());
                        buf.data.truncate(at);
                        break 'ranges;
                    }
                    if !seen
                        && let Some(w) = want
                        && !w(user)
                    {
                        // Not wanted: passed over by its key — as many as
                        // a batch passes before the tree's lock is let go.
                        *skipped += 1;
                        passed += 1;
                        if passed >= PASS_LIMIT || *skipped >= most {
                            upper = Some(user.to_vec());
                            buf.data.truncate(at);
                            break 'ranges;
                        }
                        buf.data.truncate(at);
                        continue;
                    }
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
                    if keyed {
                        range.value_into(v)?;
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
        // The keys left escaped, as above: a leaf at a time, the intents
        // ranked in place.
        if self.escaped_keys && want.is_none() && !self.validated && self.writes.peek().is_none() {
            return self.refill_escaped_merged(&tree, &seek, max, bytes, &sides);
        }
        // Intents seen — committed ones at their commit timestamps, the
        // transaction's own above everything — against the versions: per
        // user key the highest ranked.
        let rank_of = |id: u64| sides.iter().find(|&&(i, _)| i == id).map(|&(_, r)| r);
        // A user key's versions come together (intents first).  Its bytes
        // are decoded once into the batch; its entry is pushed at the
        // first version seen and its value replaced by a better ranked one
        // (`best`); a key with none seen is taken out again.
        let [k, v, _] = &mut self.bufs;
        let (buf, live) = (&mut self.buffer, &mut self.live);
        let mut key: Option<(usize, usize)> = None;
        let mut best: Option<u64> = None;
        let mut taken = false;
        let mut passed = 0;
        'ranges: for mut range in versions(&tree, Bound::Included(&seek), end)? {
            while range.next_into(k, v)? {
                let at = buf.data.len();
                let ts = decode_key_onto(k, &mut buf.data)
                    .ok_or(Error::Corrupt("undecodable version key"))?;
                if let Some((ks, ke)) = key
                    && buf.data[ks..ke] == buf.data[at..]
                {
                    buf.data.truncate(at);
                } else {
                    let mut ks = at;
                    if let Some((last, _)) = key
                        && best.is_none()
                    {
                        // The last key had no version seen: its bytes go.
                        let n = buf.data.len() - at;
                        buf.data.copy_within(at.., last);
                        buf.data.truncate(last + n);
                        ks = last;
                    }
                    if buf.len() >= max || passed >= PASS_LIMIT {
                        upper = Some(buf.data[ks..].to_vec());
                        buf.data.truncate(ks);
                        key = None;
                        break 'ranges;
                    }
                    key = Some((ks, buf.data.len()));
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
                if best.is_none_or(|r| rank > r) {
                    let stored = decode_value(v).ok_or(Error::Corrupt("bad version value"))?;
                    let (ks, ke) = key.expect("a key is being read");
                    if best.is_none() {
                        buf.ents.push((ks, ke));
                        live.push(stored.is_some());
                    } else {
                        buf.data.truncate(ke);
                        *live.last_mut().expect("its entry") = stored.is_some();
                    }
                    buf.data.extend_from_slice(stored.unwrap_or_default());
                    best = Some(rank);
                }
            }
        }
        if let Some((ks, _)) = key
            && best.is_none()
        {
            buf.data.truncate(ks);
        }
        if let Some(u) = upper {
            self.from = Some(Bound::Included(u));
        }
        Ok(())
    }

    /// Moves forward to the first entry at or after `key` (nowhere where
    /// the scan is past it already): within what was read ahead where the
    /// key is there, else the store read again from `key` at its next
    /// refill — a descent of the tree, as a lookup's, not the entries in
    /// between.  The transaction's own writes before `key` are passed
    /// over.  What a merge of sorted keys with the store asks: rows found
    /// by an index read in key order (PLAN.md §M14.21 T5b).
    pub fn seek(&mut self, key: &[u8]) {
        while self.writes.peek().is_some_and(|(k, _)| k.as_slice() < key) {
            self.writes.next();
        }
        let len = self.buffer.len();
        if self.pos < len && *self.buffer.key(len - 1) >= *key {
            let (mut lo, mut hi) = (self.pos, len);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if *self.buffer.key(mid) < *key {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            self.pos = lo;
            return;
        }
        self.pos = len;
        if let Some(from) = &self.from {
            let ahead = match from {
                Bound::Included(u) | Bound::Excluded(u) => u.as_slice() >= key,
                Bound::Unbounded => false,
            };
            if !ahead {
                self.from = Some(Bound::Included(key.to_vec()));
            }
        }
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

    /// From here on only how many entries there are is wanted (a
    /// `count(*)`): where the store is read alone — no side area to merge,
    /// none of the transaction's own writes in the range, its reads not
    /// validated — each live entry comes with an empty key and value: a
    /// version's user key is not decoded (a key's versions are told apart
    /// by their escaped form, decoded only where a batch ends), and it is
    /// told live or deleted without its value read — a tombstone is one
    /// byte in its cell, so a value compressed or in overflow pages is not
    /// one.  Elsewhere entries come whole, as before.
    pub fn count_only(&mut self) {
        self.count_only = true;
    }

    /// From here on its chunks' keys may come escaped, as the store keeps
    /// them, not decoded ([`Chunk::escaped`], [`Chunk::user_key_into`]):
    /// for a reader that seldom needs them (a row's out-of-line values are
    /// found by its key) — where the store is read alone, nothing compares
    /// them with user keys (PLAN.md §M14.33 R4).
    pub fn escaped_keys(&mut self) {
        self.escaped_keys = true;
    }

    /// The side areas a batch from `from` merges, each with its rank: a
    /// committed one's commit timestamp (a version at or below the
    /// snapshot), the transaction's own above everything.
    fn sides(&self, from: &Bound<Vec<u8>>) -> Vec<(u64, u64)> {
        let txn = self.txn;
        let mut sides: Vec<(u64, u64)> = txn
            .engine
            .unresolved_over(
                from.as_ref().map(Vec::as_slice),
                self.range.1.as_ref().map(Vec::as_slice),
            )
            .into_iter()
            .filter(|&(_, ts)| ts <= txn.start)
            .collect();
        if let Some(id) = txn.spilled {
            sides.push((id, u64::MAX));
        }
        sides
    }

    /// The next entries — up to `entries` — handed to `each` where they
    /// lie, none copied: the key escaped ([`escaped_keys`](Self::escaped_keys)),
    /// the value in its page (its tree's leaf held meanwhile — `each` reads
    /// nothing else from the store).  Only where a chunk would be read so:
    /// keys escaped, the store read alone (no side area, none of the
    /// transaction's own writes, its reads not validated), nothing read
    /// ahead.  `each` returns false to stop before an entry, which then
    /// comes next.  `None`: not so — [`next_chunk`](Self::next_chunk)
    /// reads on (and says where the scan ends); `Some(0)`: nothing given
    /// (an entry refused, versions too new, or the end — likewise).
    pub fn visit(
        &mut self,
        entries: usize,
        each: &mut dyn FnMut(&[u8], &[u8]) -> bool,
    ) -> Result<Option<usize>> {
        if !self.escaped_keys
            || self.validated
            || self.count_only
            || self.writes.peek().is_some()
            || self.pos != self.buffer.len()
        {
            return Ok(None);
        }
        let Some(from) = self.from.take() else {
            return Ok(None);
        };
        if !self.sides(&from).is_empty() {
            self.from = Some(from);
            return Ok(None);
        }
        let tree = self.txn.engine.store.read()?;
        let seek = seek_of(&from);
        self.buffer.clear();
        self.live.clear();
        self.pos = 0;
        self.refill_escaped(&tree, &seek, entries.max(1), usize::MAX, Some(each))
            .map(Some)
    }

    /// Replaces `out`'s entries with the next ones — at most `entries` of
    /// them, and no more once they take `bytes` — through buffers reused
    /// from chunk to chunk; false at the end.
    pub fn next_chunk(&mut self, out: &mut Chunk, entries: usize, bytes: usize) -> Result<bool> {
        self.next_chunk_where(out, entries, bytes, None, &mut 0, usize::MAX)
    }

    /// [`next_chunk`](Self::next_chunk) of the entries whose keys `want`
    /// takes — the others passed over by their keys where the store is read
    /// without side areas (their values not read), counted in `skipped`;
    /// elsewhere they may come too.  Once `most` are passed over the chunk
    /// ends, empty perhaps, with more to come.
    pub fn next_chunk_where(
        &mut self,
        out: &mut Chunk,
        entries: usize,
        bytes: usize,
        want: Option<&dyn Fn(&[u8]) -> bool>,
        skipped: &mut usize,
        most: usize,
    ) -> Result<bool> {
        out.clear();
        // None of the transaction's own writes left to merge, the read-ahead
        // used up: the store's next entries are the chunk — read into the
        // buffer, which becomes `out` as it is.
        while self.writes.peek().is_none() && self.pos == self.buffer.len() && self.from.is_some() {
            if *skipped >= most {
                return Ok(true);
            }
            self.refill_where(entries.max(1), bytes, want, skipped, most)?;
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
                    if out.is_empty() {
                        out.escaped = self.buffer.escaped;
                    }
                    let (k, v) = (self.buffer.key(i), self.buffer.value(i));
                    out.push_key_as(k, self.buffer.escaped, v)?;
                }
                Some(Next::Write(k, v)) => out.push_key_as(k, false, v)?,
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

/// Where a batch from `from` starts in the store: a user key's first
/// version, or past its versions.
fn seek_of(from: &Bound<Vec<u8>>) -> Vec<u8> {
    match from {
        Bound::Unbounded => Vec::new(),
        Bound::Included(k) => first_version(k),
        Bound::Excluded(k) => after_versions(k),
    }
}

impl Iterator for Scan<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        let next = match self.advance() {
            Ok(Some(Next::Store(i))) => {
                // An entry at a time: its user key, decoded if escaped.
                let mut k = Vec::new();
                match self.buffer.user_key_into(i, &mut k) {
                    Ok(()) => Ok(Some((k, self.buffer.value(i)))),
                    Err(e) => Err(e),
                }
            }
            Ok(Some(Next::Write(k, v))) => Ok(Some((k.to_vec(), v))),
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        };
        match next {
            Ok(Some((k, v))) => {
                let v = v.to_vec();
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

/// [`versions_at`]'s answer from the first entry at or after `from` (the
/// version key of `user` at the snapshot, `own` its bytes before the
/// timestamp) — `None` where that entry does not settle it (an intent, a
/// version newer than `ts`, an entry past its leaf, a tree with routes):
/// the versions are then walked.  Its buffers a thread keeps.
#[allow(clippy::type_complexity)]
fn one(
    tree: &Tables,
    from: &[u8],
    own: usize,
    ts: u64,
    conflict_only: bool,
) -> Result<Option<(bool, Option<Option<Vec<u8>>>)>> {
    thread_local! {
        static FOUND: std::cell::RefCell<nucleus_storage::btree::Found> =
            std::cell::RefCell::new(Default::default());
    }
    FOUND.with(|f| {
        let mut fresh = Default::default();
        let mut held = f.try_borrow_mut();
        let found = match held.as_deref_mut() {
            Ok(f) => f,
            Err(_) => &mut fresh, // a look inside a look: its own
        };
        if !tree.seek_one(from, found)? || !found.at {
            return Ok(None);
        }
        let key = &found.key;
        if key.len() < own || key[..own] != from[..own] {
            // Another key's entry: no version of `user` from `from` on.
            return Ok(Some((false, None)));
        }
        if key.len() != own + 8 {
            return Err(Error::Corrupt("undecodable version key"));
        }
        let vts = u64::MAX - u64::from_be_bytes(key[own..].try_into().expect("eight bytes"));
        if intent_of(vts).is_some() || (!conflict_only && vts > ts) {
            return Ok(None);
        }
        if conflict_only {
            return Ok(Some((vts > ts, None)));
        }
        let v = decode_value(&found.value).ok_or(Error::Corrupt("bad version value"))?;
        Ok(Some((false, Some(v.map(<[u8]>::to_vec)))))
    })
}

/// [`Inner::look`] where no intent is in sight and no gap is wanted — every
/// read of a key by its primary key outside a writing transaction: the
/// newest version of `user` at or below `ts` (`conflict_only`: whether a
/// version is newer than `ts`), its key bounds built in buffers a thread
/// keeps and the walk's keys read into one, so a look allocates only what
/// it returns.  A version's key is `user` escaped, `00 00` and its
/// timestamp: whatever starts so is one, and its timestamp is its last
/// eight bytes.
fn versions_at(
    tree: &Tables,
    user: &[u8],
    ts: u64,
    conflict_only: bool,
) -> Result<(bool, Option<Option<Vec<u8>>>)> {
    thread_local! {
        static BUFS: std::cell::RefCell<[Vec<u8>; 3]> = const {
            std::cell::RefCell::new([Vec::new(), Vec::new(), Vec::new()])
        };
    }
    BUFS.with(|bufs| {
        let mut fresh = Default::default();
        let mut held = bufs.try_borrow_mut();
        let [from, to, key] = match held.as_deref_mut() {
            Ok(b) => b,
            Err(_) => &mut fresh, // a look inside a look: its own buffers
        };
        from.clear();
        mvcc::escape_onto(user, from);
        let own = from.len() + 2;
        to.clear();
        to.extend_from_slice(from);
        to.extend_from_slice(&[0, 1]);
        let start = if conflict_only { u64::MAX } else { ts };
        from.extend_from_slice(&[0, 0]);
        from.extend_from_slice(&(u64::MAX - start).to_be_bytes());
        // The first entry from `from` alone, where it is the answer: no
        // range made (its cursor and buffers were most of a read's
        // allocations).
        if let Some(found) = one(tree, from, own, ts, conflict_only)? {
            return Ok(found);
        }
        let mut scan = tree.range(Bound::Included(from), Bound::Excluded(to))?;
        while scan.next_key_into(key)? {
            if key.len() != own + 8 || key[..own] != from[..own] {
                return Err(Error::Corrupt("undecodable version key"));
            }
            let vts = u64::MAX - u64::from_be_bytes(key[own..].try_into().expect("eight bytes"));
            if intent_of(vts).is_some() || (!conflict_only && vts > ts) {
                continue;
            }
            if conflict_only {
                return Ok((vts > ts, None));
            }
            // The value read into `key`'s buffer: the key is done with.
            scan.value_into(key)?;
            let v = decode_value(key).ok_or(Error::Corrupt("bad version value"))?;
            return Ok((false, Some(v.map(<[u8]>::to_vec))));
        }
        Ok((false, None))
    })
}

/// Whether `a` and `b` are the same bytes, compared from the end eight at a
/// time: a scan's neighbouring keys differ last (a table's rows by their
/// keys), where `memcmp` from the start reads the prefix they share first.
#[inline]
fn same_from_end(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut n = a.len();
    while n >= 8 {
        let word = |s: &[u8]| u64::from_ne_bytes(s[n - 8..n].try_into().expect("eight bytes"));
        if word(a) != word(b) {
            return false;
        }
        n -= 8;
    }
    a[..n] == b[..n]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_keep_apart_what_lies_apart() {
        fn at(k: &[u8]) -> (Bound<&[u8]>, Bound<&[u8]>) {
            (Bound::Included(k), Bound::Included(k))
        }
        // A load: a catalog key, its rows, its index (other tables between).
        let mut s = Spans::default();
        s.add(b"c\x01next");
        let row = |i: u32| [&b"t7\x00"[..], &i.to_be_bytes()].concat();
        let entry = |i: u32| [&b"t7\x01"[..], &(i * 7).to_be_bytes()].concat();
        for i in 0..1000 {
            s.add(&row(i));
        }
        for i in 0..1000 {
            s.add(&entry(i));
        }
        // The joins kept the widest gaps: catalog | rows | index.
        assert!(s.ranges.len() <= SPANS);
        for k in [&b"c\x01next"[..], &row(0), &row(999), &entry(500)] {
            let (lo, hi) = at(k);
            assert!(s.overlaps(lo, hi));
        }
        for k in [&b"t3\x00a"[..], b"c\x02", b"t8", b"a"] {
            let (lo, hi) = at(k);
            assert!(!s.overlaps(lo, hi), "{k:?}");
        }
        assert!(s.overlaps(
            Bound::Excluded(b"t3"),
            Bound::Excluded(b"t7\x00\x00\x00\x00\x01")
        ));
        assert!(!s.overlaps(Bound::Excluded(b"c\x01next"), Bound::Excluded(&row(0))));
        // Twenty groups, in any order: at most eight ranges, every key
        // covered, neighbours with the longest shared prefix joined.
        let mut s = Spans::default();
        let mut rng = 7u64;
        let mut keys = Vec::new();
        for _ in 0..2000 {
            rng = rng.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let g = (rng >> 40) % 20;
            let key = format!("g{g:02}/{}", rng >> 50).into_bytes();
            s.add(&key);
            keys.push(key);
            assert!(s.ranges.len() <= SPANS);
            assert!(s.ranges.windows(2).all(|w| w[0].1 < w[1].0));
            assert!(s.ranges.iter().all(|(lo, hi)| lo <= hi));
        }
        for k in &keys {
            let (lo, hi) = at(k);
            assert!(s.overlaps(lo, hi));
        }
    }

    /// Which of `leaf` (keys and stored values) [`Collect`] drops.
    fn dropped(wm: u64, leaf: &[(Vec<u8>, Vec<u8>)]) -> Vec<bool> {
        dropped_with(wm, false, leaf)
    }

    fn dropped_with(wm: u64, intents: bool, leaf: &[(Vec<u8>, Vec<u8>)]) -> Vec<bool> {
        let entries: Vec<_> = leaf
            .iter()
            .map(|(k, v)| LeafEntry {
                key: k,
                inline: Some(v),
            })
            .collect();
        let mut drop = vec![false; leaf.len()];
        Collect { wm, intents }.drops(&entries, &mut drop);
        drop
    }

    fn version(user: &[u8], ts: u64, value: Option<&[u8]>) -> (Vec<u8>, Vec<u8>) {
        (encode_key(user, ts), encode_value(value))
    }

    #[test]
    fn collect_drops_what_no_snapshot_sees() {
        let leaf = [
            version(b"a", 9, Some(b"a9")),
            version(b"a", 5, Some(b"a5")),
            version(b"a", 3, Some(b"a3")), // under the newest at or below 5
            version(b"b", 4, None),        // a tombstone no snapshot needs
            version(b"b", 2, Some(b"b2")),
            version(b"c", 1, Some(b"c1")),
        ];
        assert_eq!(dropped(5, &leaf), [false, false, true, true, true, false]);
        // Nothing at or below the watermark but the newest: nothing goes.
        assert_eq!(dropped(0, &leaf), [false; 6]);
    }

    #[test]
    fn collect_keeps_a_tombstone_the_leaf_ends_with() {
        let leaf = [
            version(b"a", 3, Some(b"a3")),
            version(b"b", 4, None),
            version(b"b", 2, Some(b"b2")),
        ];
        // `b`'s versions may go on in the next leaf: its tombstone stays,
        // what is under it here goes.
        assert_eq!(dropped(9, &leaf), [false, false, true]);
        assert_eq!(dropped(9, &leaf[..2]), [false, false]);
        // A leaf that begins inside `a`'s versions (newer ones before it):
        // the first here at or below the watermark stays, which is one
        // more than needed, never one too few.
        let leaf = [
            version(b"a", 3, Some(b"a3")),
            version(b"a", 1, None),
            version(b"a", 0, Some(b"a0")),
            version(b"b", 1, Some(b"b1")),
        ];
        assert_eq!(dropped(9, &leaf), [false, true, true, false]);
    }

    #[test]
    fn collect_keeps_a_tombstone_over_an_unmade_intent() {
        // `a` written by a large commit (its intent, committed at 2, not
        // yet a version), then deleted at 4: the tombstone hides it.
        let leaf = [
            version(b"0", 1, Some(b"x")),
            version(b"a", intent_ts(7), Some(b"big")),
            version(b"a", 4, None),
            version(b"a", 1, Some(b"a1")),
            version(b"b", 3, None),
            version(b"b", 1, Some(b"b1")),
            version(b"c", 1, Some(b"c1")),
        ];
        assert_eq!(
            dropped_with(9, true, &leaf),
            [false, false, false, true, true, true, false]
        );
        // `b` has no intent: its tombstone goes.  A leaf's first key may
        // have one in the leaf before: its tombstone stays.
        let first = [
            version(b"a", 4, None),
            version(b"a", 1, Some(b"a1")),
            version(b"b", 3, None),
            version(b"b", 1, Some(b"b1")),
            version(b"c", 1, Some(b"c1")),
        ];
        assert_eq!(
            dropped_with(9, true, &first),
            [false, true, true, true, false]
        );
        // With no intent pending, as before.
        assert_eq!(dropped(9, &first), [true, true, true, true, false]);
        assert_eq!(
            dropped(9, &leaf),
            [false, false, true, true, true, true, false]
        );
    }

    #[test]
    fn collect_skips_intents_and_reserved_keys() {
        let leaf = [
            (status_key(7), 3u64.to_be_bytes().to_vec()),
            (NEXT_ID.to_vec(), 8u64.to_be_bytes().to_vec()),
            version(b"a", intent_ts(7), Some(b"mine")),
            version(b"a", 5, Some(b"a5")),
            version(b"a", 3, Some(b"a3")),
            version(b"b\x00", 5, Some(b"b5")),
            version(b"b\x00", 3, Some(b"b3")),
            version(b"b\x00\x00", 3, Some(b"c3")),
            version(b"b\x00\x00", 2, Some(b"c2")),
            version(b"d", 1, Some(b"d1")),
        ];
        assert_eq!(
            dropped(9, &leaf),
            [
                false, false, false, false, true, false, true, false, true, false
            ]
        );
    }
}
