//! The store: a copy-on-write B+tree made durable by a write-ahead log.
//!
//! # Writes
//!
//! A write is a [`Batch`] of puts and deletes, applied atomically.
//! [`Store::write`] appends the batch to the WAL and applies it to the tree
//! in memory; [`Store::wait_durable`] waits for the WAL `fsync` (group
//! commit).  Writers are serialised; readers run alongside, against the tree
//! under a read lock.
//!
//! # Checkpoints
//!
//! The tree's pages reach the data file whenever the buffer pool evicts
//! them, but the file's committed state changes only at a
//! [`Store::checkpoint`]: the tree root, the LSN up to which the WAL has
//! been applied, and a caller-defined meta value are committed together
//! with the pages, atomically (copy-on-write; see the pager).  WAL segments
//! wholly before that LSN are then deleted.
//!
//! # Recovery
//!
//! [`Store::open`] opens the data file at its last checkpoint and replays
//! every WAL record after it.  Replaying is applying the same batches again
//! on top of the checkpointed tree, so the result is the state after the
//! last durable batch — never a batch in part, never one that was not
//! durable.
//!
//! # Deferred entries
//!
//! [`Store::write_deferring`] logs some puts with a batch but leaves them
//! out of the tree: they wait in memory ([`Store::deferred`]) and go into
//! the tree together — at the next checkpoint, or once
//! [`DEFERRED_MAX`] wait — so a stream of small batches that each add
//! one entry to the same far-off leaf (the transaction layer's commit
//! times) rewrites that leaf once for thousands of them.  Replay puts them
//! back in memory.  A later batch's put or delete of the same key
//! replaces them.  A memtable for one key range: RocksDB's, LevelDB's.
//!
//! A write or checkpoint that fails part-way **poisons** the store: memory
//! and disk may disagree, so every later call fails until it is reopened.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard};

use nucleus_platform::{FileSystem, Platform};

use crate::btree::{BTree, LeafFilter};
use crate::buffer::BufferPool;
use crate::error::{Error, Result};
use crate::node::MAX_KEY;
use crate::page::PageId;
use crate::pager::{FIRST_DATA_PAGE, Frozen, NO_BRANCH, Pager, Space};
use crate::wal::{Lsn, Wal};

const META_ROOT: usize = 0;
const META_CHECKPOINT_LSN: usize = 1;
const META_USER: usize = 2;
/// The branch directory's root (PLAN.md §M14.13): 0 until a branch is
/// made.
const META_SYSTEM: usize = 3;
const BATCH_VERSION: u8 = 1;
/// A batch with covered ranges ([`Store::write_covering`]): version 1's
/// layout, then the ranges.
const BATCH_VERSION_COVERING: u8 = 2;
/// A batch with deferred puts ([`Store::write_deferring`]): version 2's
/// layout (its ranges maybe none), then the deferred puts.
const BATCH_VERSION_DEFERRING: u8 = 3;
/// Deferred entries kept in memory at most: past it they go into the tree
/// with the batch that brought them there.
pub const DEFERRED_MAX: usize = 4096;

/// Puts logged but not yet in the tree, by key.
pub type Deferred = BTreeMap<Vec<u8>, Vec<u8>>;

/// Tuning for a [`Store`].
#[derive(Clone, Debug)]
pub struct StoreOptions {
    /// Buffer pool frames (8 KiB each).
    pub cache_pages: usize,
    /// WAL segment size in bytes.
    pub wal_segment_size: u64,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            cache_pages: 1024,
            wal_segment_size: 16 << 20,
        }
    }
}

/// What [`Store::scrub`] overwrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scrubbed {
    /// Free pages zeroed.
    pub pages: u64,
    /// Bytes of dropped WAL segments and spares zeroed.
    pub wal_bytes: u64,
}

/// An atomic set of writes, with a caller-defined `meta` value carried
/// through the WAL (the transaction layer stores its commit timestamp
/// there).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Batch {
    /// Caller-defined; returned by recovery for the last batch replayed.
    pub meta: u64,
    /// Puts (`Some(value)`) and deletes (`None`), applied in order.
    pub ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl Batch {
    fn encode(&self) -> Vec<u8> {
        self.encode_covering(&[])
    }

    fn encode_covering(&self, covered: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
        self.encode_full(covered, &[])
    }

    fn encode_full(
        &self,
        covered: &[(Vec<u8>, Vec<u8>)],
        deferred: &[(Vec<u8>, Vec<u8>)],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(if !deferred.is_empty() {
            BATCH_VERSION_DEFERRING
        } else if covered.is_empty() {
            BATCH_VERSION
        } else {
            BATCH_VERSION_COVERING
        });
        out.extend_from_slice(&self.meta.to_le_bytes());
        out.extend_from_slice(&(self.ops.len() as u32).to_le_bytes());
        for (k, v) in &self.ops {
            out.push(u8::from(v.is_none()));
            out.extend_from_slice(&(k.len() as u32).to_le_bytes());
            out.extend_from_slice(k);
            if let Some(v) = v {
                out.extend_from_slice(&(v.len() as u32).to_le_bytes());
                out.extend_from_slice(v);
            }
        }
        if !covered.is_empty() || !deferred.is_empty() {
            out.extend_from_slice(&(covered.len() as u32).to_le_bytes());
            for (start, end) in covered {
                for k in [start, end] {
                    out.extend_from_slice(&(k.len() as u32).to_le_bytes());
                    out.extend_from_slice(k);
                }
            }
        }
        if !deferred.is_empty() {
            out.extend_from_slice(&(deferred.len() as u32).to_le_bytes());
            for (k, v) in deferred {
                for b in [k, v] {
                    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
                    out.extend_from_slice(b);
                }
            }
        }
        out
    }

    #[cfg(test)]
    fn decode(b: &[u8]) -> Result<Self> {
        Ok(Self::decode_full(b)?.0)
    }

    #[cfg(test)]
    fn decode_covering(b: &[u8]) -> Result<(Self, Vec<(Vec<u8>, Vec<u8>)>)> {
        let (batch, covered, _) = Self::decode_full(b)?;
        Ok((batch, covered))
    }

    /// A batch, its covered ranges (none before version 2) and its
    /// deferred puts (none before version 3).
    #[allow(clippy::type_complexity)]
    fn decode_full(
        mut b: &[u8],
    ) -> Result<(Self, Vec<(Vec<u8>, Vec<u8>)>, Vec<(Vec<u8>, Vec<u8>)>)> {
        fn take<'a>(b: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
            if b.len() < n {
                return Err(Error::Corrupt {
                    page: PageId(0),
                    reason: "WAL batch truncated",
                });
            }
            let (head, rest) = b.split_at(n);
            *b = rest;
            Ok(head)
        }
        let u32_of = |s: &[u8]| u32::from_le_bytes(s.try_into().unwrap()) as usize;
        let version = take(&mut b, 1)?[0];
        if !matches!(
            version,
            BATCH_VERSION | BATCH_VERSION_COVERING | BATCH_VERSION_DEFERRING
        ) {
            return Err(Error::Corrupt {
                page: PageId(0),
                reason: "unknown WAL batch version",
            });
        }
        let meta = u64::from_le_bytes(take(&mut b, 8)?.try_into().unwrap());
        let n = u32_of(take(&mut b, 4)?);
        let mut ops = Vec::with_capacity(n.min(1 << 16));
        for _ in 0..n {
            let delete = take(&mut b, 1)?[0] == 1;
            let klen = u32_of(take(&mut b, 4)?);
            let key = take(&mut b, klen)?.to_vec();
            let val = if delete {
                None
            } else {
                let vlen = u32_of(take(&mut b, 4)?);
                Some(take(&mut b, vlen)?.to_vec())
            };
            ops.push((key, val));
        }
        let bytes = |b: &mut &[u8]| -> Result<Vec<u8>> {
            let n = u32_of(take(b, 4)?);
            Ok(take(b, n)?.to_vec())
        };
        let mut covered = Vec::new();
        if version >= BATCH_VERSION_COVERING {
            let n = u32_of(take(&mut b, 4)?);
            for _ in 0..n {
                let start = bytes(&mut b)?;
                let end = bytes(&mut b)?;
                covered.push((start, end));
            }
        }
        let mut deferred = Vec::new();
        if version == BATCH_VERSION_DEFERRING {
            let n = u32_of(take(&mut b, 4)?);
            for _ in 0..n.min(1 << 20) {
                let k = bytes(&mut b)?;
                let v = bytes(&mut b)?;
                deferred.push((k, v));
            }
        }
        Ok((Self { meta, ops }, covered, deferred))
    }
}

/// What [`Store::open`] recovered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recovery {
    /// The meta value of the last batch replayed, or the checkpoint's.
    pub meta: u64,
    /// Batches replayed from the WAL.
    pub replayed: u64,
}

/// The main branch's id: the one [`Store::open`] returns.
pub const MAIN: u32 = 0;

/// The main branch's name.
pub const MAIN_NAME: &str = "main";

/// A branch (PLAN.md §M14.13) as the branch directory records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchInfo {
    /// Its id, never reused.
    pub id: u32,
    /// Its name ([`MAIN_NAME`] for the main branch).
    pub name: String,
    /// The branch it was made from (`None`: the main branch).
    pub parent: Option<u32>,
    /// The commit (the pager's sequence number) it was made at.
    pub origin: Option<u64>,
}

/// A branch's record in the directory: who it is, where its state is.
struct Record {
    info: BranchInfo,
    root: u64,
    lsn: Lsn,
    meta: u64,
}

impl Record {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(36 + self.info.name.len());
        out.extend_from_slice(&self.info.parent.unwrap_or(MAIN).to_le_bytes());
        out.extend_from_slice(&self.info.origin.unwrap_or(0).to_le_bytes());
        for v in [self.root, self.lsn, self.meta] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(self.info.name.as_bytes());
        out
    }

    fn decode(key: &[u8], b: &[u8]) -> Result<Self> {
        let corrupt = || Error::Corrupt {
            page: PageId(0),
            reason: "bad branch record",
        };
        if key.len() != 5 || b.len() < 36 {
            return Err(corrupt());
        }
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        Ok(Self {
            info: BranchInfo {
                id: u32::from_be_bytes(key[1..5].try_into().unwrap()),
                name: String::from_utf8(b[36..].to_vec()).map_err(|_| corrupt())?,
                parent: Some(u32::from_le_bytes(b[..4].try_into().unwrap())),
                origin: Some(u64_at(4)),
            },
            root: u64_at(12),
            lsn: u64_at(20),
            meta: u64_at(28),
        })
    }
}

// The branch directory's keys: a branch's record, its name, a page on a
// dead list (the branch that freed it, the branch point, the page; its
// birth the value), the next id.
fn branch_key(id: u32) -> Vec<u8> {
    let mut k = vec![b'b'];
    k.extend_from_slice(&id.to_be_bytes());
    k
}

fn name_key(name: &str) -> Vec<u8> {
    let mut k = vec![b'n'];
    k.extend_from_slice(name.as_bytes());
    k
}

fn dead_key(branch: u32, point: u64, page: u64) -> Vec<u8> {
    let mut k = dead_prefix(branch, point);
    k.extend_from_slice(&page.to_be_bytes());
    k
}

fn dead_prefix(branch: u32, point: u64) -> Vec<u8> {
    let mut k = vec![b'd'];
    k.extend_from_slice(&branch.to_be_bytes());
    k.extend_from_slice(&point.to_be_bytes());
    k
}

const NEXT_ID_KEY: &[u8] = b"i";

/// Whether `name` may name a branch: 1–63 ASCII letters, digits, `-`,
/// `_` or `.` (it names a database in a URL), not the main branch's.
pub fn valid_branch_name(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        && name != MAIN_NAME
}

/// What every branch shares: the file, the cache, the branch directory,
/// the checkpoints.
struct Shared {
    platform: Arc<dyn Platform>,
    dir: PathBuf,
    opts: StoreOptions,
    pool: Arc<BufferPool>,
    /// The branch directory (meta slot [`META_SYSTEM`]); none until the
    /// first branch is made.
    system: Mutex<Option<BTree>>,
    /// Every branch, open while the store is.
    branches: Mutex<BTreeMap<u32, Arc<Branch>>>,
    /// One checkpoint (or branch made or dropped) at a time.
    checkpointing: Mutex<()>,
    poisoned: AtomicBool,
    /// A [`Store::scrub`] runs: checkpoints overwrite the WAL segments
    /// they drop; the bytes they zeroed.
    scrubbing: AtomicBool,
    wal_zeroed: AtomicU64,
    /// The exclusive lock on the directory (`LOCK`).
    _lock: Box<dyn nucleus_platform::Lock>,
}

impl Shared {
    fn check(&self) -> Result<()> {
        if self.poisoned.load(Ordering::Acquire) {
            Err(Error::Poisoned)
        } else {
            Ok(())
        }
    }

    fn poison(&self) {
        self.poisoned.store(true, Ordering::Release);
    }

    fn branches(&self) -> MutexGuard<'_, BTreeMap<u32, Arc<Branch>>> {
        self.branches.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn by_name(&self, name: &str) -> Result<Arc<Branch>> {
        self.branches()
            .values()
            .find(|b| b.info.name == name)
            .cloned()
            .ok_or_else(|| Error::Branch(format!("branch \"{name}\" does not exist")))
    }
}

/// One branch: its tree, its log, its writer.
struct Branch {
    info: BranchInfo,
    tree: RwLock<BTree>,
    /// Writers waiting for the tree: new readers let them go first (a
    /// while), so a stream of scans does not keep a writer out.
    writers_waiting: std::sync::atomic::AtomicUsize,
    wal: Wal,
    /// Deferred puts ([`Store::write_deferring`]): changed only with the
    /// tree locked for writing, so a reader holding [`Store::read`]'s
    /// guard sees the two agree.
    deferred: Mutex<Deferred>,
    /// Serialises writers (and a checkpoint's freeze); holds the applied
    /// LSN and meta.
    writer: Mutex<(Lsn, u64)>,
    checkpoint_lsn: AtomicU64,
    recovery: Recovery,
    /// Dropped ([`Store::drop_branch`]): its pages may be another's now.
    dropped: AtomicBool,
}

impl Branch {
    fn writer(&self) -> MutexGuard<'_, (Lsn, u64)> {
        self.writer.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A durable, crash-safe ordered key-value store: one branch of the
/// database in a directory (PLAN.md §M14.13) — [`Store::open`] gives the
/// main one, [`Store::branch`] and [`Store::create_branch`] the others.
/// Every branch's tree lives in the one data file, sharing the pages it
/// has not changed; each has its own WAL (`wal/` the main branch's,
/// `branches/<id>/wal/` another's), and a checkpoint commits them all.
pub struct Store {
    shared: Arc<Shared>,
    b: Arc<Branch>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("branch", &self.b.info.name)
            .field("wal", &self.b.wal)
            .finish_non_exhaustive()
    }
}

/// Creates `dir` (and its parents) durably if it does not exist.
fn ensure_dir(fs: &dyn FileSystem, dir: &Path) -> Result<()> {
    if fs.exists(dir)? {
        return Ok(());
    }
    // The levels missing, innermost first.  Only their parents are synced
    // (a new entry is durable once its directory is): not every ancestor,
    // which may not be ours to open — an Android app may not read
    // `/data/user`.
    let mut missing = Vec::new();
    let mut level = dir;
    while !fs.exists(level)? {
        missing.push(level);
        match level.parent() {
            Some(p) if !p.as_os_str().is_empty() => level = p,
            _ => break,
        }
    }
    fs.create_dir_all(dir)?;
    // Outermost first: each new level's entry, then the levels in it.
    for new in missing.into_iter().rev() {
        if let Some(parent) = new.parent()
            && !parent.as_os_str().is_empty()
        {
            fs.sync_dir(parent)?;
        }
    }
    Ok(())
}

/// A branch's WAL directory.
fn branch_wal(dir: &Path, id: u32) -> PathBuf {
    if id == MAIN {
        dir.join("wal")
    } else {
        dir.join("branches").join(id.to_string()).join("wal")
    }
}

/// Deletes the files of a WAL directory, if there is one (a dropped
/// branch's; the directory stays, empty).
fn remove_wal_files(fs: &dyn FileSystem, wal_dir: &Path) -> Result<()> {
    if !fs.exists(wal_dir)? {
        return Ok(());
    }
    let names = fs.list_dir(wal_dir)?;
    if names.is_empty() {
        return Ok(());
    }
    for name in names {
        fs.remove_file(&wal_dir.join(name))?;
    }
    fs.sync_dir(wal_dir)?;
    Ok(())
}

/// The branch directory's tree, made if there is none yet.
fn system_tree<'t>(sys: &'t mut Option<BTree>, pool: &Arc<BufferPool>) -> Result<&'t mut BTree> {
    if sys.is_none() {
        let mut t = BTree::create(Arc::clone(pool))?;
        t.set_branch(NO_BRANCH);
        *sys = Some(t);
    }
    Ok(sys.as_mut().expect("made"))
}

/// The records of the branches other than the main one.
fn records(sys: &BTree) -> Result<Vec<Record>> {
    sys.range(Bound::Included(b"b"), Bound::Excluded(b"c"))?
        .map(|e| {
            let (k, v) = e?;
            Record::decode(&k, &v)
        })
        .collect()
}

/// What a free means in each branch ([`Pager::set_space`]), from the
/// branches' records.
fn spaces(records: &[Record]) -> BTreeMap<u32, Space> {
    let mut m: BTreeMap<u32, Space> = BTreeMap::new();
    for r in records {
        m.entry(r.info.id).or_default().origin = r.info.origin;
        if let (Some(p), Some(o)) = (r.info.parent, r.info.origin) {
            m.entry(p).or_default().points.push(o);
        }
    }
    for sp in m.values_mut() {
        sp.points.sort_unstable();
    }
    m
}

/// A checkpoint's change to the branches.
enum Op<'n> {
    Nothing,
    Create { parent: u32, name: &'n str },
    Drop { id: u32 },
    Reset { id: u32 },
}

/// A branch's state as a checkpoint freezes it: root, LSN, meta.
type Frozen1 = (Arc<Branch>, u64, Lsn, u64);

/// Opens branch `info`'s tree at `root` (a new tree if 0) and its WAL in
/// `wal_dir`, replaying what follows `checkpoint`.
#[allow(clippy::too_many_arguments)]
fn open_branch(
    platform: &Arc<dyn Platform>,
    pool: &Arc<BufferPool>,
    opts: &StoreOptions,
    info: BranchInfo,
    root: u64,
    wal_dir: &Path,
    checkpoint: Lsn,
    meta: u64,
) -> Result<Branch> {
    ensure_dir(platform.fs(), wal_dir)?;
    let mut tree = if root == 0 {
        BTree::create(Arc::clone(pool))?
    } else {
        BTree::open(Arc::clone(pool), PageId(root))
    };
    tree.set_branch(info.id);
    // A large commit's leaves prepared on the CPUs there are (one in
    // the simulation).
    tree.set_workers(platform.parallelism());
    let wal = Wal::open_from(
        Arc::clone(platform),
        wal_dir,
        opts.wal_segment_size,
        checkpoint,
    )?;
    let mut meta = meta;
    let mut replayed = 0;
    let mut applied = checkpoint.max(wal.start());
    if (checkpoint < wal.start() && checkpoint != 0) || checkpoint > wal.end() {
        // A checkpoint is only taken once the log is durable up to it, so
        // the log must cover it exactly.
        return Err(Error::Corrupt {
            page: PageId(0),
            reason: "the WAL does not line up with the checkpoint",
        });
    }
    let mut deferred = Deferred::new();
    wal.replay(applied, |lsn, payload| {
        let (batch, covered, put) = Batch::decode_full(payload)?;
        apply(&mut tree, &batch, None)?;
        remove_covered(&mut tree, &covered)?;
        overlay(&mut deferred, &batch, put);
        meta = batch.meta;
        replayed += 1;
        applied = lsn + 16 + payload.len() as u64;
        Ok(())
    })?;
    Ok(Branch {
        info,
        tree: RwLock::new(tree),
        writers_waiting: std::sync::atomic::AtomicUsize::new(0),
        wal,
        deferred: Mutex::new(deferred),
        writer: Mutex::new((applied, meta)),
        checkpoint_lsn: AtomicU64::new(checkpoint),
        recovery: Recovery { meta, replayed },
        dropped: AtomicBool::new(false),
    })
}

impl Store {
    /// Opens the store in directory `dir`, creating it if needed, and
    /// recovers it — every branch; the handle returned is the main
    /// branch's, the recovery its.
    pub fn open(
        platform: Arc<dyn Platform>,
        dir: &Path,
        opts: &StoreOptions,
    ) -> Result<(Self, Recovery)> {
        let fs = platform.fs();
        // One process at a time: the lock is held as long as the store.
        ensure_dir(fs, dir)?;
        let lock = fs.lock(&dir.join("LOCK")).map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                Error::Locked(dir.display().to_string())
            } else {
                Error::Io(e)
            }
        })?;
        let data = dir.join("data");
        let pager = if fs.exists(&data)? {
            Pager::open(fs, &data)?
        } else {
            Pager::create(fs, &data)?
        };
        let pager = Arc::new(pager);
        let pool = Arc::new(BufferPool::new(Arc::clone(&pager), opts.cache_pages));

        // The branch directory first: a free in any branch, replayed,
        // needs to know who else may read the page.
        let mut recs = Vec::new();
        let system = match pager.meta(META_SYSTEM) {
            0 => None,
            root => {
                let mut t = BTree::open(Arc::clone(&pool), PageId(root));
                t.set_branch(NO_BRANCH);
                recs = records(&t)?;
                Some(t)
            }
        };
        for (id, sp) in spaces(&recs) {
            pager.set_space(id, Some(sp));
        }

        let main_root = pager.meta(META_ROOT);
        let main = Arc::new(open_branch(
            &platform,
            &pool,
            opts,
            BranchInfo {
                id: MAIN,
                name: MAIN_NAME.to_owned(),
                parent: None,
                origin: None,
            },
            main_root,
            &branch_wal(dir, MAIN),
            pager.meta(META_CHECKPOINT_LSN),
            pager.meta(META_USER),
        )?);
        let mut replayed = main.recovery.replayed;
        let mut branches = BTreeMap::new();
        branches.insert(MAIN, Arc::clone(&main));
        for r in recs {
            let id = r.info.id;
            let b = open_branch(
                &platform,
                &pool,
                opts,
                r.info,
                r.root,
                &branch_wal(dir, id),
                r.lsn,
                r.meta,
            )?;
            replayed += b.recovery.replayed;
            branches.insert(id, Arc::new(b));
        }
        // Logs of branches a crash undid the making of, or whose drop it
        // interrupted after the commit.
        let bdir = dir.join("branches");
        if fs.exists(&bdir)? {
            for name in fs.list_dir(&bdir)? {
                let known = name
                    .to_str()
                    .and_then(|n| n.parse::<u32>().ok())
                    .is_some_and(|id| branches.contains_key(&id));
                if !known {
                    remove_wal_files(fs, &bdir.join(&name).join("wal"))?;
                }
            }
        }

        let recovery = main.recovery;
        let store = Self {
            shared: Arc::new(Shared {
                platform: Arc::clone(&platform),
                dir: dir.to_path_buf(),
                opts: opts.clone(),
                pool,
                system: Mutex::new(system),
                branches: Mutex::new(branches),
                checkpointing: Mutex::new(()),
                poisoned: AtomicBool::new(false),
                scrubbing: AtomicBool::new(false),
                wal_zeroed: AtomicU64::new(0),
                _lock: lock,
            }),
            b: main,
        };
        if replayed > 0 || main_root == 0 {
            store.checkpoint()?; // so the next open starts from here
        }
        Ok((store, recovery))
    }

    fn check(&self) -> Result<()> {
        self.shared.check()?;
        self.check_dropped()
    }

    fn check_dropped(&self) -> Result<()> {
        if self.b.dropped.load(Ordering::Acquire) {
            Err(Error::Branch(format!(
                "branch \"{}\" was dropped",
                self.b.info.name
            )))
        } else {
            Ok(())
        }
    }

    fn poison(&self) {
        self.shared.poison();
    }

    /// This handle's branch.
    pub fn info(&self) -> &BranchInfo {
        &self.b.info
    }

    /// What opening the store recovered of this branch (nothing, for one
    /// made since: its meta its parent's then).
    pub fn recovery(&self) -> Recovery {
        self.b.recovery
    }

    /// Every branch, the main one first.
    pub fn branches(&self) -> Vec<BranchInfo> {
        self.shared
            .branches()
            .values()
            .map(|b| b.info.clone())
            .collect()
    }

    /// A handle to branch `name` ([`MAIN_NAME`] the main one).
    pub fn branch(&self, name: &str) -> Result<Store> {
        self.shared.check()?;
        Ok(Store {
            shared: Arc::clone(&self.shared),
            b: self.shared.by_name(name)?,
        })
    }

    /// Makes branch `name` of this one, as it is now (PLAN.md §M14.13):
    /// a checkpoint that records the branch, its tree this one's root —
    /// constant time and space, whatever the size.  Writes on either side
    /// stay invisible to the other.
    pub fn create_branch(&self, name: &str) -> Result<Store> {
        if !valid_branch_name(name) {
            return Err(Error::Branch(format!(
                "\"{name}\" cannot name a branch: 1–63 letters, digits, '-', '_' or '.', not \"{MAIN_NAME}\""
            )));
        }
        self.check()?;
        let running = self.running();
        let (cp, made) = self.begin_running(
            running,
            Op::Create {
                parent: self.b.info.id,
                name,
            },
        )?;
        cp.finish()?;
        Ok(Store {
            shared: Arc::clone(&self.shared),
            b: made.expect("a branch made"),
        })
    }

    /// Drops branch `name` — one without branches of its own — in a
    /// checkpoint: its pages younger than its origin freed (a walk that
    /// skips what it shares, so it costs what the branch changed), the
    /// pages its parent kept for it given back, its log deleted.  Its
    /// handles fail from then on.
    pub fn drop_branch(&self, name: &str) -> Result<()> {
        self.shared.check()?;
        let running = self.running();
        let id = self.shared.by_name(name)?.info.id;
        self.begin_running(running, Op::Drop { id })?.0.finish()
    }

    /// Makes branch `name` (one without branches of its own) its parent
    /// as it is now: dropped and made again, in one checkpoint.  The new
    /// handle; the old ones fail.
    pub fn reset_branch(&self, name: &str) -> Result<Store> {
        self.shared.check()?;
        let running = self.running();
        let id = self.shared.by_name(name)?.info.id;
        let (cp, made) = self.begin_running(running, Op::Reset { id })?;
        cp.finish()?;
        Ok(Store {
            shared: Arc::clone(&self.shared),
            b: made.expect("a branch made"),
        })
    }

    fn running(&self) -> MutexGuard<'_, ()> {
        self.shared
            .checkpointing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Appends `batch` to the WAL and applies it.  Visible to readers when
    /// this returns; durable once [`Store::wait_durable`] with the returned
    /// LSN returns.
    pub fn write(&self, batch: &Batch) -> Result<Lsn> {
        self.write_with(batch, None)
    }

    /// [`write`](Store::write), `filter` dropping entries of the leaves the
    /// batch rewrites ([`LeafFilter`]) — not logged: what it drops must be
    /// garbage the layer above can live with finding again after a crash
    /// (replay applies the batch without it).
    pub fn write_filtered(&self, batch: &Batch, filter: &dyn LeafFilter) -> Result<Lsn> {
        self.write_with(batch, Some(filter))
    }

    /// [`write`](Store::write) of a batch of deletes that also *covers*
    /// key ranges `[start, end)`: ranges whose entries, in the tree as it
    /// is, are all among the batch's deletes.  Replay deletes everything
    /// in them — what a [`write_filtered`](Store::write_filtered) dropped
    /// there without logging comes back on replay and goes again — so a
    /// delete of what is left of something a filter thinned out stays
    /// whole after a crash (the transaction layer's vacuum of a key's
    /// last versions, below a filtered commit's drops).
    pub fn write_covering(&self, batch: &Batch, covered: &[(Vec<u8>, Vec<u8>)]) -> Result<Lsn> {
        self.write_encoded(batch, &batch.encode_covering(covered), None, Vec::new())
    }

    /// [`write_filtered`](Store::write_filtered), with puts `deferred`:
    /// logged with the batch, kept out of the tree until the next
    /// checkpoint or until [`DEFERRED_MAX`] wait ([`Store::deferred`]
    /// holds them meanwhile).  For entries read seldom, by whoever reads
    /// them through both.
    pub fn write_deferring(
        &self,
        batch: &Batch,
        deferred: Vec<(Vec<u8>, Vec<u8>)>,
        filter: &dyn LeafFilter,
    ) -> Result<Lsn> {
        let encoded = batch.encode_full(&[], &deferred);
        self.write_encoded(batch, &encoded, Some(filter), deferred)
    }

    fn write_with(&self, batch: &Batch, filter: Option<&dyn LeafFilter>) -> Result<Lsn> {
        self.write_encoded(batch, &batch.encode(), filter, Vec::new())
    }

    fn write_encoded(
        &self,
        batch: &Batch,
        encoded: &[u8],
        filter: Option<&dyn LeafFilter>,
        deferred: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<Lsn> {
        if let Some(k) = batch
            .ops
            .iter()
            .map(|(k, _)| k)
            .chain(deferred.iter().map(|(k, _)| k))
            .find(|k| k.len() > MAX_KEY)
        {
            return Err(Error::KeyTooLarge(k.len()));
        }
        self.check()?;
        let b = &*self.b;
        let mut w = b.writer();
        self.check()?;
        let result = b.wal.append(encoded).and_then(|lsn| {
            b.writers_waiting.fetch_add(1, Ordering::AcqRel);
            let mut tree = b.tree.write().unwrap_or_else(|e| e.into_inner());
            b.writers_waiting.fetch_sub(1, Ordering::AcqRel);
            apply(&mut tree, batch, filter)?;
            let mut d = b.deferred.lock().unwrap_or_else(|e| e.into_inner());
            overlay(&mut d, batch, deferred);
            if d.len() > DEFERRED_MAX {
                settle(&mut tree, &mut d)?;
            }
            Ok(lsn)
        });
        match result {
            Ok(lsn) => {
                *w = (lsn, batch.meta);
                Ok(lsn)
            }
            Err(e) => {
                self.poison();
                Err(e)
            }
        }
    }

    /// Applies `batch` to the tree without logging it (PLAN.md D16): it is
    /// durable only once a checkpoint after it is, and a crash before that
    /// loses it — what the layer above must be able to find again and
    /// delete (the transaction layer marks such a range in a logged record
    /// first, and its commit checkpoints before the commit record).
    pub fn write_unlogged(&self, batch: &Batch) -> Result<()> {
        if let Some((k, _)) = batch.ops.iter().find(|(k, _)| k.len() > MAX_KEY) {
            return Err(Error::KeyTooLarge(k.len()));
        }
        self.check()?;
        let b = &*self.b;
        let _w = b.writer();
        self.check()?;
        b.writers_waiting.fetch_add(1, Ordering::AcqRel);
        let mut tree = b.tree.write().unwrap_or_else(|e| e.into_inner());
        b.writers_waiting.fetch_sub(1, Ordering::AcqRel);
        apply(&mut tree, batch, None).inspect_err(|_| self.poison())?;
        overlay(
            &mut b.deferred.lock().unwrap_or_else(|e| e.into_inner()),
            batch,
            Vec::new(),
        );
        Ok(())
    }

    /// The deferred puts ([`Store::write_deferring`]), not yet in the
    /// tree.  Taken while [`Store::read`]'s guard is held, the two agree:
    /// an entry is in one or the other.  Hold it briefly.
    pub fn deferred(&self) -> MutexGuard<'_, Deferred> {
        self.b.deferred.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Returns once every write up to `lsn` is durable.
    pub fn wait_durable(&self, lsn: Lsn) -> Result<()> {
        self.check()?;
        self.b.wal.sync_to(lsn).inspect_err(|_| self.poison())
    }

    /// [`Store::write`] then [`Store::wait_durable`].
    pub fn commit(&self, batch: &Batch) -> Result<Lsn> {
        let lsn = self.write(batch)?;
        self.wait_durable(lsn)?;
        Ok(lsn)
    }

    /// The tree, for reading.  Hold the guard briefly: writers wait for it.
    pub fn read(&self) -> Result<RwLockReadGuard<'_, BTree>> {
        self.check()?;
        // A writer waiting goes first — for a few hundred yields at most
        // (a reader that holds the tree already, reading again, must not
        // wait for a writer that waits for it).
        for _ in 0..256 {
            if self.b.writers_waiting.load(Ordering::Acquire) == 0 {
                break;
            }
            std::thread::yield_now();
        }
        let tree = self.b.tree.read().unwrap_or_else(|e| e.into_inner());
        // Dropped meanwhile: its pages are not its own any more.
        self.check_dropped()?;
        Ok(tree)
    }

    /// The value of `key`.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.read()?.get(key)
    }

    /// Makes every branch's current tree the data file's committed state
    /// and drops the WAL segments before it
    /// ([`checkpoint_begin`](Self::checkpoint_begin) then
    /// [`Checkpoint::finish`]).
    pub fn checkpoint(&self) -> Result<()> {
        self.checkpoint_begin()?.finish()
    }

    /// Removes from the files what the trees no longer hold (PLAN.md
    /// §M14.24 R2): a checkpoint writes the trees as they are, the WALs
    /// move to new segments, a second checkpoint frees the pages the first
    /// replaced and drops every segment before it — overwritten with zeros
    /// first, and the spares too —, then every free page is overwritten
    /// with zeros.  Writes go on meanwhile; a checkpoint that runs between
    /// (the engine's) zeroes what it drops as well.  The deleted entries'
    /// bytes then remain only in the process's memory and, on a file
    /// system that copies on write or a disk that remaps, in blocks the
    /// files no longer name — and in pages a branch still shares.
    pub fn scrub(&self) -> Result<Scrubbed> {
        self.check()?;
        let sh = &*self.shared;
        sh.scrubbing.store(true, Ordering::Release);
        let before = sh.wal_zeroed.load(Ordering::Acquire);
        let r = (|| {
            self.checkpoint()?;
            let all: Vec<Arc<Branch>> = sh.branches().values().cloned().collect();
            for b in all {
                b.wal.roll()?;
            }
            self.checkpoint()?;
            sh.pool.pager().zero_free()
        })();
        sh.scrubbing.store(false, Ordering::Release);
        Ok(Scrubbed {
            pages: r?,
            wal_bytes: sh.wal_zeroed.load(Ordering::Acquire) - before,
        })
    }

    /// The first, short half of a checkpoint (§M14.20 W4): under every
    /// branch's writer lock, each tree as it is now — its root, the LSN
    /// applied, the last batch's meta — frozen as the next committed state
    /// ([`Pager::freeze`]).  The rest, [`Checkpoint::finish`], runs without
    /// the locks: writes go on meanwhile, copying the frozen pages they
    /// change.  Waits for a checkpoint already running.
    pub fn checkpoint_begin(&self) -> Result<Checkpoint<'_>> {
        self.shared.check()?;
        let running = self.running();
        Ok(self.begin_running(running, Op::Nothing)?.0)
    }

    /// A checkpoint, if this branch's WAL since the last one (once any
    /// running has ended) is past `bytes`; whether it ran.  Callers that
    /// find the WAL long at once do not checkpoint one after another.
    pub fn checkpoint_past(&self, bytes: u64) -> Result<bool> {
        self.shared.check()?;
        let running = self.running();
        if self.wal_since_checkpoint() <= bytes {
            return Ok(false);
        }
        self.begin_running(running, Op::Nothing)?.0.finish()?;
        Ok(true)
    }

    fn begin_running<'a>(
        &'a self,
        running: MutexGuard<'a, ()>,
        op: Op<'_>,
    ) -> Result<(Checkpoint<'a>, Option<Arc<Branch>>)> {
        let sh = &*self.shared;
        let list: Vec<Arc<Branch>> = sh.branches().values().cloned().collect();
        // Every writer held, in id order (writers take only their own).
        let guards: Vec<MutexGuard<'_, (Lsn, u64)>> = list.iter().map(|b| b.writer()).collect();
        sh.check()?;
        let states: Vec<Frozen1> = list
            .iter()
            .zip(&guards)
            .map(|(b, w)| (Arc::clone(b), 0, w.0, w.1))
            .collect();
        let frozen = self.freeze_all(states, op);
        drop(guards);
        let (lsns, frozen, made, dropped) = match frozen {
            Ok(f) => f,
            Err(e @ Error::Branch(_)) => return Err(e), // refused before any change
            Err(e) => {
                sh.poison();
                return Err(e);
            }
        };
        Ok((
            Checkpoint {
                shared: sh,
                lsns,
                frozen,
                dropped,
                _running: running,
            },
            made,
        ))
    }

    /// [`begin_running`](Self::begin_running)'s work, every writer held:
    /// each tree's deferred puts settled, `op` done, the pages put on
    /// dead lists and the branches' records written to the directory, the
    /// state frozen.
    #[allow(clippy::type_complexity)]
    fn freeze_all(
        &self,
        mut states: Vec<Frozen1>,
        op: Op<'_>,
    ) -> Result<(
        Vec<(Arc<Branch>, Lsn)>,
        Frozen,
        Option<Arc<Branch>>,
        Vec<Arc<Branch>>,
    )> {
        let sh = &*self.shared;
        let pager = sh.pool.pager();
        let mut sys = sh.system.lock().unwrap_or_else(|e| e.into_inner());
        // Refusals first, before anything changes.
        match op {
            Op::Nothing => {}
            Op::Create { name, .. } => {
                if states.iter().any(|s| s.0.info.name == name) {
                    return Err(Error::Branch(format!("branch \"{name}\" exists")));
                }
            }
            Op::Drop { id } | Op::Reset { id } => {
                let Some(s) = states.iter().find(|s| s.0.info.id == id) else {
                    return Err(Error::Branch("the branch was dropped".into()));
                };
                if id == MAIN {
                    return Err(Error::Branch(format!(
                        "the main branch \"{MAIN_NAME}\" cannot be dropped or reset"
                    )));
                }
                if let Some(c) = states.iter().find(|c| c.0.info.parent == Some(id)) {
                    return Err(Error::Branch(format!(
                        "branch \"{}\" has a branch of its own, \"{}\"",
                        s.0.info.name, c.0.info.name
                    )));
                }
            }
        }
        for s in &mut states {
            // The deferred puts into the tree: the WAL before the
            // checkpoint, which logged them, goes.
            let mut tree = s.0.tree.write().unwrap_or_else(|e| e.into_inner());
            let mut d = s.0.deferred.lock().unwrap_or_else(|e| e.into_inner());
            settle(&mut tree, &mut d)?;
            s.1 = tree.root().0;
        }
        // The dead lists so far into the directory (a drop reads them).
        let dead = pager.take_dead();
        if !dead.is_empty() {
            let t = system_tree(&mut sys, &sh.pool)?;
            for d in dead {
                t.insert(&dead_key(d.branch, d.point, d.page), &d.birth.to_le_bytes())?;
            }
        }
        let mut made = None;
        let mut dropped = Vec::new();
        match op {
            Op::Nothing => {}
            Op::Create { parent, name } => {
                made = Some(self.create_in(&mut sys, &mut states, parent, name.to_owned())?);
            }
            Op::Drop { id } => dropped.push(self.drop_in(&mut sys, &mut states, id)?),
            Op::Reset { id } => {
                let old = self.drop_in(&mut sys, &mut states, id)?;
                let parent = old.info.parent.expect("not the main branch");
                made =
                    Some(self.create_in(&mut sys, &mut states, parent, old.info.name.clone())?);
                dropped.push(old);
            }
        }
        // Each branch's record, where it changed.
        for (b, root, lsn, meta) in &states {
            if b.info.id == MAIN {
                continue;
            }
            let rec = Record {
                info: b.info.clone(),
                root: *root,
                lsn: *lsn,
                meta: *meta,
            }
            .encode();
            let key = branch_key(b.info.id);
            let t = system_tree(&mut sys, &sh.pool)?;
            if t.get(&key)?.as_deref() != Some(rec.as_slice()) {
                t.insert(&key, &rec)?;
            }
        }
        let main = states
            .iter()
            .find(|s| s.0.info.id == MAIN)
            .expect("the main branch");
        let frozen = pager.freeze(&[
            (META_ROOT, main.1),
            (META_CHECKPOINT_LSN, main.2),
            (META_USER, main.3),
            (META_SYSTEM, sys.as_ref().map_or(0, |t| t.root().0)),
        ])?;
        {
            let mut all = sh.branches();
            for b in &dropped {
                all.remove(&b.info.id);
            }
            if let Some(b) = &made {
                all.insert(b.info.id, Arc::clone(b));
            }
        }
        let lsns = states.into_iter().map(|(b, _, lsn, _)| (b, lsn)).collect();
        Ok((lsns, frozen, made, dropped))
    }

    /// Makes branch `name` of `parent` as frozen now (its root the
    /// parent's): recorded in the directory, its log made, the frees of
    /// both told ([`Pager::set_space`]).
    fn create_in(
        &self,
        sys: &mut Option<BTree>,
        states: &mut Vec<Frozen1>,
        parent: u32,
        name: String,
    ) -> Result<Arc<Branch>> {
        let sh = &*self.shared;
        let pager = sh.pool.pager();
        let p = states
            .iter()
            .find(|s| s.0.info.id == parent)
            .ok_or_else(|| Error::Branch("the parent branch was dropped".into()))?;
        let (root, meta) = (p.1, p.3);
        let t = system_tree(sys, &sh.pool)?;
        let id = match t.get(NEXT_ID_KEY)? {
            Some(v) if v.len() == 4 => u32::from_le_bytes(v.try_into().unwrap()),
            _ => 1,
        };
        t.insert(NEXT_ID_KEY, &(id + 1).to_le_bytes())?;
        t.insert(&name_key(&name), &id.to_be_bytes())?;
        // It starts from the commit this freeze makes.
        let origin = pager.next_seq();
        let info = BranchInfo {
            id,
            name,
            parent: Some(parent),
            origin: Some(origin),
        };
        let wal_dir = branch_wal(&sh.dir, id);
        // Left by a making a crash undid (ids come from the directory).
        remove_wal_files(sh.platform.fs(), &wal_dir)?;
        let b = Arc::new(open_branch(
            &sh.platform,
            &sh.pool,
            &sh.opts,
            info,
            root,
            &wal_dir,
            0,
            meta,
        )?);
        let mut ps = pager.space(parent);
        ps.points.push(origin);
        ps.points.sort_unstable();
        pager.set_space(parent, Some(ps));
        pager.set_space(
            id,
            Some(Space {
                origin: Some(origin),
                points: Vec::new(),
            }),
        );
        states.push((Arc::clone(&b), root, 0, meta));
        Ok(b)
    }

    /// Drops branch `id` (one without branches: checked): its own pages
    /// freed, its record gone, its parent's dead list for its branch
    /// point moved to the parent's previous point or freed.
    fn drop_in(
        &self,
        sys: &mut Option<BTree>,
        states: &mut Vec<Frozen1>,
        id: u32,
    ) -> Result<Arc<Branch>> {
        let sh = &*self.shared;
        let pager = sh.pool.pager();
        let pos = states
            .iter()
            .position(|s| s.0.info.id == id)
            .expect("checked");
        let (b, ..) = states.remove(pos);
        let parent = b.info.parent.expect("not the main branch");
        let origin = b.info.origin.expect("not the main branch");
        {
            // Under its tree's write lock: no reader of it meanwhile, and
            // none after (it is marked dropped first).
            let tree = b.tree.write().unwrap_or_else(|e| e.into_inner());
            b.dropped.store(true, Ordering::Release);
            tree.release_younger(Some(origin))?;
        }
        // Its own frees just now went nowhere but free (it has no
        // branches), so the dead lists in the directory are whole.
        let t = system_tree(sys, &sh.pool)?;
        t.remove(&branch_key(id))?;
        t.remove(&name_key(&b.info.name))?;
        let mut ps = pager.space(parent);
        if let Some(i) = ps.points.iter().position(|&p| p == origin) {
            ps.points.remove(i);
        }
        if !ps.points.contains(&origin) {
            // The pages the parent freed while this branch was its latest
            // (or a later one, since dropped, was): still read through an
            // earlier branch point if born by it, else no one's.
            let prev = ps.points.iter().rev().find(|&&p| p < origin).copied();
            let start = dead_prefix(parent, origin);
            let end = dead_prefix(parent, origin + 1);
            let list: Vec<(Vec<u8>, Vec<u8>)> = t
                .range(Bound::Included(&start), Bound::Excluded(&end))?
                .collect::<Result<_>>()?;
            for (k, v) in list {
                t.remove(&k)?;
                let page = u64::from_be_bytes(k[13..21].try_into().unwrap());
                let birth =
                    u64::from_le_bytes(v.as_slice().try_into().map_err(|_| Error::Corrupt {
                        page: PageId(page),
                        reason: "bad dead list entry",
                    })?);
                match prev {
                    Some(p) if birth <= p => t.insert(&dead_key(parent, p, page), &v)?,
                    _ => sh.pool.free(PageId(page))?,
                }
            }
        }
        pager.set_space(parent, Some(ps));
        pager.set_space(id, None);
        Ok(b)
    }

    /// Checks that every page of the file is in exactly one place: in use
    /// (in a branch's tree, the branch directory, a dead list, or
    /// allocated since the last commit) or free (free, freed since the
    /// last commit, holding the free list) — no page lost, none both —
    /// and every tree's invariants ([`BTree::check`]).  Waits for writes
    /// and checkpoints.
    pub fn check_pages(&self) -> Result<()> {
        self.check()?;
        let sh = &*self.shared;
        let _running = self.running();
        let list: Vec<Arc<Branch>> = sh.branches().values().cloned().collect();
        let _guards: Vec<_> = list.iter().map(|b| b.writer()).collect();
        let pager = sh.pool.pager();
        pager.check()?;
        let mut used = BTreeSet::new();
        for b in &list {
            let tree = b.tree.read().unwrap_or_else(|e| e.into_inner());
            used.extend(tree.check()?.pages);
        }
        if let Some(t) = sh.system.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            used.extend(t.check()?.pages);
            for e in t.range(Bound::Included(b"d"), Bound::Excluded(b"e"))? {
                let (k, _) = e?;
                used.insert(u64::from_be_bytes(k[13..21].try_into().unwrap()));
            }
        }
        used.extend(pager.dead().iter().map(|d| d.page));
        let a = pager.accounting();
        for id in FIRST_DATA_PAGE.0..a.page_count {
            let places = [
                used.contains(&id) || a.allocated.contains(&id),
                a.free.contains(&id),
                a.pending.contains(&id),
                a.freelist_pages.contains(&id),
                a.frozen.contains(&id),
            ];
            if places.iter().filter(|&&p| p).count() != 1 {
                return Err(Error::Corrupt {
                    page: PageId(id),
                    reason: "page not in exactly one place (in use or free)",
                });
            }
        }
        Ok(())
    }

    /// WAL bytes written to this branch since the last checkpoint — how
    /// much a crash would replay.  Callers checkpoint when it grows too
    /// large.
    pub fn wal_since_checkpoint(&self) -> u64 {
        self.b.wal.end()
            - self
                .b
                .checkpoint_lsn
                .load(Ordering::Acquire)
                .max(self.b.wal.start())
    }

    /// The buffer pool (for statistics and checks).
    pub fn pool(&self) -> &Arc<BufferPool> {
        &self.shared.pool
    }

    /// Checkpoints and closes the handle (the store closes with its last).
    pub fn close(self) -> Result<()> {
        if self.b.dropped.load(Ordering::Acquire) {
            return Ok(());
        }
        self.checkpoint()
    }
}

/// A checkpoint begun ([`Store::checkpoint_begin`]).
#[must_use = "a checkpoint begun is finished by `finish`"]
pub struct Checkpoint<'a> {
    shared: &'a Shared,
    /// Each branch frozen, with the LSN its log is applied up to.
    lsns: Vec<(Arc<Branch>, Lsn)>,
    frozen: Frozen,
    /// Branches dropped in it: their logs go once it is durable.
    dropped: Vec<Arc<Branch>>,
    _running: MutexGuard<'a, ()>,
}

impl Checkpoint<'_> {
    /// The rest of the checkpoint, writes going on: each WAL durable up
    /// to its frozen LSN, the frozen pages written back, the pager's
    /// header committed, each WAL before its LSN dropped, dropped
    /// branches' logs deleted.  A failure poisons the store.
    pub fn finish(self) -> Result<()> {
        let Checkpoint {
            shared: s,
            lsns,
            frozen,
            dropped,
            _running,
        } = self;
        let result = (|| {
            // Nothing in the checkpoint may be less durable than the WALs.
            for (b, lsn) in &lsns {
                b.wal.sync_to(*lsn)?;
            }
            s.pool.flush_frozen()?;
            s.pool.pager().commit_frozen(frozen)?;
            for (b, lsn) in &lsns {
                b.checkpoint_lsn.store(*lsn, Ordering::Release);
                if s.scrubbing.load(Ordering::Acquire) {
                    let n = b.wal.truncate_before_zeroed(*lsn)?;
                    s.wal_zeroed.fetch_add(n, Ordering::AcqRel);
                } else {
                    b.wal.truncate_before(*lsn)?;
                }
            }
            for b in &dropped {
                remove_wal_files(s.platform.fs(), &branch_wal(&s.dir, b.info.id))?;
            }
            Ok(())
        })();
        result.inspect_err(|_| s.poison())
    }
}

/// A batch's effect on the deferred puts: its own keys' deferred entries
/// replaced (by its put or delete, applied to the tree), then its
/// deferred puts added.
fn overlay(d: &mut Deferred, batch: &Batch, put: Vec<(Vec<u8>, Vec<u8>)>) {
    if let (Some((first, _)), Some((last, _))) = (d.first_key_value(), d.last_key_value()) {
        let (first, last) = (first.clone(), last.clone());
        for (k, _) in &batch.ops {
            if *k >= first && *k <= last {
                d.remove(k);
            }
        }
    }
    d.extend(put);
}

/// The deferred puts into the tree.
fn settle(tree: &mut BTree, d: &mut Deferred) -> Result<()> {
    if d.is_empty() {
        return Ok(());
    }
    let ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = std::mem::take(d)
        .into_iter()
        .map(|(k, v)| (k, Some(v)))
        .collect();
    tree.apply_sorted(&ops)
}

/// Replay's half of [`Store::write_covering`]: every entry left in the
/// covered ranges deleted.
fn remove_covered(tree: &mut BTree, covered: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
    for (start, end) in covered {
        let keys = tree
            .range(
                std::ops::Bound::Included(start),
                std::ops::Bound::Excluded(end),
            )?
            .map(|e| e.map(|(k, _)| k))
            .collect::<Result<Vec<_>>>()?;
        for k in keys {
            tree.remove(&k)?;
        }
    }
    Ok(())
}

fn apply(tree: &mut BTree, batch: &Batch, filter: Option<&dyn LeafFilter>) -> Result<()> {
    let sorted = |tree: &mut BTree, ops: &[(Vec<u8>, Option<Vec<u8>>)]| match filter {
        Some(f) => tree.apply_sorted_filtered(ops, f),
        None => tree.apply_sorted(ops),
    };
    // When no key comes twice (how the engine writes batches), the order
    // does not matter: one sorted pass over the tree, a page written once
    // for all its keys.
    // (Already sorted — how the engine writes its commits —, as it is.)
    // A filter wants the sorted pass even for one key (it acts as leaves
    // are rewritten).
    let many = batch.ops.len() > 1 || (filter.is_some() && !batch.ops.is_empty());
    if many && batch.ops.windows(2).all(|w| w[0].0 < w[1].0) {
        return sorted(tree, &batch.ops);
    }
    let mut ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = batch.ops.clone();
    ops.sort_by(|a, b| a.0.cmp(&b.0));
    if ops.len() > 1 && ops.windows(2).all(|w| w[0].0 != w[1].0) {
        return sorted(tree, &ops);
    }
    for (k, v) in &batch.ops {
        match v {
            Some(v) => tree.insert(k, v)?,
            None => {
                tree.remove(k)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_roundtrip() {
        let b = Batch {
            meta: 42,
            ops: vec![
                (b"a".to_vec(), Some(b"1".to_vec())),
                (b"b".to_vec(), None),
                (vec![], Some(vec![])),
            ],
        };
        assert_eq!(Batch::decode(&b.encode()).unwrap(), b);
        assert!(Batch::decode(&b.encode()[..10]).is_err());
        // With covered ranges: version 2, read back whole.
        let covered = vec![(b"a".to_vec(), b"c".to_vec()), (vec![], vec![0])];
        let e = b.encode_covering(&covered);
        assert_eq!(e[0], BATCH_VERSION_COVERING);
        assert_eq!(
            Batch::decode_covering(&e).unwrap(),
            (b.clone(), covered.clone())
        );
        assert!(Batch::decode_covering(&e[..e.len() - 1]).is_err());
        assert_eq!(b.encode_covering(&[]), b.encode());
        // With deferred puts: version 3, its ranges maybe none.
        let deferred = vec![(b"t".to_vec(), b"9".to_vec())];
        for covered in [vec![], covered] {
            let e = b.encode_full(&covered, &deferred);
            assert_eq!(e[0], BATCH_VERSION_DEFERRING);
            assert_eq!(
                Batch::decode_full(&e).unwrap(),
                (b.clone(), covered.clone(), deferred.clone())
            );
            assert!(Batch::decode_full(&e[..e.len() - 1]).is_err());
        }
    }

    /// A file system whose directories above `/apps/me` may not be
    /// opened (as an Android app may not open `/data/user`), recording
    /// the directories synced.
    struct Confined {
        inner: nucleus_platform::memory::MemoryFs,
        synced: Mutex<Vec<PathBuf>>,
    }

    impl FileSystem for Confined {
        fn open(
            &self,
            path: &Path,
            mode: nucleus_platform::OpenMode,
        ) -> std::io::Result<Box<dyn nucleus_platform::File>> {
            self.inner.open(path, mode)
        }
        fn remove_file(&self, path: &Path) -> std::io::Result<()> {
            self.inner.remove_file(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            self.inner.rename(from, to)
        }
        fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
            self.inner.create_dir_all(path)
        }
        fn list_dir(&self, path: &Path) -> std::io::Result<Vec<std::ffi::OsString>> {
            self.inner.list_dir(path)
        }
        fn exists(&self, path: &Path) -> std::io::Result<bool> {
            self.inner.exists(path)
        }
        fn sync_dir(&self, path: &Path) -> std::io::Result<()> {
            if !path.starts_with("/apps/me") {
                return Err(std::io::ErrorKind::PermissionDenied.into());
            }
            self.synced.lock().unwrap().push(path.to_path_buf());
            self.inner.sync_dir(path)
        }
        fn lock(&self, path: &Path) -> std::io::Result<Box<dyn nucleus_platform::Lock>> {
            self.inner.lock(path)
        }
    }

    /// Making a directory syncs the parents of the levels it made, and no
    /// directory above them (PLAN.md §M14.23 step 3).
    #[test]
    fn ensure_dir_syncs_only_the_new_levels_parents() {
        let fs = Confined {
            inner: nucleus_platform::memory::MemoryFs::new(),
            synced: Mutex::new(Vec::new()),
        };
        fs.inner.create_dir_all(Path::new("/apps/me")).unwrap();
        ensure_dir(&fs, Path::new("/apps/me/files/nucleus/notes")).unwrap();
        assert!(
            fs.exists(Path::new("/apps/me/files/nucleus/notes"))
                .unwrap()
        );
        let synced = fs.synced.lock().unwrap().clone();
        assert_eq!(
            synced,
            ["/apps/me", "/apps/me/files", "/apps/me/files/nucleus"].map(PathBuf::from)
        );
        // One that exists already: nothing to make, nothing synced.
        ensure_dir(&fs, Path::new("/apps/me/files")).unwrap();
        assert_eq!(fs.synced.lock().unwrap().len(), 3);
    }
}
