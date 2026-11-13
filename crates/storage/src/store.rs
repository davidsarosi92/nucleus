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
//! A write or checkpoint that fails part-way **poisons** the store: memory
//! and disk may disagree, so every later call fails until it is reopened.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard};

use nucleus_platform::{FileSystem, Platform};

use crate::btree::{BTree, LeafFilter};
use crate::buffer::BufferPool;
use crate::error::{Error, Result};
use crate::node::MAX_KEY;
use crate::page::PageId;
use crate::pager::{Frozen, Pager};
use crate::wal::{Lsn, Wal};

const META_ROOT: usize = 0;
const META_CHECKPOINT_LSN: usize = 1;
const META_USER: usize = 2;
const BATCH_VERSION: u8 = 1;

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
        let mut out = Vec::new();
        out.push(BATCH_VERSION);
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
        out
    }

    fn decode(mut b: &[u8]) -> Result<Self> {
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
        if take(&mut b, 1)?[0] != BATCH_VERSION {
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
        Ok(Self { meta, ops })
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

/// A durable, crash-safe ordered key-value store.
pub struct Store {
    pool: Arc<BufferPool>,
    tree: RwLock<BTree>,
    /// Writers waiting for the tree: new readers let them go first (a
    /// while), so a stream of scans does not keep a writer out.
    writers_waiting: std::sync::atomic::AtomicUsize,
    wal: Wal,
    /// Serialises writers (and a checkpoint's freeze); holds the applied
    /// LSN and meta.
    writer: Mutex<(Lsn, u64)>,
    /// One checkpoint at a time.
    checkpointing: Mutex<()>,
    checkpoint_lsn: AtomicU64,
    poisoned: AtomicBool,
    /// The exclusive lock on the directory (`LOCK`).
    _lock: Box<dyn nucleus_platform::Lock>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("wal", &self.wal)
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

impl Store {
    /// Opens the store in directory `dir`, creating it if needed, and
    /// recovers it.
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
        let wal_dir: PathBuf = dir.join("wal");
        ensure_dir(fs, &wal_dir)?;
        let data = dir.join("data");
        let pager = if fs.exists(&data)? {
            Pager::open(fs, &data)?
        } else {
            Pager::create(fs, &data)?
        };
        let pager = Arc::new(pager);
        let pool = Arc::new(BufferPool::new(Arc::clone(&pager), opts.cache_pages));
        let root = pager.meta(META_ROOT);
        let mut tree = if root == 0 {
            BTree::create(Arc::clone(&pool))?
        } else {
            BTree::open(Arc::clone(&pool), PageId(root))
        };
        // A large commit's leaves prepared on the CPUs there are (one in
        // the simulation).
        tree.set_workers(platform.parallelism());
        let checkpoint = pager.meta(META_CHECKPOINT_LSN);
        let wal = Wal::open_from(
            Arc::clone(&platform),
            &wal_dir,
            opts.wal_segment_size,
            checkpoint,
        )?;

        let mut meta = pager.meta(META_USER);
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
        wal.replay(applied, |lsn, payload| {
            let batch = Batch::decode(payload)?;
            apply(&mut tree, &batch, None)?;
            meta = batch.meta;
            replayed += 1;
            applied = lsn + 16 + payload.len() as u64;
            Ok(())
        })?;

        let store = Self {
            pool,
            tree: RwLock::new(tree),
            writers_waiting: std::sync::atomic::AtomicUsize::new(0),
            wal,
            writer: Mutex::new((applied, meta)),
            checkpointing: Mutex::new(()),
            checkpoint_lsn: AtomicU64::new(checkpoint),
            poisoned: AtomicBool::new(false),
            _lock: lock,
        };
        if replayed > 0 || root == 0 {
            store.checkpoint()?; // so the next open starts from here
        }
        Ok((store, Recovery { meta, replayed }))
    }

    fn check(&self) -> Result<()> {
        if self.poisoned.load(Ordering::Acquire) {
            Err(Error::Poisoned)
        } else {
            Ok(())
        }
    }

    fn writer(&self) -> MutexGuard<'_, (Lsn, u64)> {
        self.writer.lock().unwrap_or_else(|e| e.into_inner())
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

    fn write_with(&self, batch: &Batch, filter: Option<&dyn LeafFilter>) -> Result<Lsn> {
        if let Some((k, _)) = batch.ops.iter().find(|(k, _)| k.len() > MAX_KEY) {
            return Err(Error::KeyTooLarge(k.len()));
        }
        self.check()?;
        let mut w = self.writer();
        self.check()?;
        let result = self.wal.append(&batch.encode()).and_then(|lsn| {
            self.writers_waiting.fetch_add(1, Ordering::AcqRel);
            let mut tree = self.tree.write().unwrap_or_else(|e| e.into_inner());
            self.writers_waiting.fetch_sub(1, Ordering::AcqRel);
            apply(&mut tree, batch, filter)?;
            Ok(lsn)
        });
        match result {
            Ok(lsn) => {
                *w = (lsn, batch.meta);
                Ok(lsn)
            }
            Err(e) => {
                self.poisoned.store(true, Ordering::Release);
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
        let _w = self.writer();
        self.check()?;
        self.writers_waiting.fetch_add(1, Ordering::AcqRel);
        let mut tree = self.tree.write().unwrap_or_else(|e| e.into_inner());
        self.writers_waiting.fetch_sub(1, Ordering::AcqRel);
        apply(&mut tree, batch, None).inspect_err(|_| self.poisoned.store(true, Ordering::Release))
    }

    /// Returns once every write up to `lsn` is durable.
    pub fn wait_durable(&self, lsn: Lsn) -> Result<()> {
        self.check()?;
        self.wal
            .sync_to(lsn)
            .inspect_err(|_| self.poisoned.store(true, Ordering::Release))
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
            if self.writers_waiting.load(Ordering::Acquire) == 0 {
                break;
            }
            std::thread::yield_now();
        }
        Ok(self.tree.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// The value of `key`.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.read()?.get(key)
    }

    /// Makes the current tree the data file's committed state and drops the
    /// WAL segments before it ([`checkpoint_begin`](Self::checkpoint_begin)
    /// then [`Checkpoint::finish`]).
    pub fn checkpoint(&self) -> Result<()> {
        self.checkpoint_begin()?.finish()
    }

    /// The first, short half of a checkpoint (§M14.20 W4): under the
    /// writer lock, the tree as it is now — its root, the LSN applied, the
    /// last batch's meta — frozen as the next committed state
    /// ([`Pager::freeze`]).  The rest, [`Checkpoint::finish`], runs without
    /// the lock: writes go on meanwhile, copying the frozen pages they
    /// change.  Waits for a checkpoint already running.
    pub fn checkpoint_begin(&self) -> Result<Checkpoint<'_>> {
        self.check()?;
        let running = self.checkpointing.lock().unwrap_or_else(|e| e.into_inner());
        self.begin_running(running)
    }

    /// A checkpoint, if the WAL since the last one (once any running has
    /// ended) is past `bytes`; whether it ran.  Callers that find the WAL
    /// long at once do not checkpoint one after another.
    pub fn checkpoint_past(&self, bytes: u64) -> Result<bool> {
        self.check()?;
        let running = self.checkpointing.lock().unwrap_or_else(|e| e.into_inner());
        if self.wal_since_checkpoint() <= bytes {
            return Ok(false);
        }
        self.begin_running(running)?.finish()?;
        Ok(true)
    }

    fn begin_running<'a>(&'a self, running: MutexGuard<'a, ()>) -> Result<Checkpoint<'a>> {
        let w = self.writer();
        self.check()?;
        let (lsn, meta) = *w;
        let root = self.tree.read().unwrap_or_else(|e| e.into_inner()).root();
        let frozen = self
            .pool
            .pager()
            .freeze(&[
                (META_ROOT, root.0),
                (META_CHECKPOINT_LSN, lsn),
                (META_USER, meta),
            ])
            .inspect_err(|_| self.poisoned.store(true, Ordering::Release))?;
        drop(w);
        Ok(Checkpoint {
            store: self,
            lsn,
            frozen,
            _running: running,
        })
    }

    /// WAL bytes written since the last checkpoint — how much a crash would
    /// replay.  Callers checkpoint when it grows too large.
    pub fn wal_since_checkpoint(&self) -> u64 {
        self.wal.end()
            - self
                .checkpoint_lsn
                .load(Ordering::Acquire)
                .max(self.wal.start())
    }

    /// The buffer pool (for statistics and checks).
    pub fn pool(&self) -> &Arc<BufferPool> {
        &self.pool
    }

    /// Checkpoints and closes the store.
    pub fn close(self) -> Result<()> {
        self.checkpoint()
    }
}

/// A checkpoint begun ([`Store::checkpoint_begin`]).
#[must_use = "a checkpoint begun is finished by `finish`"]
pub struct Checkpoint<'a> {
    store: &'a Store,
    lsn: Lsn,
    frozen: Frozen,
    _running: MutexGuard<'a, ()>,
}

impl Checkpoint<'_> {
    /// The rest of the checkpoint, writes going on: the WAL durable up to
    /// the frozen LSN, the frozen pages written back, the pager's header
    /// committed, the WAL before the LSN dropped.  A failure poisons the
    /// store.
    pub fn finish(self) -> Result<()> {
        let s = self.store;
        let lsn = self.lsn;
        let result = (|| {
            // Nothing in the checkpoint may be less durable than the WAL.
            s.wal.sync_to(lsn)?;
            s.pool.flush_frozen()?;
            s.pool.pager().commit_frozen(self.frozen)?;
            s.checkpoint_lsn.store(lsn, Ordering::Release);
            s.wal.truncate_before(lsn)
        })();
        result.inspect_err(|_| s.poisoned.store(true, Ordering::Release))
    }
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
