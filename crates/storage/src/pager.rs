//! The pager: page I/O, page allocation, and atomic commits of a
//! copy-on-write file.
//!
//! # The rule
//!
//! **The durable state is immutable.**  A page that the last committed
//! header can reach — a data page, or a page of the free list itself — is
//! never written again.  [`Pager::write_page`] accepts only pages
//! [allocated](Pager::allocate) since the last [`Pager::sync`]; everything
//! else is refused with [`Error::Immutable`].  Changing a durable page means
//! copying it to a new page and freeing the old one (copy-on-write), and a
//! freed durable page becomes reusable only after the next commit, when
//! nothing durable refers to it any more.
//!
//! So a crash at any moment leaves the last committed state exactly as it
//! was: every page it refers to is untouched, and everything written since
//! is in pages it does not refer to.  `sync` then makes a new state durable
//! with one atomic header write.
//!
//! # File layout
//!
//! Pages 0 and 1 are two copies of the file header; data pages start at
//! [`FIRST_DATA_PAGE`].  A commit writes the *older* copy with the next
//! sequence number, so a crash mid-write leaves the newer copy intact, and
//! opening picks the valid copy with the highest sequence number.
//!
//! Header payload (little-endian, after the common page header):
//!
//! | Offset | Size | Field |
//! |-------:|-----:|-------|
//! | 0  | 8 | magic `NUCLEUS\0` |
//! | 8  | 4 | format version ([`FORMAT_VERSION`]) |
//! | 12 | 4 | page size |
//! | 16 | 8 | sequence number |
//! | 24 | 8 | page count (header pages included) |
//! | 32 | 8 | first free-list page (0 = none) |
//! | 40 | 8 | number of free pages |
//! | 48 | 8 × [`META_SLOTS`] | meta slots, owned by the layers above |
//!
//! # The free list
//!
//! The free page numbers are stored in dedicated free-list pages (a chain;
//! each holds up to [`IDS_PER_FREELIST_PAGE`] numbers), rewritten as a whole
//! by every commit into pages the previous state does not use.  Pages that
//! held the previous free list are free in the new state.  Nothing leaks:
//! after a crash, every page is either reachable from the committed state,
//! on its free list, or beyond its page count.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use nucleus_platform::{File, FileSystem, OpenMode};

use crate::error::{Error, Result};
use crate::page::{
    PAGE_SIZE, PAYLOAD_SIZE, Page, PageId, PageType, read_u32, read_u64, write_u32, write_u64,
};

/// The on-disk format version this code writes.  Version 2 (§M14.20 V5)
/// added compressed values; this code reads version 1 too (a file's
/// header says 2 from its next commit on, so older builds refuse it rather
/// than misread it).
pub const FORMAT_VERSION: u32 = 2;

/// The oldest format version this code reads.
pub const OLDEST_FORMAT_VERSION: u32 = 1;

/// The first page that can hold data; 0 and 1 are the header copies.
pub const FIRST_DATA_PAGE: PageId = PageId(2);

/// How many meta slots the header has.  The layers above use them for the
/// few values that must change atomically with the data: a tree root, a
/// checkpoint position, a timestamp.
pub const META_SLOTS: usize = 16;

/// Free page numbers per free-list page.
pub const IDS_PER_FREELIST_PAGE: usize = (PAYLOAD_SIZE - 16) / 8;

const MAGIC: &[u8; 8] = b"NUCLEUS\0";

/// The file header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// Incremented by every commit.
    pub seq: u64,
    /// Pages in the file, header pages included.
    pub page_count: u64,
    /// First free-list page, or 0.
    pub freelist_head: u64,
    /// Number of free pages.
    pub free_count: u64,
    /// The meta slots.
    pub meta: [u64; META_SLOTS],
}

impl Header {
    fn new() -> Self {
        Self {
            seq: 0,
            page_count: FIRST_DATA_PAGE.0,
            freelist_head: 0,
            free_count: 0,
            meta: [0; META_SLOTS],
        }
    }

    fn encode(&self) -> Page {
        let mut page = Page::new(PageType::HEADER);
        let p = page.payload_mut();
        p[0..8].copy_from_slice(MAGIC);
        write_u32(p, 8, FORMAT_VERSION);
        write_u32(p, 12, PAGE_SIZE as u32);
        write_u64(p, 16, self.seq);
        write_u64(p, 24, self.page_count);
        write_u64(p, 32, self.freelist_head);
        write_u64(p, 40, self.free_count);
        for (i, m) in self.meta.iter().enumerate() {
            write_u64(p, 48 + 8 * i, *m);
        }
        page
    }

    fn decode(page: &Page) -> Result<Self> {
        let p = page.payload();
        if &p[0..8] != MAGIC {
            return Err(Error::NotADatabase("bad magic".into()));
        }
        let version = read_u32(p, 8);
        if !(OLDEST_FORMAT_VERSION..=FORMAT_VERSION).contains(&version) {
            return Err(Error::NotADatabase(format!(
                "format version {version}, this build reads {OLDEST_FORMAT_VERSION} to \
                 {FORMAT_VERSION}"
            )));
        }
        let page_size = read_u32(p, 12);
        if page_size as usize != PAGE_SIZE {
            return Err(Error::NotADatabase(format!(
                "page size {page_size}, this build uses {PAGE_SIZE}"
            )));
        }
        let mut meta = [0; META_SLOTS];
        for (i, m) in meta.iter_mut().enumerate() {
            *m = read_u64(p, 48 + 8 * i);
        }
        Ok(Self {
            seq: read_u64(p, 16),
            page_count: read_u64(p, 24),
            freelist_head: read_u64(p, 32),
            free_count: read_u64(p, 40),
            meta,
        })
    }
}

#[derive(Debug)]
struct State {
    /// The header the next commit will write (meta slots, page count).
    hdr: Header,
    /// The header as last committed.
    durable: Header,
    /// Free in the durable state: reusable now.
    free: BTreeSet<u64>,
    /// Durable pages freed since the last commit: free after the next one.
    pending: BTreeSet<u64>,
    /// The pages holding the durable free list: free after the next commit.
    freelist_pages: Vec<u64>,
    /// Allocated since the last commit (or freeze): the only pages that may
    /// be changed.
    allocated: BTreeSet<u64>,
    /// A commit in flight ([`Pager::freeze`]), until it is durable.
    frozen: Option<FrozenPages>,
    /// Pages the file holds at least: a write past them extends it, which
    /// happens under the state lock so a commit's extension never races
    /// one (and never cuts it short).
    known_len: u64,
    /// A sync failed; refuse everything.
    poisoned: bool,
}

/// The pages of a commit in flight.
#[derive(Debug)]
struct FrozenPages {
    /// Allocated before the freeze: the new state's pages, no longer to be
    /// changed (a writer copies them) but still to be written back.
    pages: BTreeSet<u64>,
    /// Durable pages freed before the freeze: free once it is durable.
    carry: BTreeSet<u64>,
    /// The pages its free list goes into.
    list_pages: Vec<u64>,
}

/// A state frozen by [`Pager::freeze`], for [`Pager::commit_frozen`]: its
/// header and its free list.
#[derive(Debug)]
pub struct Frozen {
    hdr: Header,
    list_pages: Vec<u64>,
    free: Vec<u64>,
}

/// Where every page of the file stands, from [`Pager::accounting`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Accounting {
    /// Pages in the file, header pages included.
    pub page_count: u64,
    /// Free and reusable now.
    pub free: BTreeSet<u64>,
    /// Freed since the last commit, reusable after the next.
    pub pending: BTreeSet<u64>,
    /// Holding the durable free list.
    pub freelist_pages: BTreeSet<u64>,
    /// Allocated since the last commit.
    pub allocated: BTreeSet<u64>,
    /// A commit in flight's ([`Pager::freeze`]): its pages, the durable
    /// pages it frees, its free list's pages.
    pub frozen: BTreeSet<u64>,
}

/// A database file as an array of pages.  Shareable between threads: page
/// reads and writes run in parallel, allocation and `sync` take a lock.
pub struct Pager {
    file: Box<dyn File>,
    state: Mutex<State>,
    /// The state's page count and poisoned flag, mirrored (changed under
    /// the state lock, read without it): a page read checks its id against
    /// them, so concurrent reads never meet on the lock (§M14.20 W5).  The
    /// count only grows; a reader learns a page's id from a tree published
    /// after the page was allocated, so it never sees a count too small.
    pages: AtomicU64,
    poisoned: AtomicBool,
}

impl std::fmt::Debug for Pager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.lock();
        f.debug_struct("Pager")
            .field("header", &s.hdr)
            .field("free", &s.free.len())
            .field("pending", &s.pending.len())
            .field("allocated", &s.allocated.len())
            .finish_non_exhaustive()
    }
}

impl Pager {
    /// Creates a new database file at `path`; fails if one exists.  The file
    /// and its directory entry are durable when this returns.
    ///
    /// Atomic: the file is built under a temporary name and renamed into
    /// place, so after a crash `path` either does not exist or is a complete,
    /// empty database — never a half-written header.
    pub fn create(fs: &dyn FileSystem, path: &Path) -> Result<Self> {
        if fs.exists(path)? {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{} already exists", path.display()),
            )));
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".creating");
        let tmp = PathBuf::from(tmp);
        match fs.remove_file(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let file = fs.open(&tmp, OpenMode::CreateNew)?;
        let mut hdr = Header::new();
        for _ in 0..2 {
            write_header(file.as_ref(), &hdr)?;
            hdr.seq += 1;
        }
        hdr.seq -= 1;
        file.sync()?;
        fs.rename(&tmp, path)?;
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
        fs.sync_dir(dir.unwrap_or(Path::new(".")))?;
        Ok(Self::with(file, hdr, BTreeSet::new(), Vec::new()))
    }

    /// Opens the database file at `path`.
    pub fn open(fs: &dyn FileSystem, path: &Path) -> Result<Self> {
        let file = fs.open(path, OpenMode::ReadWrite)?;
        let hdr = read_best_header(file.as_ref())?;
        let header_page = PageId(hdr.seq % 2);
        if hdr.page_count < FIRST_DATA_PAGE.0 {
            return Err(Error::Corrupt {
                page: header_page,
                reason: "header fields out of range",
            });
        }
        if file.size()? < hdr.page_count * PAGE_SIZE as u64 {
            return Err(Error::Corrupt {
                page: PageId(hdr.page_count - 1),
                reason: "file shorter than its header says",
            });
        }
        let (free, freelist_pages) = read_freelist(file.as_ref(), &hdr)?;
        Ok(Self::with(file, hdr, free, freelist_pages))
    }

    fn with(file: Box<dyn File>, hdr: Header, free: BTreeSet<u64>, fl: Vec<u64>) -> Self {
        let known_len = file.size().map_or(0, |n| n / PAGE_SIZE as u64);
        Self {
            pages: AtomicU64::new(hdr.page_count),
            poisoned: AtomicBool::new(false),
            file,
            state: Mutex::new(State {
                durable: hdr.clone(),
                hdr,
                free,
                pending: BTreeSet::new(),
                freelist_pages: fl,
                allocated: BTreeSet::new(),
                frozen: None,
                known_len,
                poisoned: false,
            }),
        }
    }

    /// Writes `page` at `id`; past the pages the file is known to hold,
    /// under the state lock (an extension of the file).
    fn write_at(&self, id: PageId, page: &mut Page) -> Result<()> {
        if id.0 < self.lock().known_len {
            return write_raw(self.file.as_ref(), id, page);
        }
        let mut s = self.lock();
        write_raw(self.file.as_ref(), id, page)?;
        s.known_len = s.known_len.max(id.0 + 1);
        Ok(())
    }

    /// Whether `id` may be written back: allocated since the last freeze,
    /// or frozen and not yet durable.
    fn may_write(s: &State, id: u64) -> bool {
        s.allocated.contains(&id) || s.frozen.as_ref().is_some_and(|f| f.pages.contains(&id))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The state lock, unless the pager is poisoned.
    fn begin(&self) -> Result<MutexGuard<'_, State>> {
        let s = self.lock();
        if s.poisoned {
            Err(Error::Poisoned)
        } else {
            Ok(s)
        }
    }

    fn check_data_page(s: &State, id: PageId) -> Result<()> {
        if id < FIRST_DATA_PAGE || id.0 >= s.hdr.page_count {
            Err(Error::InvalidPage(id))
        } else {
            Ok(())
        }
    }

    /// Reads and verifies page `id`.
    pub fn read_page(&self, id: PageId) -> Result<Page> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(Error::Poisoned);
        }
        if id < FIRST_DATA_PAGE || id.0 >= self.pages.load(Ordering::Acquire) {
            return Err(Error::InvalidPage(id));
        }
        read_raw(self.file.as_ref(), id)
    }

    /// Reads and verifies the `n` pages from `first` on with one read of
    /// the file (a scan's read-ahead, §M14.20 W5).
    pub fn read_pages(&self, first: PageId, n: usize) -> Result<Vec<Page>> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(Error::Poisoned);
        }
        let end = first.0.checked_add(n as u64);
        if first < FIRST_DATA_PAGE || end.is_none_or(|e| e > self.pages.load(Ordering::Acquire)) {
            return Err(Error::InvalidPage(first));
        }
        let mut buf = vec![0u8; n * PAGE_SIZE];
        match self.file.read_exact_at(&mut buf, first.offset()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(Error::Corrupt {
                    page: first,
                    reason: "never written (past end of file)",
                });
            }
            Err(e) => return Err(e.into()),
        }
        buf.as_chunks::<PAGE_SIZE>()
            .0
            .iter()
            .zip(first.0..)
            .map(|(bytes, id)| {
                let mut page = Page::new(PageType::UNUSED);
                page.bytes_mut().copy_from_slice(bytes);
                page.verify(PageId(id)).map_err(|reason| Error::Corrupt {
                    page: PageId(id),
                    reason,
                })?;
                Ok(page)
            })
            .collect()
    }

    /// Writes `page` at `id`, stamping its id and checksum.  Only pages
    /// allocated since the last sync may be written — and, while a commit
    /// is in flight, the pages it froze (written back unchanged).
    pub fn write_page(&self, id: PageId, page: &mut Page) -> Result<()> {
        {
            let s = self.begin()?;
            Self::check_data_page(&s, id)?;
            if !Self::may_write(&s, id.0) {
                return Err(Error::Immutable(id));
            }
        }
        if page.page_type() < PageType::FIRST_USER {
            return Err(Error::InvalidPage(id));
        }
        self.write_at(id, page)
    }

    /// Whether `id` was allocated since the last sync (or freeze), so may
    /// be changed in place rather than copied.
    pub fn is_writable(&self, id: PageId) -> bool {
        self.lock().allocated.contains(&id.0)
    }

    /// Whether `id` belongs to a commit in flight and is not durable yet:
    /// its contents must reach the file before the commit's header does.
    pub fn is_frozen(&self, id: PageId) -> bool {
        self.lock()
            .frozen
            .as_ref()
            .is_some_and(|f| f.pages.contains(&id.0))
    }

    /// Allocates a page: the lowest free one, else a new one at the end of
    /// the file.  Its contents are undefined until written; reading it first
    /// fails as corrupt.
    pub fn allocate(&self) -> Result<PageId> {
        let mut s = self.begin()?;
        let id = match s.free.pop_first() {
            Some(id) => id,
            None => {
                s.hdr.page_count += 1;
                self.pages.store(s.hdr.page_count, Ordering::Release);
                s.hdr.page_count - 1
            }
        };
        s.allocated.insert(id);
        Ok(PageId(id))
    }

    /// Frees page `id`.  A page allocated since the last sync is reusable at
    /// once; a durable page only after the next sync.  The caller must not
    /// use the page again.
    pub fn free(&self, id: PageId) -> Result<()> {
        let mut s = self.begin()?;
        Self::check_data_page(&s, id)?;
        if s.allocated.remove(&id.0) {
            s.free.insert(id.0);
        } else if s.free.contains(&id.0)
            || s.pending.contains(&id.0)
            || s.freelist_pages.contains(&id.0)
            || s.frozen
                .as_ref()
                .is_some_and(|f| f.carry.contains(&id.0) || f.list_pages.contains(&id.0))
        {
            return Err(Error::InvalidPage(id)); // a double free
        } else {
            s.pending.insert(id.0);
        }
        Ok(())
    }

    /// Meta slot `slot` as it will be committed.
    pub fn meta(&self, slot: usize) -> u64 {
        self.lock().hdr.meta[slot]
    }

    /// Sets meta slot `slot`.  Durable after the next [`Pager::sync`].
    pub fn set_meta(&self, slot: usize, value: u64) -> Result<()> {
        self.begin()?.hdr.meta[slot] = value;
        Ok(())
    }

    /// Pages in the file, header pages included.
    pub fn page_count(&self) -> u64 {
        self.lock().hdr.page_count
    }

    /// Pages free now (not counting those that become free at the next sync).
    pub fn free_count(&self) -> u64 {
        self.lock().free.len() as u64
    }

    /// The header as last committed.
    pub fn durable_header(&self) -> Header {
        self.lock().durable.clone()
    }

    /// Where every page stands, for consistency checks.
    pub fn accounting(&self) -> Accounting {
        let s = self.lock();
        Accounting {
            page_count: s.hdr.page_count,
            free: s.free.clone(),
            pending: s.pending.clone(),
            freelist_pages: s.freelist_pages.iter().copied().collect(),
            allocated: s.allocated.clone(),
            frozen: s.frozen.as_ref().map_or_else(BTreeSet::new, |f| {
                f.pages
                    .iter()
                    .chain(&f.carry)
                    .chain(&f.list_pages)
                    .copied()
                    .collect()
            }),
        }
    }

    /// Commits: makes every page written since the last sync, every
    /// allocation, free and meta change durable, atomically.  If this fails
    /// the pager is poisoned: what reached the disk is unknown, so the handle
    /// must be dropped and the file reopened.
    pub fn sync(&self) -> Result<()> {
        let frozen = self.freeze(&[])?;
        self.commit_frozen(frozen)
    }

    /// Syncs and drops the pager.
    pub fn close(self) -> Result<()> {
        self.sync()
    }

    /// The first half of a commit (§M14.20 W4): the state as it is now —
    /// every page allocated so far, the meta slots with `meta` set — is the
    /// next to be made durable, by [`commit_frozen`](Self::commit_frozen).
    /// Its pages may no longer be changed (a writer copies them, as it
    /// copies durable ones), so writing goes on meanwhile into pages the
    /// frozen state does not use: what is allocated now on comes from pages
    /// free in it, or past its end, and what is freed now on is free only
    /// after the next commit.  The frozen pages still to be written must
    /// reach the file before `commit_frozen` (the buffer pool writes them
    /// back: [`BufferPool::flush_frozen`](crate::buffer::BufferPool::flush_frozen)).
    ///
    /// # Panics
    ///
    /// If a commit is already in flight: one at a time (the store's
    /// checkpoints are serialized).
    pub fn freeze(&self, meta: &[(usize, u64)]) -> Result<Frozen> {
        let mut s = self.begin()?;
        assert!(s.frozen.is_none(), "a commit is already in flight");
        for &(slot, value) in meta {
            s.hdr.meta[slot] = value;
        }
        // Free in the new state: free now, durable pages freed, and the
        // pages of the durable free list.  The new free list goes into
        // pages neither state uses: free ones (taken out, so no writer gets
        // them meanwhile), or new ones.
        let carry = std::mem::take(&mut s.pending);
        let mut list_pages = Vec::new();
        let others = carry.len() + s.freelist_pages.len();
        while list_pages.len() * IDS_PER_FREELIST_PAGE < s.free.len() + others {
            match s.free.pop_first() {
                Some(id) => list_pages.push(id),
                None => {
                    list_pages.push(s.hdr.page_count);
                    s.hdr.page_count += 1;
                    self.pages.store(s.hdr.page_count, Ordering::Release);
                }
            }
        }
        let mut free = s.free.clone();
        free.extend(&carry);
        free.extend(&s.freelist_pages);
        let mut hdr = s.hdr.clone();
        hdr.seq = s.durable.seq + 1;
        hdr.freelist_head = list_pages.first().copied().unwrap_or(0);
        hdr.free_count = free.len() as u64;
        let pages = std::mem::take(&mut s.allocated);
        s.frozen = Some(FrozenPages {
            pages,
            carry,
            list_pages: list_pages.clone(),
        });
        Ok(Frozen {
            hdr,
            list_pages,
            free: free.into_iter().collect(),
        })
    }

    /// The second half of a commit: `frozen`'s free list and header
    /// written, durable after its pages — without the state lock, so pages
    /// are allocated, freed and written meanwhile.  If this fails the pager
    /// is poisoned (see [`sync`](Self::sync)).
    pub fn commit_frozen(&self, frozen: Frozen) -> Result<()> {
        let result = self.write_frozen(&frozen);
        let mut s = self.lock();
        if let Err(e) = result {
            s.poisoned = true;
            self.poisoned.store(true, Ordering::Release);
            return Err(e);
        }
        let f = s.frozen.take().expect("the commit in flight");
        let old_list = std::mem::replace(&mut s.freelist_pages, f.list_pages);
        s.free.extend(f.carry);
        s.free.extend(old_list);
        s.hdr.seq = frozen.hdr.seq;
        s.durable = frozen.hdr;
        Ok(())
    }

    fn write_frozen(&self, f: &Frozen) -> Result<()> {
        if self.lock().poisoned {
            return Err(Error::Poisoned);
        }
        let mut chunks = f.free.chunks(IDS_PER_FREELIST_PAGE);
        for (i, &page_id) in f.list_pages.iter().enumerate() {
            let next = f.list_pages.get(i + 1).copied().unwrap_or(0);
            let chunk = chunks.next().unwrap_or(&[]);
            let mut page = Page::new(PageType::FREELIST);
            let p = page.payload_mut();
            write_u64(p, 0, next);
            write_u32(p, 8, chunk.len() as u32);
            for (j, id) in chunk.iter().enumerate() {
                write_u64(p, 16 + 8 * j, *id);
            }
            self.write_at(PageId(page_id), &mut page)?;
        }
        // The file must be as long as the header will claim (allocated
        // pages nobody wrote yet exist as zeros) — extended under the state
        // lock, which every write past the known length takes, so it never
        // cuts one short.
        {
            let mut s = self.lock();
            if s.known_len < f.hdr.page_count {
                if self.file.size()? < f.hdr.page_count * PAGE_SIZE as u64 {
                    self.file.set_len(f.hdr.page_count * PAGE_SIZE as u64)?;
                }
                s.known_len = f.hdr.page_count;
            }
        }
        // The data the new header refers to, before it (ordering is all it
        // needs: the header's sync makes both durable).
        self.file.sync_order()?;
        write_header(self.file.as_ref(), &f.hdr)?;
        self.file.sync()?; // the header itself
        Ok(())
    }

    /// Checks the free list on disk against the pager's view of it, and that
    /// no page is in two places at once.
    pub fn check(&self) -> Result<()> {
        let s = self.begin()?;
        let (on_disk, pages) = read_freelist(self.file.as_ref(), &s.durable)?;
        let corrupt = |reason| Error::Corrupt {
            page: PageId(s.durable.seq % 2),
            reason,
        };
        if pages != s.freelist_pages {
            return Err(corrupt("free list pages differ from the pager's"));
        }
        let none = BTreeSet::new();
        let (frozen, carry, list) = match &s.frozen {
            Some(f) => (&f.pages, &f.carry, f.list_pages.as_slice()),
            None => (&none, &none, &[][..]),
        };
        // Durable-free = free now or allocated since, within the durable
        // page count (pages past it were never on the durable list) — told
        // only between commits (while one is in flight, a page freed since
        // its freeze may have been on the durable list or not).
        let expect: BTreeSet<u64> = s
            .free
            .iter()
            .chain(&s.allocated)
            .copied()
            .filter(|&id| id < s.durable.page_count)
            .collect();
        if s.frozen.is_none() && on_disk != expect {
            return Err(corrupt("free list on disk differs from the pager's"));
        }
        let mut seen = BTreeSet::new();
        for id in s
            .free
            .iter()
            .chain(&s.pending)
            .chain(&s.freelist_pages)
            .chain(&s.allocated)
            .chain(frozen)
            .chain(carry)
            .chain(list)
        {
            if *id < FIRST_DATA_PAGE.0 || *id >= s.hdr.page_count || !seen.insert(*id) {
                return Err(Error::Corrupt {
                    page: PageId(*id),
                    reason: "page accounted twice or out of range",
                });
            }
        }
        Ok(())
    }
}

fn read_raw(file: &dyn File, id: PageId) -> Result<Page> {
    let mut page = Page::new(PageType::UNUSED);
    match file.read_exact_at(page.bytes_mut(), id.offset()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(Error::Corrupt {
                page: id,
                reason: "never written (past end of file)",
            });
        }
        Err(e) => return Err(e.into()),
    }
    page.verify(id)
        .map_err(|reason| Error::Corrupt { page: id, reason })?;
    Ok(page)
}

fn write_raw(file: &dyn File, id: PageId, page: &mut Page) -> Result<()> {
    page.seal(id);
    Ok(file.write_all_at(page.bytes(), id.offset())?)
}

/// Reads the free list of `hdr`: the free page numbers and the pages that
/// hold them.
fn read_freelist(file: &dyn File, hdr: &Header) -> Result<(BTreeSet<u64>, Vec<u64>)> {
    let mut free = BTreeSet::new();
    let mut pages = Vec::new();
    let mut id = hdr.freelist_head;
    let in_range = |id: u64| (FIRST_DATA_PAGE.0..hdr.page_count).contains(&id);
    while id != 0 {
        let corrupt = |reason| Error::Corrupt {
            page: PageId(id),
            reason,
        };
        if !in_range(id) || pages.contains(&id) {
            return Err(corrupt("free list chain out of range or cyclic"));
        }
        let page = read_raw(file, PageId(id))?;
        if page.page_type() != PageType::FREELIST {
            return Err(corrupt("free list chain reaches a non-free-list page"));
        }
        let p = page.payload();
        let n = read_u32(p, 8) as usize;
        if n > IDS_PER_FREELIST_PAGE {
            return Err(corrupt("free-list page overfull"));
        }
        for j in 0..n {
            let free_id = read_u64(p, 16 + 8 * j);
            if !in_range(free_id) || !free.insert(free_id) {
                return Err(corrupt("free page number out of range or repeated"));
            }
        }
        pages.push(id);
        id = read_u64(p, 0);
    }
    if free.len() as u64 != hdr.free_count || pages.iter().any(|p| free.contains(p)) {
        return Err(Error::Corrupt {
            page: PageId(hdr.seq % 2),
            reason: "free list inconsistent with the header",
        });
    }
    Ok((free, pages))
}

fn write_header(file: &dyn File, hdr: &Header) -> Result<()> {
    let slot = PageId(hdr.seq % 2);
    let mut page = hdr.encode();
    page.seal(slot);
    Ok(file.write_all_at(page.bytes(), slot.offset())?)
}

/// Reads both header copies and returns the valid one with the highest
/// sequence number.
fn read_best_header(file: &dyn File) -> Result<Header> {
    let mut best: Option<Header> = None;
    let mut not_ours = None;
    for slot in [PageId(0), PageId(1)] {
        let mut page = Page::new(PageType::UNUSED);
        match file.read_exact_at(page.bytes_mut(), slot.offset()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => continue,
            Err(e) => return Err(e.into()),
        }
        if page.verify(slot).is_err() || page.page_type() != PageType::HEADER {
            continue; // torn or damaged: the other copy decides
        }
        match Header::decode(&page) {
            Ok(h) if h.seq % 2 == slot.0 => {
                if best.as_ref().is_none_or(|b| h.seq > b.seq) {
                    best = Some(h);
                }
            }
            Ok(_) => {}
            Err(e) => not_ours = Some(e),
        }
    }
    match (best, not_ours) {
        (Some(h), _) => Ok(h),
        (None, Some(e)) => Err(e),
        (None, None) => {
            let mut head = [0u8; 32];
            let n = file.read_at(&mut head, 0)?;
            if n < 32 || &head[24..32] != MAGIC {
                Err(Error::NotADatabase("no nucleus header".into()))
            } else {
                Err(Error::Corrupt {
                    page: PageId(0),
                    reason: "both header copies are damaged",
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nucleus_platform::Platform;
    use nucleus_platform::sim::SimPlatform;

    /// A file of format version 1 (before compressed values) opens; its
    /// next commit marks it version 2; a version this build does not know
    /// is refused.
    #[test]
    fn older_format_versions_open_and_newer_are_refused() {
        let p = SimPlatform::new(1);
        let fs = p.fs();
        fs.create_dir_all(Path::new("/db")).unwrap();
        let path = Path::new("/db/data");
        Pager::create(fs, path).unwrap().close().unwrap();
        let set_version = |v: u32| {
            let file = fs.open(path, OpenMode::ReadWrite).unwrap();
            for id in [PageId(0), PageId(1)] {
                let mut page = read_raw(file.as_ref(), id).unwrap();
                write_u32(page.payload_mut(), 8, v);
                write_raw(file.as_ref(), id, &mut page).unwrap();
            }
            file.sync().unwrap();
        };
        let version = || {
            let file = fs.open(path, OpenMode::ReadOnly).unwrap();
            [PageId(0), PageId(1)]
                .map(|id| read_u32(read_raw(file.as_ref(), id).unwrap().payload(), 8))
        };
        set_version(1);
        let pager = Pager::open(fs, path).unwrap();
        pager.set_meta(0, 7).unwrap();
        pager.sync().unwrap();
        pager.close().unwrap();
        assert!(version().contains(&FORMAT_VERSION));
        assert_eq!(Pager::open(fs, path).unwrap().meta(0), 7);
        set_version(FORMAT_VERSION + 1);
        assert!(matches!(Pager::open(fs, path), Err(Error::NotADatabase(_))));
    }
}
