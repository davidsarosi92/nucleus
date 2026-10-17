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

use crate::btree::BTree;
use crate::buffer::BufferPool;
use crate::error::{Error, Result};
use crate::node::MAX_KEY;
use crate::page::PageId;
use crate::pager::Pager;
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
    wal: Wal,
    /// Serialises writers and checkpoints; holds the applied LSN and meta.
    writer: Mutex<(Lsn, u64)>,
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
    fs.create_dir_all(dir)?;
    // Sync each new level's parent, outermost first.
    let mut levels: Vec<&Path> = dir.ancestors().skip(1).collect();
    levels.reverse();
    for parent in levels {
        if !parent.as_os_str().is_empty() && fs.exists(parent)? {
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
        let wal = Wal::open(Arc::clone(&platform), &wal_dir, opts.wal_segment_size)?;

        let checkpoint = pager.meta(META_CHECKPOINT_LSN);
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
            apply(&mut tree, &batch)?;
            meta = batch.meta;
            replayed += 1;
            applied = lsn + 16 + payload.len() as u64;
            Ok(())
        })?;

        let store = Self {
            pool,
            tree: RwLock::new(tree),
            wal,
            writer: Mutex::new((applied, meta)),
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
        if let Some((k, _)) = batch.ops.iter().find(|(k, _)| k.len() > MAX_KEY) {
            return Err(Error::KeyTooLarge(k.len()));
        }
        self.check()?;
        let mut w = self.writer();
        self.check()?;
        let result = self.wal.append(&batch.encode()).and_then(|lsn| {
            let mut tree = self.tree.write().unwrap_or_else(|e| e.into_inner());
            apply(&mut tree, batch)?;
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
        Ok(self.tree.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// The value of `key`.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.read()?.get(key)
    }

    /// Makes the current tree the data file's committed state and drops the
    /// WAL segments before it.
    pub fn checkpoint(&self) -> Result<()> {
        self.check()?;
        let w = self.writer();
        let (lsn, meta) = *w;
        let result = (|| {
            self.wal.sync_to(lsn)?; // nothing in the checkpoint may be less durable than the WAL
            let tree = self.tree.read().unwrap_or_else(|e| e.into_inner());
            let pager = self.pool.pager();
            pager.set_meta(META_ROOT, tree.root().0)?;
            pager.set_meta(META_CHECKPOINT_LSN, lsn)?;
            pager.set_meta(META_USER, meta)?;
            self.pool.sync()?;
            self.checkpoint_lsn.store(lsn, Ordering::Release);
            self.wal.truncate_before(lsn)
        })();
        drop(w);
        result.inspect_err(|_| self.poisoned.store(true, Ordering::Release))
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

fn apply(tree: &mut BTree, batch: &Batch) -> Result<()> {
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
}
