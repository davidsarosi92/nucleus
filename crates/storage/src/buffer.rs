//! The buffer pool: a fixed number of page frames caching the pager.
//!
//! [`BufferPool::fetch`] returns a [`PageRef`] that **pins** the page: a
//! pinned page is never evicted.  Dropping the `PageRef` unpins it.  Each
//! frame has its own reader/writer latch, taken by [`PageRef::read`] and
//! [`PageRef::write`]; writing marks the frame dirty.
//!
//! Eviction is the **clock** algorithm: a hand sweeps the frames, giving
//! each recently used unpinned frame a second chance, and evicts the first
//! unpinned frame not used since the hand last passed.  A dirty victim is
//! written to the pager first.
//!
//! Because the pager refuses writes to durable pages, a dirty page can be
//! written back at any time — eviction never risks the committed state, so
//! there is no "no-steal" restriction.  [`PageRef::write`] checks the same
//! rule up front, so a caller that forgets to copy-on-write fails at the
//! write, not at a later eviction.
//!
//! Page table changes and eviction take one pool lock; a miss reads the page
//! while holding it.  Hits only touch the lock briefly.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::error::{Error, Result};
use crate::page::{Page, PageId, PageType};
use crate::pager::Pager;

struct Frame {
    page: RwLock<Page>,
    pins: AtomicU32,
    dirty: AtomicBool,
    referenced: AtomicBool,
}

struct Inner {
    table: HashMap<PageId, usize>,
    ids: Vec<Option<PageId>>,
    free_frames: Vec<usize>,
    hand: usize,
}

/// Counters, for tests and diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Fetches answered from a frame.
    pub hits: u64,
    /// Fetches that read the page from the pager.
    pub misses: u64,
    /// Frames reused for another page.
    pub evictions: u64,
    /// Dirty pages written back (by eviction or flush).
    pub writebacks: u64,
}

/// A page cache over a [`Pager`].
pub struct BufferPool {
    pager: Arc<Pager>,
    frames: Box<[Frame]>,
    inner: Mutex<Inner>,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
    writebacks: AtomicU64,
}

impl std::fmt::Debug for BufferPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferPool")
            .field("frames", &self.frames.len())
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// A pinned page.  Unpins on drop.
pub struct PageRef<'a> {
    pool: &'a BufferPool,
    frame: usize,
    id: PageId,
}

impl std::fmt::Debug for PageRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PageRef").field(&self.id).finish()
    }
}

impl<'a> PageRef<'a> {
    /// The page's id.
    pub fn id(&self) -> PageId {
        self.id
    }

    /// Latches the page for reading.
    pub fn read(&self) -> RwLockReadGuard<'_, Page> {
        self.frame().page.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Latches the page for writing and marks it dirty.  Fails with
    /// [`Error::Immutable`] if the page belongs to the durable state.
    pub fn write(&self) -> Result<RwLockWriteGuard<'_, Page>> {
        if !self.pool.pager.is_writable(self.id) {
            return Err(Error::Immutable(self.id));
        }
        let guard = self.frame().page.write().unwrap_or_else(|e| e.into_inner());
        self.frame().dirty.store(true, Ordering::Release);
        Ok(guard)
    }

    fn frame(&self) -> &'a Frame {
        &self.pool.frames[self.frame]
    }
}

impl Drop for PageRef<'_> {
    fn drop(&mut self) {
        self.frame().pins.fetch_sub(1, Ordering::Release);
    }
}

impl BufferPool {
    /// A pool of `capacity` frames (at least 1) over `pager`.
    pub fn new(pager: Arc<Pager>, capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let frames = (0..capacity)
            .map(|_| Frame {
                page: RwLock::new(Page::new(PageType::UNUSED)),
                pins: AtomicU32::new(0),
                dirty: AtomicBool::new(false),
                referenced: AtomicBool::new(false),
            })
            .collect();
        Self {
            pager,
            frames,
            inner: Mutex::new(Inner {
                table: HashMap::new(),
                ids: vec![None; capacity],
                free_frames: (0..capacity).rev().collect(),
                hand: 0,
            }),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            writebacks: AtomicU64::new(0),
        }
    }

    /// The pager underneath.
    pub fn pager(&self) -> &Arc<Pager> {
        &self.pager
    }

    /// Number of frames.
    pub fn capacity(&self) -> usize {
        self.frames.len()
    }

    /// The counters so far.
    pub fn stats(&self) -> PoolStats {
        PoolStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            writebacks: self.writebacks.load(Ordering::Relaxed),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pin(&self, frame: usize, id: PageId) -> PageRef<'_> {
        let f = &self.frames[frame];
        f.pins.fetch_add(1, Ordering::Acquire);
        f.referenced.store(true, Ordering::Relaxed);
        PageRef {
            pool: self,
            frame,
            id,
        }
    }

    /// Pins page `id`, reading it from the pager on a miss.
    pub fn fetch(&self, id: PageId) -> Result<PageRef<'_>> {
        let mut inner = self.lock();
        if let Some(&frame) = inner.table.get(&id) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(self.pin(frame, id));
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let page = self.pager.read_page(id)?;
        let frame = self.victim(&mut inner)?;
        *self.frames[frame]
            .page
            .write()
            .unwrap_or_else(|e| e.into_inner()) = page;
        self.install(&mut inner, frame, id, false);
        Ok(self.pin(frame, id))
    }

    /// Allocates a new page of type `ty` and pins it.  The page is dirty: it
    /// reaches the file on eviction or flush.
    pub fn create(&self, ty: PageType) -> Result<PageRef<'_>> {
        let mut inner = self.lock();
        let frame = self.victim(&mut inner)?;
        let id = match self.pager.allocate() {
            Ok(id) => id,
            Err(e) => {
                inner.free_frames.push(frame);
                return Err(e);
            }
        };
        *self.frames[frame]
            .page
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Page::new(ty);
        self.install(&mut inner, frame, id, true);
        Ok(self.pin(frame, id))
    }

    fn install(&self, inner: &mut Inner, frame: usize, id: PageId, dirty: bool) {
        inner.table.insert(id, frame);
        inner.ids[frame] = Some(id);
        self.frames[frame].dirty.store(dirty, Ordering::Release);
    }

    /// Frees page `id` (see [`Pager::free`]) and drops it from the cache.
    /// The page must not be pinned.
    pub fn free(&self, id: PageId) -> Result<()> {
        let mut inner = self.lock();
        if let Some(&frame) = inner.table.get(&id) {
            if self.frames[frame].pins.load(Ordering::Acquire) != 0 {
                return Err(Error::Pinned(id));
            }
            inner.table.remove(&id);
            inner.ids[frame] = None;
            self.frames[frame].dirty.store(false, Ordering::Release);
            inner.free_frames.push(frame);
        }
        self.pager.free(id)
    }

    /// Whether `id` may be written in place (see [`Pager::is_writable`]).
    pub fn is_writable(&self, id: PageId) -> bool {
        self.pager.is_writable(id)
    }

    /// A frame for a new page: a free one, else a clock victim.
    fn victim(&self, inner: &mut Inner) -> Result<usize> {
        if let Some(frame) = inner.free_frames.pop() {
            return Ok(frame);
        }
        let n = self.frames.len();
        // Two sweeps: the first may only clear reference bits.
        for _ in 0..2 * n {
            let frame = inner.hand;
            inner.hand = (inner.hand + 1) % n;
            let f = &self.frames[frame];
            // Pins only grow under the pool lock, which we hold, so a zero
            // seen here stays zero until we are done.
            if f.pins.load(Ordering::Acquire) != 0 {
                continue;
            }
            if f.referenced.swap(false, Ordering::Relaxed) {
                continue;
            }
            let id = inner.ids[frame].expect("a frame outside the free list holds a page");
            if f.dirty.load(Ordering::Acquire) {
                let mut page = f.page.write().unwrap_or_else(|e| e.into_inner());
                self.pager.write_page(id, &mut page)?;
                f.dirty.store(false, Ordering::Release);
                self.writebacks.fetch_add(1, Ordering::Relaxed);
            }
            inner.table.remove(&id);
            inner.ids[frame] = None;
            self.evictions.fetch_add(1, Ordering::Relaxed);
            return Ok(frame);
        }
        Err(Error::PoolExhausted)
    }

    /// Writes every dirty page to the pager.  Pages stay cached.
    pub fn flush(&self) -> Result<()> {
        let inner = self.lock();
        for (frame, id) in inner.ids.iter().enumerate() {
            let (Some(id), f) = (id, &self.frames[frame]) else {
                continue;
            };
            // Clear the flag before copying: a write that starts after the
            // copy sets it again and is flushed next time.
            if f.dirty.swap(false, Ordering::AcqRel) {
                let mut copy = f.page.read().unwrap_or_else(|e| e.into_inner()).clone();
                if let Err(e) = self.pager.write_page(*id, &mut copy) {
                    f.dirty.store(true, Ordering::Release);
                    return Err(e);
                }
                self.writebacks.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    /// Flushes and commits (see [`Pager::sync`]).  Cached pages become
    /// durable, so writing them afterwards needs a copy.
    pub fn sync(&self) -> Result<()> {
        self.flush()?;
        self.pager.sync()
    }
}
