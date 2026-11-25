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
//! reads the page without any lock.
//!
//! A hit takes no lock and writes nothing every thread writes: it finds
//! the frame in the shard's index ([`Shard::find`], atomic words read,
//! none written), pins it, and checks the frame still holds the page
//! ([`Frame::owner`]) — an eviction clears the owner before it looks at
//! the pins, and a hit pins before it looks at the owner, so one of the
//! two sees the other (both sequentially consistent).  The hits are
//! counted on a line of the thread's own ([`Stripes`]).  A lookup's
//! threads all descend from the same root: its shard's lock and counter,
//! written by every hit, were a third of ten threads' time.
//!
//! An internal node a thread descends through often is read from a copy
//! of its own ([`BufferPool::read_copy`]), not pinned and latched — the
//! pin and the latch of the root, written by every descent, are the next
//! lines every thread writes.  The copy is good while the frame holds
//! the same page unchanged: its owner and its version ([`Frame::version`],
//! moved by every write latch and every page put in it) as when copied,
//! two loads.  A tree's pages change only under its writer's lock, which
//! its readers exclude, so a copy found good stays so while a reader
//! holds the tree.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::PAGE_SIZE;
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
    /// Its page, read with a run of others ([`Page::of_run`]), keeps the
    /// run's buffer alive with half or more of them gone: to be copied
    /// out at its next hit, the buffer then left to the rest (a page in
    /// use kept, the scan's others evicted, holds 8 KiB, not 128).
    unshare: AtomicBool,
    /// The page it holds, [`NONE`] while it holds none or is being
    /// emptied: what a hit found in the index checks after pinning it.
    owner: AtomicU64,
    /// Moved by every write latch taken and every page put in it: a
    /// thread's copy of its page is good while it is the same.
    version: AtomicU64,
}

/// [`Frame::owner`] of a frame that holds no page.
const NONE: u64 = u64::MAX;

impl Frame {
    fn new(page: Page) -> Self {
        Frame {
            page: RwLock::new(page),
            pins: AtomicU32::new(0),
            dirty: AtomicBool::new(false),
            referenced: AtomicBool::new(false),
            unshare: AtomicBool::new(false),
            owner: AtomicU64::new(NONE),
            version: AtomicU64::new(0),
        }
    }
}

/// A thread's copies of internal nodes ([`BufferPool::read_copy`]), by a
/// hash of their pool and page: a few, so a thread's memory stays small
/// (a tree's root and the nodes under it are what every descent reads).
const COPIES: usize = 16;

/// A copy, or a page seen once (copied when seen again unchanged).
struct Copy {
    pool: u64,
    id: PageId,
    frame: usize,
    version: u64,
    page: Option<Box<Page>>,
}

thread_local! {
    static THREAD_COPIES: std::cell::RefCell<Vec<Option<Copy>>> =
        std::cell::RefCell::new((0..COPIES).map(|_| None).collect());
}

/// A number of its own for each pool, so copies of another pool's pages
/// are not taken for its.
static POOLS: AtomicU64 = AtomicU64::new(0);

/// A shard: its lock, guarding what it holds, and an index of its pages
/// a hit reads without the lock.
#[repr(align(128))]
struct Shard {
    inner: RwLock<Inner>,
    /// Its first frame (by global index).
    start: usize,
    /// Open addressing, linear probing: a page's entry is `(id + 1) << 24
    /// | frame - start`, [`EMPTY`] or [`GONE`] — written under the lock
    /// held for writing, read without it.  Only a guide: an entry read
    /// while it changes, or one gone stale, is caught by the frame's
    /// owner, and a page not found is looked for under the lock.  A page
    /// id or a frame too large to encode is not entered.
    index: Box<[AtomicU64]>,
}

const EMPTY: u64 = 0;
const GONE: u64 = u64::MAX;
const FRAME_BITS: u32 = 24;

impl Shard {
    /// Where `id`'s probe starts.
    fn home(&self, id: PageId) -> usize {
        (id.0.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29) as usize) & (self.index.len() - 1)
    }

    /// The entry of `id`, `None` if it cannot have one.
    fn entry(&self, id: PageId, frame: usize) -> Option<u64> {
        let off = (frame - self.start) as u64;
        (id.0 < (1 << (64 - FRAME_BITS)) - 2 && off < 1 << FRAME_BITS)
            .then(|| (id.0 + 1) << FRAME_BITS | off)
    }

    /// The frame the index says holds `id`, without the lock.
    fn find(&self, id: PageId) -> Option<usize> {
        let key = id.0.wrapping_add(1);
        let mask = self.index.len() - 1;
        let mut at = self.home(id);
        for _ in 0..self.index.len() {
            let e = self.index[at].load(Ordering::Acquire);
            if e == EMPTY {
                return None;
            }
            if e != GONE && e >> FRAME_BITS == key {
                return Some(self.start + (e & ((1 << FRAME_BITS) - 1)) as usize);
            }
            at = (at + 1) & mask;
        }
        None
    }

    /// Enters `id` in `frame`; the lock held for writing.
    fn enter(&self, inner: &mut Inner, id: PageId, frame: usize) {
        let Some(e) = self.entry(id, frame) else {
            return;
        };
        let mask = self.index.len() - 1;
        let mut at = self.home(id);
        loop {
            let old = self.index[at].load(Ordering::Relaxed);
            if old == EMPTY || old == GONE {
                if old == GONE {
                    inner.gone -= 1;
                }
                self.index[at].store(e, Ordering::Release);
                return;
            }
            at = (at + 1) & mask;
        }
    }

    /// Takes `id` out; the lock held for writing.
    fn leave(&self, inner: &mut Inner, id: PageId) {
        let key = id.0.wrapping_add(1);
        let mask = self.index.len() - 1;
        let mut at = self.home(id);
        for _ in 0..self.index.len() {
            let e = self.index[at].load(Ordering::Relaxed);
            if e == EMPTY {
                break;
            }
            if e != GONE && e >> FRAME_BITS == key {
                self.index[at].store(GONE, Ordering::Release);
                inner.gone += 1;
                break;
            }
            at = (at + 1) & mask;
        }
        // Too many marks lengthen the probes: entered again from the table
        // (a hit meanwhile may miss, and is looked for under the lock).
        if inner.gone > self.index.len() / 4 {
            for e in &*self.index {
                e.store(EMPTY, Ordering::Release);
            }
            inner.gone = 0;
            let pages: Vec<(PageId, usize)> = inner.table.iter().map(|(&i, &f)| (i, f)).collect();
            for (i, f) in pages {
                self.enter(inner, i, f);
            }
        }
    }
}

/// A counter each thread adds to on a line of its own (by its stripe), so
/// counting writes no line other threads write.
struct Stripes(Box<[Stripe]>);

#[repr(align(128))]
#[derive(Default)]
struct Stripe(AtomicU64);

const STRIPES: usize = 32;

impl Stripes {
    fn new() -> Self {
        Stripes((0..STRIPES).map(|_| Stripe::default()).collect())
    }

    fn add(&self, n: u64) {
        thread_local! {
            static STRIPE: usize = {
                static NEXT: AtomicUsize = AtomicUsize::new(0);
                NEXT.fetch_add(1, Ordering::Relaxed) % STRIPES
            };
        }
        let at = STRIPE.with(|s| *s);
        self.0[at].0.fetch_add(n, Ordering::Relaxed);
    }

    fn sum(&self) -> u64 {
        self.0.iter().map(|s| s.0.load(Ordering::Relaxed)).sum()
    }
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
    /// [`GONE`] marks in the shard's index.
    gone: usize,
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

/// Pages a checkpoint writes at once at most ([`BufferPool::flush_frozen`]):
/// 1 MiB.
const RUN_PAGES: usize = 128;

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

/// The bytes of the frames the process's buffer pools have made (each
/// made on first use, kept until its pool goes): the page cache's part of
/// the memory budget it shares with the LSM trees' block cache, which
/// takes what is left (issue #11/#13, [`crate::lsm`]'s `BlockCache`).
pub(crate) static FRAME_BYTES: AtomicUsize = AtomicUsize::new(0);

/// A page cache over a [`Pager`].
pub struct BufferPool {
    pager: Arc<Pager>,
    /// Each made when first used, so a large pool costs nothing until it
    /// fills.
    frames: Box<[std::sync::OnceLock<Frame>]>,
    /// Locked for reading to find a page and pin it (a hit), for writing
    /// to change what the shard holds.
    shards: Box<[Shard]>,
    /// Its number ([`POOLS`]).
    uid: u64,
    hits: Stripes,
    misses: AtomicU64,
    evictions: AtomicU64,
    writebacks: AtomicU64,
    /// Held by a flush: two at once could write a page's copies in the
    /// wrong order (the shards' read locks keep only evictions out).
    flushing: Mutex<()>,
    /// Frozen pages not yet written that were freed while a commit is in
    /// flight — from its freeze ([`hold_freed_frozen`]) to the end of
    /// [`flush_frozen`] (whether they are taken, and their contents):
    /// written with the others, in the file's order, where each was a
    /// write of its own on the writer's thread (issue #9: a large index
    /// build's checkpoints, the build's last leaves and their parents
    /// replaced as it goes on).
    ///
    /// [`hold_freed_frozen`]: Self::hold_freed_frozen
    /// [`flush_frozen`]: Self::flush_frozen
    orphans: Mutex<(bool, Vec<(PageId, Page)>)>,
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
        let page = self.frame().page.read().unwrap_or_else(|e| e.into_inner());
        if page.shares_sparsely() {
            self.frame().unshare.store(true, Ordering::Relaxed);
        }
        page
    }

    /// Latches the page for writing and marks it dirty.  Fails with
    /// [`Error::Immutable`] if the page belongs to the durable state.
    pub fn write(&self) -> Result<RwLockWriteGuard<'_, Page>> {
        if !self.pool.pager.is_writable(self.id) {
            return Err(Error::Immutable(self.id));
        }
        let guard = self.frame().page.write().unwrap_or_else(|e| e.into_inner());
        self.frame().version.fetch_add(1, Ordering::Release);
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

impl Drop for BufferPool {
    fn drop(&mut self) {
        FRAME_BYTES.fetch_sub(self.frames_made() * PAGE_SIZE, Ordering::Relaxed);
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
                        gone: 0,
                    }),
                    start,
                    index: (0..(2 * (end - start)).next_power_of_two())
                        .map(|_| AtomicU64::new(EMPTY))
                        .collect(),
                }
            })
            .collect();
        Self {
            pager,
            frames,
            shards,
            uid: POOLS.fetch_add(1, Ordering::Relaxed),
            hits: Stripes::new(),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            writebacks: AtomicU64::new(0),
            flushing: Mutex::new(()),
            orphans: Mutex::new((false, Vec::new())),
        }
    }

    /// The pager underneath.
    pub fn pager(&self) -> &Arc<Pager> {
        &self.pager
    }

    /// Frame `i`, made on first use.
    fn frame(&self, i: usize) -> &Frame {
        self.frames[i].get_or_init(|| {
            FRAME_BYTES.fetch_add(PAGE_SIZE, Ordering::Relaxed);
            Frame::new(Page::new(PageType::UNUSED))
        })
    }

    /// Puts `page` in frame `i` — the frame made with it on first use, so
    /// no empty page is allocated (and zeroed) only to be replaced: after
    /// a start every frame a cold scan fills is a first use.
    fn put(&self, i: usize, page: Page) -> &Frame {
        let mut page = Some(page);
        let f = self.frames[i].get_or_init(|| {
            FRAME_BYTES.fetch_add(PAGE_SIZE, Ordering::Relaxed);
            Frame::new(page.take().expect("not taken yet"))
        });
        if let Some(page) = page {
            *f.page.write().unwrap_or_else(|e| e.into_inner()) = page;
        }
        f.unshare.store(false, Ordering::Relaxed);
        f
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
            hits: self.hits.sum(),
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
        f.pins.fetch_add(1, Ordering::SeqCst);
        self.pinned(f, frame, id)
    }

    fn pinned(&self, f: &Frame, frame: usize, id: PageId) -> PageRef<'_> {
        // Written only when it changes: a hot page's frame is not written
        // by every thread that pins it.
        if !f.referenced.load(Ordering::Relaxed) {
            f.referenced.store(true, Ordering::Relaxed);
        }
        if f.unshare.load(Ordering::Relaxed)
            && let Ok(mut page) = f.page.try_write()
        {
            page.unshare();
            f.unshare.store(false, Ordering::Relaxed);
        }
        PageRef {
            pool: self,
            frame,
            id,
        }
    }

    /// Page `id` pinned if the shard's index finds it — no lock taken.
    fn hit(&self, shard: &Shard, id: PageId) -> Option<PageRef<'_>> {
        let frame = shard.find(id)?;
        let f = self.frames.get(frame)?.get()?;
        f.pins.fetch_add(1, Ordering::SeqCst);
        if f.owner.load(Ordering::SeqCst) != id.0 {
            // Emptied, or another page's now.
            f.pins.fetch_sub(1, Ordering::Release);
            return None;
        }
        self.hits.add(1);
        Some(self.pinned(f, frame, id))
    }

    /// Pins page `id`, reading it from the pager on a miss.
    pub fn fetch(&self, id: PageId) -> Result<PageRef<'_>> {
        let shard = self.shard(id);
        if let Some(r) = self.hit(shard, id) {
            return Ok(r);
        }
        loop {
            let epoch = {
                // Not in the index (or found changing): under the lock.
                let inner = shard.read().unwrap_or_else(|e| e.into_inner());
                if let Some(&frame) = inner.table.get(&id) {
                    self.hits.add(1);
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
                self.hits.add(1);
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

    /// `read` of this thread's copy of page `id`, if it has one still
    /// good ([`remember`](Self::remember)); `None` otherwise — the page
    /// is then to be fetched.  The caller holds the page's tree for
    /// reading.
    pub fn read_copy<R>(&self, id: PageId, read: impl FnOnce(&Page) -> R) -> Option<R> {
        THREAD_COPIES.with(|c| {
            let c = c.borrow();
            let e = c[self.slot(id)].as_ref()?;
            if e.pool != self.uid || e.id != id {
                return None;
            }
            let page = e.page.as_deref()?;
            let f = self.frames.get(e.frame)?.get()?;
            if f.owner.load(Ordering::Acquire) != id.0
                || f.version.load(Ordering::Acquire) != e.version
            {
                return None;
            }
            if !f.referenced.load(Ordering::Relaxed) {
                f.referenced.store(true, Ordering::Relaxed);
            }
            self.hits.add(1);
            Some(read(page))
        })
    }

    /// Notes `page`, pinned by `r` and latched for reading, as one this
    /// thread reads often (an internal node): copied for
    /// [`read_copy`](Self::read_copy) the second time it is seen
    /// unchanged — a page changed between two descents (a writer's) is
    /// not copied each time.
    pub fn remember(&self, r: &PageRef<'_>, page: &Page) {
        let id = r.id;
        let version = r.frame().version.load(Ordering::Acquire);
        let at = self.slot(id);
        THREAD_COPIES.with(|c| {
            let mut c = c.borrow_mut();
            match &mut c[at] {
                Some(e)
                    if e.pool == self.uid
                        && e.id == id
                        && e.frame == r.frame
                        && e.version == version =>
                {
                    if e.page.is_none() {
                        e.page = Some(Box::new(page.copied()));
                    }
                }
                slot => {
                    *slot = Some(Copy {
                        pool: self.uid,
                        id,
                        frame: r.frame,
                        version,
                        page: None,
                    });
                }
            }
        });
    }

    /// The slot of page `id`'s copy.
    fn slot(&self, id: PageId) -> usize {
        ((id.0 ^ self.uid.rotate_left(32)).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as usize
            % COPIES
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
            self.put(frame, page)
                .referenced
                .store(false, Ordering::Relaxed);
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
        self.put(frame, page);
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
        self.put(frame, Page::new(ty));
        self.install(&mut inner, frame, id, true);
        Ok(self.pin(frame, id))
    }

    fn install(&self, inner: &mut Inner, frame: usize, id: PageId, dirty: bool) {
        inner.table.insert(id, frame);
        *inner.id(frame) = Some(id);
        let f = self.frame(frame);
        f.dirty.store(dirty, Ordering::Release);
        f.version.fetch_add(1, Ordering::Release);
        f.owner.store(id.0, Ordering::SeqCst);
        self.shard(id).enter(inner, id, frame);
    }

    /// Empties `frame` of `id` unless a hit pinned it meanwhile (whether
    /// it did): the owner cleared first, then the pins looked at — a hit
    /// pins first, then looks at the owner.  The shard's lock held for
    /// writing, `frame` seen unpinned.
    fn empty(&self, frame: usize, id: PageId) -> bool {
        let f = self.frame(frame);
        f.owner.store(NONE, Ordering::SeqCst);
        if f.pins.load(Ordering::SeqCst) != 0 {
            f.owner.store(id.0, Ordering::SeqCst);
            return false;
        }
        true
    }

    /// Frees page `id` (see [`Pager::free`]) and drops it from the cache.
    /// The page must not be pinned.
    pub fn free(&self, id: PageId) -> Result<()> {
        self.uncache(id)?;
        self.pager.free(id)
    }

    /// Frees page `id` for `branch` ([`Pager::release`]: a page shared with
    /// other branches stays, and stays cached) and drops it from the
    /// cache.  The page must not be pinned (unless it stays).
    pub fn free_in(&self, id: PageId, branch: u32) -> Result<()> {
        if !self.pager.needs_birth(branch) {
            return self.free(id);
        }
        let birth = self.birth(id)?;
        if !self.pager.keeps(id, branch, birth) {
            self.uncache(id)?;
        }
        self.pager.release(id, branch, birth)
    }

    /// The commit page `id` was (or will be) born in: the pager knows a
    /// page not durable yet; a durable one's header says.
    pub fn birth(&self, id: PageId) -> Result<u64> {
        if let Some(b) = self.pager.birth(id) {
            return Ok(b);
        }
        let r = self.fetch(id)?;
        let b = r.read().lsn();
        Ok(b)
    }

    /// Drops page `id` from the cache (written back first if a commit in
    /// flight still needs it).
    fn uncache(&self, id: PageId) -> Result<()> {
        let mut inner = self.lock(id);
        if let Some(&frame) = inner.table.get(&id) {
            let f = self.frame(frame);
            if f.pins.load(Ordering::Acquire) != 0 || !self.empty(frame, id) {
                return Err(Error::Pinned(id));
            }
            // A page of a commit in flight, copied since: the frozen state
            // still holds it, so its contents go to the file first — with
            // the checkpoint's other pages where it is writing them (the
            // page stays the pager's to write until that commit: freed,
            // it is pending).
            if f.dirty.load(Ordering::Acquire) && self.pager.is_frozen(id) {
                let mut orphans = self.orphans.lock().unwrap_or_else(|e| e.into_inner());
                if orphans.0 {
                    let page = f.page.read().unwrap_or_else(|e| e.into_inner()).clone();
                    orphans.1.push((id, page));
                } else {
                    drop(orphans);
                    let mut page = f.page.write().unwrap_or_else(|e| e.into_inner());
                    if let Err(e) = self.pager.write_page(id, &mut page) {
                        // Not emptied after all.
                        f.owner.store(id.0, Ordering::SeqCst);
                        return Err(e);
                    }
                    self.writebacks.fetch_add(1, Ordering::Relaxed);
                }
                f.dirty.store(false, Ordering::Release);
            }
            inner.table.remove(&id);
            self.shard(id).leave(&mut inner, id);
            *inner.id(frame) = None;
            self.frame(frame).dirty.store(false, Ordering::Release);
            inner.free_frames.push(frame);
        }
        Ok(())
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
            if f.pins.load(Ordering::Acquire) != 0 {
                continue;
            }
            if f.referenced.swap(false, Ordering::Relaxed) {
                continue;
            }
            let id = inner
                .id(frame)
                .expect("a frame outside the free list holds a page");
            // A hit pins without the lock: once emptied, none can.
            if !self.empty(frame, id) {
                continue;
            }
            if f.dirty.load(Ordering::Acquire) {
                let mut page = f.page.write().unwrap_or_else(|e| e.into_inner());
                if let Err(e) = self.pager.write_page(id, &mut page) {
                    // Not emptied after all.
                    f.owner.store(id.0, Ordering::SeqCst);
                    return Err(e);
                }
                f.dirty.store(false, Ordering::Release);
                self.writebacks.fetch_add(1, Ordering::Relaxed);
            }
            inner.table.remove(&id);
            self.shard(id).leave(inner, id);
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

    /// From now until [`flush_frozen`](Self::flush_frozen) ends, frozen
    /// pages freed before they are written are kept for it to write (a
    /// commit just frozen, its writers still held).
    pub fn hold_freed_frozen(&self) {
        self.orphans.lock().unwrap_or_else(|e| e.into_inner()).0 = true;
    }

    /// Writes the dirty pages of the commit in flight ([`Pager::freeze`])
    /// to the pager — while writers go on, copying them as they change
    /// them.  Pages stay cached.
    ///
    /// In the file's order, a run of neighbouring pages at a time
    /// ([`Pager::write_run`]; PostgreSQL's checkpointer sorts its buffers
    /// so): a page at a time in the cache's order was a random write each,
    /// ~28 µs, and a load's checkpoints write gigabytes (PLAN.md §M14.33
    /// R4).  A frozen page does not change — a writer copies it — so a
    /// copy taken under its shard's lock is what the file needs, and its
    /// dirty flag is cleared only once the copy is written: an eviction
    /// meanwhile writes the same contents again rather than dropping a
    /// page not yet in the file.
    pub fn flush_frozen(&self) -> Result<()> {
        let _one = self.flushing.lock().unwrap_or_else(|e| e.into_inner());
        self.orphans.lock().unwrap_or_else(|e| e.into_inner()).0 = true;
        let flushed = self.flush_frozen_cached();
        // The pages freed meanwhile, taken no more (one freed from now on
        // is written as it goes).
        let mut orphans = {
            let mut o = self.orphans.lock().unwrap_or_else(|e| e.into_inner());
            o.0 = false;
            std::mem::take(&mut o.1)
        };
        flushed?;
        orphans.sort_unstable_by_key(|(id, _)| *id);
        let mut run: Vec<Page> = Vec::with_capacity(RUN_PAGES);
        let mut first = PageId(0);
        for (id, page) in orphans {
            if !run.is_empty() && (id.0 != first.0 + run.len() as u64 || run.len() == RUN_PAGES) {
                self.pager.write_run(first, &mut run)?;
                self.writebacks
                    .fetch_add(run.len() as u64, Ordering::Relaxed);
                run.clear();
            }
            if run.is_empty() {
                first = id;
            }
            run.push(page);
        }
        if !run.is_empty() {
            self.pager.write_run(first, &mut run)?;
            self.writebacks
                .fetch_add(run.len() as u64, Ordering::Relaxed);
        }
        Ok(())
    }

    /// [`flush_frozen`](Self::flush_frozen)'s pages still cached.
    fn flush_frozen_cached(&self) -> Result<()> {
        let mut ids = Vec::new();
        for shard in &*self.shards {
            let inner = shard.read().unwrap_or_else(|e| e.into_inner());
            for (i, id) in inner.ids.iter().enumerate() {
                if let Some(id) = *id
                    && self.frame(inner.start + i).dirty.load(Ordering::Acquire)
                {
                    ids.push(id);
                }
            }
        }
        self.pager.retain_frozen(&mut ids);
        ids.sort_unstable();
        let mut run: Vec<Page> = Vec::with_capacity(RUN_PAGES);
        let mut first = PageId(0);
        for id in ids {
            if !run.is_empty() && (id.0 != first.0 + run.len() as u64 || run.len() == RUN_PAGES) {
                self.write_frozen_run(first, &mut run)?;
            }
            // Its copy, unless it left the cache or was written meanwhile
            // (an eviction's, an `uncache`'s).
            let copy = {
                let inner = self.shard(id).read().unwrap_or_else(|e| e.into_inner());
                inner.table.get(&id).and_then(|&frame| {
                    let f = self.frame(frame);
                    f.dirty
                        .load(Ordering::Acquire)
                        .then(|| f.page.read().unwrap_or_else(|e| e.into_inner()).clone())
                })
            };
            let Some(copy) = copy else {
                self.write_frozen_run(first, &mut run)?;
                continue;
            };
            if run.is_empty() {
                first = id;
            }
            run.push(copy);
        }
        self.write_frozen_run(first, &mut run)
    }

    /// Writes `run` — the frozen pages from `first` on — then marks the
    /// cached pages clean, with the birth the pager stamped (§M14.13: it
    /// is fixed when a page is allocated, so it is the cached page's too).
    /// Empties `run`.
    fn write_frozen_run(&self, first: PageId, run: &mut Vec<Page>) -> Result<()> {
        if run.is_empty() {
            return Ok(());
        }
        self.pager.write_run(first, run)?;
        for (i, page) in run.iter().enumerate() {
            let id = PageId(first.0 + i as u64);
            let inner = self.shard(id).read().unwrap_or_else(|e| e.into_inner());
            if let Some(&frame) = inner.table.get(&id) {
                let f = self.frame(frame);
                f.page
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .set_lsn(page.lsn());
                f.dirty.store(false, Ordering::Release);
            }
        }
        self.writebacks
            .fetch_add(run.len() as u64, Ordering::Relaxed);
        run.clear();
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
                // The birth the pager stamped (§M14.13), in the cached
                // page too: it is fixed when the page is allocated, so a
                // write since cannot have made it another.
                f.page
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .set_lsn(copy.lsn());
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
