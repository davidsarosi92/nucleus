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
//! The frames are split into shards, a page always in the shard of its
//! id: each shard's page table, free frames and clock hand behind its own
//! lock, so threads reading different pages (a parallel scan's workers) do
//! not queue on one.  A pool too small to split has one shard.  A miss
//! reads the page without any lock; hits only touch their shard's briefly.

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

/// One shard: frames `start..end` (by global index) and the pages in them.
struct Inner {
    start: usize,
    end: usize,
    table: HashMap<PageId, usize>,
    /// By frame, less `start`.
    ids: Vec<Option<PageId>>,
    free_frames: Vec<usize>,
    hand: usize,
}

impl Inner {
    fn id(&mut self, frame: usize) -> &mut Option<PageId> {
        &mut self.ids[frame - self.start]
    }
}

/// Frames per shard at least, and shards at most.
const SHARD_FRAMES: usize = 256;
const MAX_SHARDS: usize = 16;

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
    /// Each made when first used, so a large pool costs nothing until it
    /// fills.
    frames: Box<[std::sync::OnceLock<Frame>]>,
    shards: Box<[Mutex<Inner>]>,
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
        self.pool.frame(self.frame)
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
        let frames = (0..capacity).map(|_| std::sync::OnceLock::new()).collect();
        let n = (capacity / SHARD_FRAMES).clamp(1, MAX_SHARDS);
        let shards = (0..n)
            .map(|i| {
                let (start, end) = (capacity * i / n, capacity * (i + 1) / n);
                Mutex::new(Inner {
                    start,
                    end,
                    table: HashMap::new(),
                    ids: vec![None; end - start],
                    free_frames: (start..end).rev().collect(),
                    hand: start,
                })
            })
            .collect();
        Self {
            pager,
            frames,
            shards,
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

    /// Frame `i`, made on first use.
    fn frame(&self, i: usize) -> &Frame {
        self.frames[i].get_or_init(|| Frame {
            page: RwLock::new(Page::new(PageType::UNUSED)),
            pins: AtomicU32::new(0),
            dirty: AtomicBool::new(false),
            referenced: AtomicBool::new(false),
        })
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

    /// The shard of page `id`, locked.
    fn lock(&self, id: PageId) -> MutexGuard<'_, Inner> {
        let n = self.shards.len();
        let at = if n == 1 {
            0
        } else {
            // Neighbouring pages (a scan's) to different shards.
            (id.0.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize % n
        };
        self.shards[at].lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pin(&self, frame: usize, id: PageId) -> PageRef<'_> {
        let f = self.frame(frame);
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
        loop {
            let epoch = {
                let inner = self.lock(id);
                if let Some(&frame) = inner.table.get(&id) {
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(self.pin(frame, id));
                }
                self.writebacks.load(Ordering::Acquire)
            };
            // Read without the pool's lock, so other threads' fetches go on
            // meanwhile (a parallel scan's workers, reading cold pages).
            let page = self.pager.read_page(id)?;
            let inner = self.lock(id);
            if let Some(&frame) = inner.table.get(&id) {
                // Another thread read it meanwhile.
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(self.pin(frame, id));
            }
            if self.writebacks.load(Ordering::Acquire) != epoch {
                // A page went back to the file meanwhile — perhaps this one,
                // newer than what was read: read it again.
                continue;
            }
            self.misses.fetch_add(1, Ordering::Relaxed);
            return self.load(inner, id, page);
        }
    }

    /// Puts `page`, just read, in a frame for `id`.
    fn load(
        &self,
        mut inner: std::sync::MutexGuard<'_, Inner>,
        id: PageId,
        page: Page,
    ) -> Result<PageRef<'_>> {
        let frame = self.victim(&mut inner)?;
        *self
            .frame(frame)
            .page
            .write()
            .unwrap_or_else(|e| e.into_inner()) = page;
        self.install(&mut inner, frame, id, false);
        Ok(self.pin(frame, id))
    }

    /// Allocates a new page of type `ty` and pins it.  The page is dirty: it
    /// reaches the file on eviction or flush.
    pub fn create(&self, ty: PageType) -> Result<PageRef<'_>> {
        let id = self.pager.allocate()?;
        let mut inner = self.lock(id);
        let frame = match self.victim(&mut inner) {
            Ok(frame) => frame,
            Err(e) => {
                drop(inner);
                let _ = self.pager.free(id);
                return Err(e);
            }
        };
        *self
            .frame(frame)
            .page
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Page::new(ty);
        self.install(&mut inner, frame, id, true);
        Ok(self.pin(frame, id))
    }

    fn install(&self, inner: &mut Inner, frame: usize, id: PageId, dirty: bool) {
        inner.table.insert(id, frame);
        *inner.id(frame) = Some(id);
        self.frame(frame).dirty.store(dirty, Ordering::Release);
    }

    /// Frees page `id` (see [`Pager::free`]) and drops it from the cache.
    /// The page must not be pinned.
    pub fn free(&self, id: PageId) -> Result<()> {
        let mut inner = self.lock(id);
        if let Some(&frame) = inner.table.get(&id) {
            if self.frame(frame).pins.load(Ordering::Acquire) != 0 {
                return Err(Error::Pinned(id));
            }
            inner.table.remove(&id);
            *inner.id(frame) = None;
            self.frame(frame).dirty.store(false, Ordering::Release);
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
        let (start, end) = (inner.start, inner.end);
        // Two sweeps: the first may only clear reference bits.
        for _ in 0..2 * (end - start) {
            let frame = inner.hand;
            inner.hand = if frame + 1 == end { start } else { frame + 1 };
            let f = self.frame(frame);
            // Pins only grow under the shard's lock, which we hold, so a zero
            // seen here stays zero until we are done.
            if f.pins.load(Ordering::Acquire) != 0 {
                continue;
            }
            if f.referenced.swap(false, Ordering::Relaxed) {
                continue;
            }
            let id = inner
                .id(frame)
                .expect("a frame outside the free list holds a page");
            if f.dirty.load(Ordering::Acquire) {
                let mut page = f.page.write().unwrap_or_else(|e| e.into_inner());
                self.pager.write_page(id, &mut page)?;
                f.dirty.store(false, Ordering::Release);
                self.writebacks.fetch_add(1, Ordering::Relaxed);
            }
            inner.table.remove(&id);
            *inner.id(frame) = None;
            self.evictions.fetch_add(1, Ordering::Relaxed);
            return Ok(frame);
        }
        Err(Error::PoolExhausted)
    }

    /// Writes every dirty page to the pager.  Pages stay cached.
    pub fn flush(&self) -> Result<()> {
        for shard in &*self.shards {
            let inner = shard.lock().unwrap_or_else(|e| e.into_inner());
            self.flush_shard(&inner)?;
        }
        Ok(())
    }

    fn flush_shard(&self, inner: &Inner) -> Result<()> {
        for (i, id) in inner.ids.iter().enumerate() {
            let (Some(id), f) = (id, self.frame(inner.start + i)) else {
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
