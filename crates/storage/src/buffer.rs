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
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::error::{Error, Result};
use crate::page::{Page, PageId, PageType};
use crate::pager::Pager;

/// A frame, and a shard ([`Shard`]), take a cache line of their own (128
/// bytes on Apple's cores; two lines of 64 elsewhere, which adjacent-line
/// prefetch fetches together): threads pinning neighbouring frames, or
/// finding pages in neighbouring shards, do not write the same line.
#[repr(align(128))]
struct Frame {
    page: RwLock<Page>,
    pins: AtomicU32,
    dirty: AtomicBool,
    referenced: AtomicBool,
}

/// A shard's lock, and the hits found in it — counted on the line the
/// lock already writes, not on one every thread's hits write.
#[repr(align(128))]
struct Shard {
    inner: RwLock<Inner>,
    hits: AtomicU64,
}

impl std::ops::Deref for Shard {
    type Target = RwLock<Inner>;

    fn deref(&self) -> &RwLock<Inner> {
        &self.inner
    }
}

/// One shard: frames `start..end` (by global index) and the pages in them.
struct Inner {
    start: usize,
    end: usize,
    table: HashMap<PageId, usize, PageHash>,
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

/// A page id's hash: one multiplication (a page table is looked up on
/// every fetch; SipHash's resistance to chosen keys buys nothing here).
#[derive(Clone, Copy, Default)]
struct PageHash;

impl std::hash::BuildHasher for PageHash {
    type Hasher = PageHasher;
    fn build_hasher(&self) -> PageHasher {
        PageHasher(0)
    }
}

#[derive(Default)]
struct PageHasher(u64);

impl std::hash::Hasher for PageHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ u64::from(b)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_u64(&mut self, x: u64) {
        self.0 = (self.0 ^ x).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 ^= self.0 >> 29;
    }
}

/// Frames per shard at least, and shards at most.
const SHARD_FRAMES: usize = 256;
const MAX_SHARDS: usize = 16;

/// Pages read at once ahead of a scan ([`BufferPool::read_ahead`]): 128 KiB.
pub const READ_AHEAD: usize = 16;

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
    /// Locked for reading to find a page and pin it (a hit), for writing
    /// to change what the shard holds.
    shards: Box<[Shard]>,
    misses: AtomicU64,
    evictions: AtomicU64,
    writebacks: AtomicU64,
    /// Held by a flush: two at once could write a page's copies in the
    /// wrong order (the shards' read locks keep only evictions out).
    flushing: Mutex<()>,
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
                Shard {
                    inner: RwLock::new(Inner {
                        start,
                        end,
                        table: HashMap::default(),
                        ids: vec![None; end - start],
                        free_frames: (start..end).rev().collect(),
                        hand: start,
                    }),
                    hits: AtomicU64::new(0),
                }
            })
            .collect();
        Self {
            pager,
            frames,
            shards,
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            writebacks: AtomicU64::new(0),
            flushing: Mutex::new(()),
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

    /// The frames made so far: each is made when a page first takes it.
    pub fn frames_made(&self) -> usize {
        self.frames.iter().filter(|f| f.get().is_some()).count()
    }

    /// The counters so far.
    pub fn stats(&self) -> PoolStats {
        PoolStats {
            hits: self
                .shards
                .iter()
                .map(|s| s.hits.load(Ordering::Relaxed))
                .sum(),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            writebacks: self.writebacks.load(Ordering::Relaxed),
        }
    }

    /// The shard of page `id`.
    fn shard(&self, id: PageId) -> &Shard {
        let n = self.shards.len();
        let at = if n == 1 {
            0
        } else {
            // Neighbouring pages (a scan's) to different shards.
            (id.0.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize % n
        };
        &self.shards[at]
    }

    /// The shard of page `id`, locked to change it.
    fn lock(&self, id: PageId) -> RwLockWriteGuard<'_, Inner> {
        self.shard(id).write().unwrap_or_else(|e| e.into_inner())
    }

    fn pin(&self, frame: usize, id: PageId) -> PageRef<'_> {
        let f = self.frame(frame);
        f.pins.fetch_add(1, Ordering::Acquire);
        // Written only when it changes: a hot page's frame is not written
        // by every thread that pins it.
        if !f.referenced.load(Ordering::Relaxed) {
            f.referenced.store(true, Ordering::Relaxed);
        }
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
                // A hit only reads the shard: many threads find their pages
                // in it at once (a parallel join's lookups all descend from
                // the same root).  Pins grow under either lock, and an
                // eviction, which takes it for writing, sees them.
                let shard = self.shard(id);
                let inner = shard.read().unwrap_or_else(|e| e.into_inner());
                if let Some(&frame) = inner.table.get(&id) {
                    shard.hits.fetch_add(1, Ordering::Relaxed);
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
                self.shard(id).hits.fetch_add(1, Ordering::Relaxed);
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

    /// Whether page `id` is in a frame.
    pub fn contains(&self, id: PageId) -> bool {
        self.shard(id)
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .table
            .contains_key(&id)
    }

    /// Reads ahead: the run of consecutive page ids at the start of `ids`
    /// (at most [`READ_AHEAD`]; the caller found `ids[0]` not cached), with
    /// one read of the file, into frames — those not cached meanwhile,
    /// unpinned and not yet referenced, so the clock takes them first if
    /// they go unused.  PostgreSQL's read stream combines a scan's
    /// neighbouring blocks into one read the same way (`io_combine_limit`,
    /// 128 kB).  Only an optimisation: on any failure nothing more is
    /// cached, and each page's own fetch reads (and reports) it.
    pub fn read_ahead(&self, ids: &[PageId]) {
        let Some(&first) = ids.first() else { return };
        let n = ids
            .iter()
            .take(READ_AHEAD)
            .enumerate()
            .take_while(|&(k, &id)| id.0 == first.0 + k as u64)
            .count();
        if n < 2 {
            return;
        }
        let epoch = self.writebacks.load(Ordering::Acquire);
        let Ok(pages) = self.pager.read_pages(first, n) else {
            return;
        };
        for (page, id) in pages.into_iter().zip(first.0..) {
            let id = PageId(id);
            let mut inner = self.lock(id);
            if self.writebacks.load(Ordering::Acquire) != epoch {
                // A page went back to the file meanwhile: what was read may
                // be older than it.
                return;
            }
            if inner.table.contains_key(&id) {
                continue;
            }
            let Ok(frame) = self.victim(&mut inner) else {
                return;
            };
            let f = self.frame(frame);
            *f.page.write().unwrap_or_else(|e| e.into_inner()) = page;
            f.referenced.store(false, Ordering::Relaxed);
            self.install(&mut inner, frame, id, false);
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Puts `page`, just read, in a frame for `id`.
    fn load(
        &self,
        mut inner: RwLockWriteGuard<'_, Inner>,
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
            let f = self.frame(frame);
            if f.pins.load(Ordering::Acquire) != 0 {
                return Err(Error::Pinned(id));
            }
            // A page of a commit in flight, copied since: the frozen state
            // still holds it, so its contents go to the file first.
            if f.dirty.load(Ordering::Acquire) && self.pager.is_frozen(id) {
                let mut page = f.page.write().unwrap_or_else(|e| e.into_inner());
                self.pager.write_page(id, &mut page)?;
                f.dirty.store(false, Ordering::Release);
                self.writebacks.fetch_add(1, Ordering::Relaxed);
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
            // Pins only grow under the shard's lock, which we hold for
            // writing, so a zero seen here stays zero until we are done.
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
        let _one = self.flushing.lock().unwrap_or_else(|e| e.into_inner());
        for shard in &*self.shards {
            self.flush_shard(shard, |_| true)?;
        }
        Ok(())
    }

    /// Writes the dirty pages of the commit in flight ([`Pager::freeze`])
    /// to the pager — while writers go on, copying them as they change
    /// them.  Pages stay cached.
    pub fn flush_frozen(&self) -> Result<()> {
        let _one = self.flushing.lock().unwrap_or_else(|e| e.into_inner());
        for shard in &*self.shards {
            self.flush_shard(shard, |id| self.pager.is_frozen(id))?;
        }
        Ok(())
    }

    /// Writes `shard`'s dirty pages that are `wanted` — a page at a time
    /// under the shard's read lock (PostgreSQL's checkpointer writes a
    /// buffer under a share lock on it the same way): hits, which read
    /// the shard too, go on meanwhile; a load or an eviction, which write
    /// it, wait for one page's write, not the shard's.  The read lock is
    /// what keeps an eviction from writing the same page meanwhile, a
    /// newer copy perhaps, which this older one would then overwrite.
    fn flush_shard(&self, shard: &Shard, wanted: impl Fn(PageId) -> bool) -> Result<()> {
        let (start, frames) = {
            let inner = shard.read().unwrap_or_else(|e| e.into_inner());
            (inner.start, inner.ids.len())
        };
        for i in 0..frames {
            let inner = shard.read().unwrap_or_else(|e| e.into_inner());
            // A slot without a page has no frame to look at — and
            // `frame` would make one: the whole cache, at the first
            // checkpoint (found fuzzing, PLAN.md §M14.29 S2).
            let Some(id) = inner.ids[i] else {
                continue;
            };
            let f = self.frame(start + i);
            if !f.dirty.load(Ordering::Acquire) || !wanted(id) {
                continue;
            }
            // Clear the flag before copying: a write that starts after the
            // copy sets it again and is flushed next time.
            if f.dirty.swap(false, Ordering::AcqRel) {
                let mut copy = f.page.read().unwrap_or_else(|e| e.into_inner()).clone();
                if let Err(e) = self.pager.write_page(id, &mut copy) {
                    f.dirty.store(true, Ordering::Release);
                    return Err(e);
                }
                self.writebacks.fetch_add(1, Ordering::Relaxed);
            }
            drop(inner);
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
