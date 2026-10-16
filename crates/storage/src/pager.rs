//! The pager: page I/O, page allocation, and an atomically updated file
//! header.
//!
//! # File layout
//!
//! Pages 0 and 1 are two copies of the file header; data pages start at
//! [`FIRST_DATA_PAGE`].  A header update writes the *older* copy with the
//! next sequence number, so a crash mid-update leaves the newer copy intact,
//! and opening picks the valid copy with the highest sequence number.
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
//! | 32 | 8 | free list head (0 = empty) |
//! | 40 | 8 | free list length |
//! | 48 | 8 × [`ROOT_SLOTS`] | root slots (0 = unset) |
//!
//! # What survives a crash
//!
//! [`Pager::sync`] makes every page write and the header durable, in that
//! order: data first, then the header that refers to it.  After a crash the
//! file opens in the state of the last completed `sync` (or of the one in
//! flight, if its header made it).  The pager's own structures — header and
//! free list — are always consistent; a crash may **leak** up to
//! [`RESERVE_TARGET`] free pages, never corrupt one.
//!
//! What the pager does *not* provide is atomicity for the pages its callers
//! write: a page rewritten or freed after a `sync` holds its new contents
//! even if the crash rolls the header back.  That is the WAL's job (PLAN.md
//! §M4).
//!
//! # Why the free list is safe
//!
//! Free pages form a linked list through the pages themselves.  The danger
//! is handing out a page that the *durable* header still lists as free: the
//! caller overwrites it, a crash rolls the header back, and the durable free
//! list now runs through a page of user data.  So the pager never allocates
//! from the durable list.  It allocates from (1) pages freed since the last
//! `sync` — not in the durable list yet — then (2) a **reserve** that `sync`
//! takes off the list *before* committing the header, then (3) the end of
//! the file.  A crash forgets the reserve; those pages leak, which a future
//! vacuum can reclaim, and nothing else is lost.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use nucleus_platform::{File, FileSystem, OpenMode};

use crate::error::{Error, Result};
use crate::page::{PAGE_SIZE, Page, PageId, PageType, read_u32, read_u64, write_u32, write_u64};

/// The on-disk format version this code reads and writes.
pub const FORMAT_VERSION: u32 = 1;

/// The first page that can hold data; 0 and 1 are the header copies.
pub const FIRST_DATA_PAGE: PageId = PageId(2);

/// How many root slots the header has.  Each holds one page id for the
/// layers above (for example the root of a B+tree).
pub const ROOT_SLOTS: usize = 16;

/// How many free pages `sync` keeps aside for allocation — and so the most
/// a crash can leak.
pub const RESERVE_TARGET: usize = 32;

const MAGIC: &[u8; 8] = b"NUCLEUS\0";

/// The file header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// Incremented by every committed update.
    pub seq: u64,
    /// Pages in the file, header pages included.
    pub page_count: u64,
    /// First page of the free list, or 0.
    pub free_head: u64,
    /// Length of the free list.
    pub free_count: u64,
    /// The root slots; 0 means unset.
    pub roots: [u64; ROOT_SLOTS],
}

impl Header {
    fn new() -> Self {
        Self {
            seq: 0,
            page_count: FIRST_DATA_PAGE.0,
            free_head: 0,
            free_count: 0,
            roots: [0; ROOT_SLOTS],
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
        write_u64(p, 32, self.free_head);
        write_u64(p, 40, self.free_count);
        for (i, r) in self.roots.iter().enumerate() {
            write_u64(p, 48 + 8 * i, *r);
        }
        page
    }

    fn decode(page: &Page) -> Result<Self> {
        let p = page.payload();
        if &p[0..8] != MAGIC {
            return Err(Error::NotADatabase("bad magic".into()));
        }
        let version = read_u32(p, 8);
        if version != FORMAT_VERSION {
            return Err(Error::NotADatabase(format!(
                "format version {version}, this build reads {FORMAT_VERSION}"
            )));
        }
        let page_size = read_u32(p, 12);
        if page_size as usize != PAGE_SIZE {
            return Err(Error::NotADatabase(format!(
                "page size {page_size}, this build uses {PAGE_SIZE}"
            )));
        }
        let mut roots = [0; ROOT_SLOTS];
        for (i, r) in roots.iter_mut().enumerate() {
            *r = read_u64(p, 48 + 8 * i);
        }
        Ok(Self {
            seq: read_u64(p, 16),
            page_count: read_u64(p, 24),
            free_head: read_u64(p, 32),
            free_count: read_u64(p, 40),
            roots,
        })
    }
}

#[derive(Debug)]
struct State {
    /// The header as it will be committed by the next `sync`.
    hdr: Header,
    /// The header as last committed.
    durable: Header,
    /// Free pages off the durable list, safe to hand out.
    reserve: Vec<u64>,
    /// How many pages at the head of `hdr`'s free list were freed since the
    /// last commit (so are not on the durable list).
    fresh: u64,
    /// A sync failed; refuse everything.
    poisoned: bool,
}

/// What [`Pager::check`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckReport {
    /// Pages on the free list.
    pub free: u64,
    /// Pages in the reserve.
    pub reserved: usize,
}

/// A database file as an array of pages.  Shareable between threads: page
/// reads and writes run in parallel, allocation and `sync` take a lock.
pub struct Pager {
    file: Box<dyn File>,
    state: Mutex<State>,
}

impl std::fmt::Debug for Pager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pager")
            .field("state", &self.lock())
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
        let tmp = std::path::PathBuf::from(tmp);
        match fs.remove_file(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()), // a leftover of a crashed create
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
        Ok(Self::with(file, hdr))
    }

    /// Opens the database file at `path`.
    pub fn open(fs: &dyn FileSystem, path: &Path) -> Result<Self> {
        let file = fs.open(path, OpenMode::ReadWrite)?;
        let hdr = read_best_header(file.as_ref())?;
        if hdr.page_count < FIRST_DATA_PAGE.0
            || (hdr.free_head != 0 && !(FIRST_DATA_PAGE.0..hdr.page_count).contains(&hdr.free_head))
        {
            return Err(Error::Corrupt {
                page: PageId(hdr.seq % 2),
                reason: "header fields out of range",
            });
        }
        if file.size()? < hdr.page_count * PAGE_SIZE as u64 {
            return Err(Error::Corrupt {
                page: PageId(hdr.page_count - 1),
                reason: "file shorter than its header says",
            });
        }
        let pager = Self::with(file, hdr);
        if pager.lock().hdr.free_head != 0 {
            pager.sync()?; // fills the reserve, so the free list is usable now
        }
        Ok(pager)
    }

    fn with(file: Box<dyn File>, hdr: Header) -> Self {
        Self {
            file,
            state: Mutex::new(State {
                durable: hdr.clone(),
                hdr,
                reserve: Vec::new(),
                fresh: 0,
                poisoned: false,
            }),
        }
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

    fn check_data_page(&self, s: &State, id: PageId) -> Result<()> {
        if id < FIRST_DATA_PAGE || id.0 >= s.hdr.page_count {
            Err(Error::InvalidPage(id))
        } else {
            Ok(())
        }
    }

    /// Reads and verifies page `id`.
    pub fn read_page(&self, id: PageId) -> Result<Page> {
        self.check_data_page(&*self.begin()?, id)?;
        self.read_raw(id)
    }

    fn read_raw(&self, id: PageId) -> Result<Page> {
        let mut page = Page::new(PageType::UNUSED);
        match self.file.read_exact_at(page.bytes_mut(), id.offset()) {
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

    /// Writes `page` at `id`, stamping its id and checksum.  Durable after
    /// the next [`Pager::sync`].
    pub fn write_page(&self, id: PageId, page: &mut Page) -> Result<()> {
        self.check_data_page(&*self.begin()?, id)?;
        if page.page_type() < PageType::FIRST_USER {
            return Err(Error::InvalidPage(id));
        }
        self.write_raw(id, page)
    }

    fn write_raw(&self, id: PageId, page: &mut Page) -> Result<()> {
        page.seal(id);
        Ok(self.file.write_all_at(page.bytes(), id.offset())?)
    }

    /// Reads free page `id` and returns its successor on the list.
    fn read_free(&self, s: &State, id: u64) -> Result<u64> {
        let page = self.read_raw(PageId(id))?;
        let corrupt = |reason| Error::Corrupt {
            page: PageId(id),
            reason,
        };
        if page.page_type() != PageType::FREE {
            return Err(corrupt("on the free list but not a free page"));
        }
        let next = read_u64(page.payload(), 0);
        if next != 0 && !(FIRST_DATA_PAGE.0..s.hdr.page_count).contains(&next) {
            return Err(corrupt("free list link out of range"));
        }
        Ok(next)
    }

    /// Pops the head of the free list.
    fn pop_free(&self, s: &mut State) -> Result<u64> {
        let id = s.hdr.free_head;
        s.hdr.free_head = self.read_free(s, id)?;
        s.hdr.free_count -= 1;
        s.fresh = s.fresh.saturating_sub(1);
        Ok(id)
    }

    /// Allocates a page.  Its contents are undefined until written; reading
    /// it first fails as corrupt.
    pub fn allocate(&self) -> Result<PageId> {
        let mut s = self.begin()?;
        if s.fresh > 0 {
            return Ok(PageId(self.pop_free(&mut s)?));
        }
        if let Some(id) = s.reserve.pop() {
            return Ok(PageId(id));
        }
        let id = s.hdr.page_count;
        s.hdr.page_count += 1;
        Ok(PageId(id))
    }

    /// Returns page `id` to the free list.  The caller must not use it
    /// again, and must not free a page twice.
    pub fn free(&self, id: PageId) -> Result<()> {
        let mut s = self.begin()?;
        self.check_data_page(&s, id)?;
        self.push_free(&mut s, id)
    }

    fn push_free(&self, s: &mut State, id: PageId) -> Result<()> {
        let mut page = Page::new(PageType::FREE);
        write_u64(page.payload_mut(), 0, s.hdr.free_head);
        self.write_raw(id, &mut page)?;
        s.hdr.free_head = id.0;
        s.hdr.free_count += 1;
        s.fresh += 1;
        Ok(())
    }

    /// The page in root slot `slot`, if set.
    pub fn root(&self, slot: usize) -> Option<PageId> {
        let r = self.lock().hdr.roots[slot];
        (r != 0).then_some(PageId(r))
    }

    /// Sets root slot `slot`.  Durable after the next [`Pager::sync`].
    pub fn set_root(&self, slot: usize, page: Option<PageId>) -> Result<()> {
        self.begin()?.hdr.roots[slot] = page.map_or(0, |p| p.0);
        Ok(())
    }

    /// Pages in the file, header pages included.
    pub fn page_count(&self) -> u64 {
        self.lock().hdr.page_count
    }

    /// Pages on the free list (the reserve not included).
    pub fn free_count(&self) -> u64 {
        self.lock().hdr.free_count
    }

    /// Pages in the reserve.
    pub fn reserved(&self) -> usize {
        self.lock().reserve.len()
    }

    /// The header as last committed.
    pub fn durable_header(&self) -> Header {
        self.lock().durable.clone()
    }

    /// Makes every page write, allocation, free and root change so far
    /// durable.  If this fails the pager is poisoned: what reached the disk
    /// is unknown, so the handle must be dropped and the file reopened.
    pub fn sync(&self) -> Result<()> {
        self.commit(true)
    }

    /// Returns the reserve to the free list and syncs, so a clean shutdown
    /// leaks nothing.
    pub fn close(self) -> Result<()> {
        {
            let mut s = self.begin()?;
            while let Some(id) = s.reserve.pop() {
                self.push_free(&mut s, PageId(id))?;
            }
        }
        self.commit(false)
    }

    fn commit(&self, refill: bool) -> Result<()> {
        let mut s = self.begin()?;
        let result = self.commit_locked(&mut s, refill);
        if result.is_err() {
            s.poisoned = true;
        }
        result
    }

    fn commit_locked(&self, s: &mut State, refill: bool) -> Result<()> {
        if refill {
            while s.reserve.len() < RESERVE_TARGET && s.hdr.free_head != 0 {
                let id = self.pop_free(s)?;
                s.reserve.push(id);
            }
        }
        // The header will claim `page_count` pages; allocated pages nobody
        // wrote yet must exist (as zeros) before it does.
        let len = s.hdr.page_count * PAGE_SIZE as u64;
        if self.file.size()? < len {
            self.file.set_len(len)?;
        }
        self.file.sync()?; // the data the new header refers to
        let mut next = s.hdr.clone();
        next.seq = s.durable.seq + 1;
        write_header(self.file.as_ref(), &next)?;
        self.file.sync()?; // the header itself
        s.hdr.seq = next.seq;
        s.durable = next;
        s.fresh = 0;
        Ok(())
    }

    /// Walks the free list and checks it against the header: every page on
    /// it is a valid free page, it has no cycle, its length matches, and the
    /// reserve is disjoint from it.
    pub fn check(&self) -> Result<CheckReport> {
        let s = self.begin()?;
        let mut seen = std::collections::BTreeSet::new();
        let mut id = s.hdr.free_head;
        while id != 0 {
            if !seen.insert(id) || seen.len() as u64 > s.hdr.page_count {
                return Err(Error::Corrupt {
                    page: PageId(id),
                    reason: "free list has a cycle",
                });
            }
            id = self.read_free(&s, id)?;
        }
        if seen.len() as u64 != s.hdr.free_count {
            return Err(Error::Corrupt {
                page: PageId(s.hdr.seq % 2),
                reason: "free list length differs from the header",
            });
        }
        if let Some(&r) = s.reserve.iter().find(|r| seen.contains(r)) {
            return Err(Error::Corrupt {
                page: PageId(r),
                reason: "reserved page also on the free list",
            });
        }
        Ok(CheckReport {
            free: s.hdr.free_count,
            reserved: s.reserve.len(),
        })
    }
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
            Ok(_) => {} // a copy in the wrong slot: ignore it
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
