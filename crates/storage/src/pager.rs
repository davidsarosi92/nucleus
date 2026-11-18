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
//! | 176 | 4 | the cipher (0: in the clear; [`crate::cipher`]) |
//! | 180 | 4 | the key's generation (encrypted files; 0 at first) |
//! | 184 | 16 | the key's check value (encrypted files) |
//! | 200 | 16 | the tag of bytes `0..200` — and, past generation 0, `216..248` — under the key (encrypted files) |
//! | 216 | 16 | the previous key's check value, while pages are sealed again under the key (zero: none) |
//! | 232 | 8 | the first page not yet looked at by that |
//! | 240 | 8 | the page it ends before |
//!
//! An encrypted file (PLAN.md §M14.30 K2b) is format version
//! [`ENCRYPTED_FORMAT_VERSION`]: its data pages are sealed
//! ([`crate::cipher`]), its headers stay readable and are authenticated.
//!
//! # A new key (K3f)
//!
//! [`Pager::rekey`] makes a new key the file's — the next generation —
//! and keeps the one before beside it: every write from then on is
//! sealed under the new key, and a page says by its parity bit which of
//! the two it is under.  The pages the old key sealed are then sealed
//! again under the new one by [`Pager::reseal_step`], a batch at a time,
//! *in place* — the one exception to the rule above, made safe by
//! keeping it to the same contents: a durable page's bytes are written
//! again with what they held, sealed under another key, so no state
//! changes.  A torn write is what could still harm it, so each batch's
//! old bytes are first written to a journal (`data.reseal`, synced:
//! InnoDB's doublewrite buffer, for this one use), and a page that opens
//! under neither key after a crash is put back from it
//! ([`Pager::recover_reseal`]).  A batch takes only pages that are
//! durable and in use, and no commit frees a page while one runs, so no
//! page is reused under it.  A read that meets a page mid-write (it
//! opens under neither key) waits for the batch and reads again.  Once
//! every page the old key sealed is done, the next commit's header names
//! no previous key and the pager lets go of it.
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
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use nucleus_platform::{File, FileSystem, OpenMode};

use crate::cipher::{CIPHER_NONE, PageCipher};
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

/// The format version of an encrypted file (PLAN.md §M14.30 K2b): a
/// file in the clear stays at [`FORMAT_VERSION`], so encryption alone
/// makes older builds refuse a file.
pub const ENCRYPTED_FORMAT_VERSION: u32 = 3;

/// Where the header's cipher fields start, in its payload.
const CIPHER_AT: usize = 48 + 8 * META_SLOTS;
/// The bytes of the header payload its tag covers (and, past key
/// generation 0, [`REKEY_AT`]'s too).
const TAGGED: usize = CIPHER_AT + 24;
/// Where a new key's fields are (K3f): the previous key's check value,
/// the next page to look at, the end.
const REKEY_AT: usize = TAGGED + 16;
const REKEY_LEN: usize = 32;

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
    /// The cipher its pages are sealed with ([`CIPHER_NONE`]: none).
    pub cipher: u32,
    /// The key's check value ([`PageCipher::check_value`]).
    pub key_check: [u8; 16],
    /// The key's generation (K3f): 0, then one more for each new key.
    pub key_generation: u32,
    /// While the pages are being sealed again under the key: the check
    /// value of the one before (K3f).
    pub previous_check: Option<[u8; 16]>,
    /// The first page the sealing again has not looked at.
    pub reseal_next: u64,
    /// The page it ends before: the file's end when the key was made.
    pub reseal_end: u64,
}

impl Header {
    fn new(cipher: Option<&PageCipher>) -> Self {
        Self {
            seq: 0,
            page_count: FIRST_DATA_PAGE.0,
            freelist_head: 0,
            free_count: 0,
            meta: [0; META_SLOTS],
            cipher: cipher.map_or(CIPHER_NONE, PageCipher::code),
            key_check: cipher.map_or([0; 16], PageCipher::check_value),
            key_generation: cipher.map_or(0, PageCipher::generation),
            previous_check: None,
            reseal_next: 0,
            reseal_end: 0,
        }
    }

    fn encode(&self) -> Page {
        let mut page = Page::new(PageType::HEADER);
        let p = page.payload_mut();
        p[0..8].copy_from_slice(MAGIC);
        let version = if self.cipher == CIPHER_NONE {
            FORMAT_VERSION
        } else {
            ENCRYPTED_FORMAT_VERSION
        };
        write_u32(p, 8, version);
        write_u32(p, 12, PAGE_SIZE as u32);
        write_u64(p, 16, self.seq);
        write_u64(p, 24, self.page_count);
        write_u64(p, 32, self.freelist_head);
        write_u64(p, 40, self.free_count);
        for (i, m) in self.meta.iter().enumerate() {
            write_u64(p, 48 + 8 * i, *m);
        }
        write_u32(p, CIPHER_AT, self.cipher);
        write_u32(p, CIPHER_AT + 4, self.key_generation);
        p[CIPHER_AT + 8..CIPHER_AT + 24].copy_from_slice(&self.key_check);
        if self.key_generation != 0 {
            p[REKEY_AT..REKEY_AT + 16].copy_from_slice(&self.previous_check.unwrap_or([0; 16]));
            write_u64(p, REKEY_AT + 16, self.reseal_next);
            write_u64(p, REKEY_AT + 24, self.reseal_end);
        }
        page
    }

    fn decode(page: &Page) -> Result<Self> {
        let p = page.payload();
        if &p[0..8] != MAGIC {
            return Err(Error::NotADatabase("bad magic".into()));
        }
        let version = read_u32(p, 8);
        if !(OLDEST_FORMAT_VERSION..=ENCRYPTED_FORMAT_VERSION).contains(&version) {
            return Err(Error::NotADatabase(format!(
                "format version {version}, this build reads {OLDEST_FORMAT_VERSION} to \
                 {ENCRYPTED_FORMAT_VERSION}"
            )));
        }
        let cipher = read_u32(p, CIPHER_AT);
        if (version == ENCRYPTED_FORMAT_VERSION) != (cipher != CIPHER_NONE) {
            return Err(Error::NotADatabase(format!(
                "format version {version} with cipher {cipher}"
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
        let key_generation = read_u32(p, CIPHER_AT + 4);
        let (previous_check, reseal_next, reseal_end) = if key_generation == 0 {
            (None, 0, 0)
        } else {
            let c: [u8; 16] = p[REKEY_AT..REKEY_AT + 16].try_into().expect("16 bytes");
            (
                (c != [0; 16]).then_some(c),
                read_u64(p, REKEY_AT + 16),
                read_u64(p, REKEY_AT + 24),
            )
        };
        Ok(Self {
            seq: read_u64(p, 16),
            page_count: read_u64(p, 24),
            freelist_head: read_u64(p, 32),
            free_count: read_u64(p, 40),
            meta,
            cipher,
            key_check: p[CIPHER_AT + 8..CIPHER_AT + 24]
                .try_into()
                .expect("16 bytes"),
            key_generation,
            previous_check,
            reseal_next,
            reseal_end,
        })
    }
}

/// The header payload's bytes its tag covers: those before the tag and,
/// past key generation 0, a new key's fields (a file never given a new
/// key is tagged as before them).
fn tagged(p: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if read_u32(p, CIPHER_AT + 4) == 0 {
        std::borrow::Cow::Borrowed(&p[..TAGGED])
    } else {
        let mut v = p[..TAGGED].to_vec();
        v.extend_from_slice(&p[REKEY_AT..REKEY_AT + REKEY_LEN]);
        std::borrow::Cow::Owned(v)
    }
}

/// Where sealing a file's pages again under a new key stands
/// ([`Pager::rekey_state`], PLAN.md §M14.30 K3f).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rekey {
    /// None runs.
    Idle,
    /// The pages from `next` to `end` are still to be looked at;
    /// `commit_needed`: not before the new key's first commit is durable
    /// (the pages written under the old key until then are all in it).
    Running {
        /// The first page not looked at.
        next: u64,
        /// The page it ends before.
        end: u64,
        /// A commit must come first.
        commit_needed: bool,
    },
    /// Every page is sealed under the new key: the next commit lets the
    /// old one go.
    Finishing,
}

/// The journal of a batch sealed again in place: its pages' old bytes
/// (K3f).
const JOURNAL_MAGIC: &[u8; 8] = b"NUCRSL01";

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
    /// The branches whose frees need their pages' births (§M14.13): a
    /// branch of another, or one with branches of its own.
    spaces: std::collections::HashMap<u32, Space>,
    /// Pages put on dead lists since [`Pager::take_dead`] last took them.
    dead: Vec<Dead>,
}

/// What a branch's free of a durable page means (PLAN.md §M14.13): the
/// commit it was branched at, if it is a branch of another — a page born
/// then or before is an ancestor's and stays —, and the commits its own
/// branches were made at — a page of its own born at or before the latest
/// may still be theirs, and goes to that point's dead list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Space {
    /// The commit the branch starts from (`None`: the main branch).
    pub origin: Option<u64>,
    /// The commits its branches start from, ascending.
    pub points: Vec<u64>,
}

/// A page on a dead list: freed by `branch` while its branch made at
/// `point` may still read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dead {
    /// The branch that freed it.
    pub branch: u32,
    /// The branch point whose list it is on.
    pub point: u64,
    /// The page.
    pub page: u64,
    /// The commit it was born in.
    pub birth: u64,
}

/// What a branch's free of a page does ([`Pager::release`]).
enum Fate {
    Free,
    Keep,
    Dead(u64),
}

/// The branch whose frees are always plain ([`Pager::free`]): the
/// branch directory's own tree, the pages a dropped branch's dead lists
/// give back.
pub const NO_BRANCH: u32 = u32::MAX;

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
    /// The key its pages are sealed under, if it is encrypted — and,
    /// while they are sealed again under a new one, the one before
    /// (K3f).  Read by every page read and write; replaced by
    /// [`rekey`](Self::rekey) and when that ends.
    cipher: RwLock<Option<Arc<PageCipher>>>,
    /// Held by a batch sealed again in place, and by a commit while it
    /// makes pages free: no page in a batch is reused under it (K3f).
    reseal: Mutex<()>,
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
        Self::create_with(fs, path, None)
    }

    /// [`create`](Self::create), its pages sealed under `cipher` if one
    /// is given (PLAN.md §M14.30 K2b).
    pub fn create_with(
        fs: &dyn FileSystem,
        path: &Path,
        cipher: Option<PageCipher>,
    ) -> Result<Self> {
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
        let mut hdr = Header::new(cipher.as_ref());
        for _ in 0..2 {
            write_header(file.as_ref(), &hdr, cipher.as_ref())?;
            hdr.seq += 1;
        }
        hdr.seq -= 1;
        file.sync()?;
        fs.rename(&tmp, path)?;
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
        fs.sync_dir(dir.unwrap_or(Path::new(".")))?;
        Ok(Self::with(file, cipher, hdr, BTreeSet::new(), Vec::new()))
    }

    /// Opens the database file at `path`.
    pub fn open(fs: &dyn FileSystem, path: &Path) -> Result<Self> {
        Self::open_with(fs, path, None)
    }

    /// [`open`](Self::open) with the key its pages are sealed under: an
    /// encrypted file needs it ([`Error::Key`] without it, or with
    /// another), a file in the clear refuses one.
    pub fn open_with(fs: &dyn FileSystem, path: &Path, cipher: Option<PageCipher>) -> Result<Self> {
        Self::open_keys(fs, path, cipher, None)
    }

    /// [`open_with`](Self::open_with) with the key before `cipher` too,
    /// while the pages it sealed are sealed again under `cipher` (PLAN.md
    /// §M14.30 K3f): a file whose header names it as the previous key
    /// needs it; a file still under it is given `cipher` as its new key
    /// at once ([`rekey`](Self::rekey), the header written at the next
    /// commit); a file under `cipher` alone ignores it.
    pub fn open_keys(
        fs: &dyn FileSystem,
        path: &Path,
        cipher: Option<PageCipher>,
        previous: Option<PageCipher>,
    ) -> Result<Self> {
        let file = fs.open(path, OpenMode::ReadWrite)?;
        let candidates: Vec<&PageCipher> = cipher.iter().chain(previous.iter()).collect();
        let hdr = read_best_header(file.as_ref(), &candidates)?;
        // A file still under the previous key: opened under it, then
        // given the new one at once.
        let under_previous = match (&cipher, &previous) {
            (Some(c), Some(p)) => {
                hdr.cipher != CIPHER_NONE
                    && hdr.key_check == p.check_value()
                    && hdr.key_check != c.check_value()
            }
            _ => false,
        };
        let (cipher, previous, rekey_now) = if under_previous {
            if hdr.previous_check.is_some() {
                return Err(Error::Key(format!(
                    "{}: its pages are being sealed again under the key before the one given, \
                     which needs the one before that",
                    path.display()
                )));
            }
            (previous, None, cipher)
        } else {
            (cipher, previous, None)
        };
        match (&cipher, hdr.cipher) {
            (None, CIPHER_NONE) => {}
            (None, _) => {
                return Err(Error::Key(format!(
                    "{} is encrypted; its key is needed",
                    path.display()
                )));
            }
            (Some(_), CIPHER_NONE) => {
                return Err(Error::Key(format!(
                    "{} is not encrypted (encrypting a database in the clear is \
                     `nucleus keys <dir> encrypt`)",
                    path.display()
                )));
            }
            (Some(c), _) => {
                if hdr.key_check != c.check_value() {
                    return Err(Error::Key(format!(
                        "{} is sealed under another key",
                        path.display()
                    )));
                }
            }
        }
        // A file whose pages are being sealed again under its key needs
        // the previous one too.
        let previous = match (hdr.previous_check, &cipher) {
            (Some(check), Some(_)) => Some(
                previous
                    .filter(|p| p.check_value() == check)
                    .ok_or_else(|| {
                        Error::Key(format!(
                            "{}: its pages are being sealed again under a new key; the \
                             previous key is needed until that ends",
                            path.display()
                        ))
                    })?,
            ),
            _ => None,
        };
        // The file's own cipher, whichever this CPU would choose; its
        // generation the header's.
        let own = |c: &PageCipher| {
            c.for_code(hdr.cipher)
                .map_err(|e| Error::Key(format!("{}: {e}", path.display())))
        };
        let cipher = match cipher {
            Some(c) => {
                let mut c = own(&c)?.with_generation(hdr.key_generation);
                if let Some(p) = &previous {
                    c = c.with_previous(own(p)?);
                }
                Some(c)
            }
            None => None,
        };
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
        let (free, freelist_pages) = read_freelist(file.as_ref(), &hdr, cipher.as_ref())?;
        let pager = Self::with(file, cipher, hdr, free, freelist_pages);
        if let Some(new) = rekey_now {
            pager.rekey(new)?;
        }
        Ok(pager)
    }

    fn with(
        file: Box<dyn File>,
        cipher: Option<PageCipher>,
        hdr: Header,
        free: BTreeSet<u64>,
        fl: Vec<u64>,
    ) -> Self {
        let known_len = file.size().map_or(0, |n| n / PAGE_SIZE as u64);
        Self {
            pages: AtomicU64::new(hdr.page_count),
            poisoned: AtomicBool::new(false),
            file,
            cipher: RwLock::new(cipher.map(Arc::new)),
            reseal: Mutex::new(()),
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
                spaces: std::collections::HashMap::new(),
                dead: Vec::new(),
            }),
        }
    }

    /// Writes `page` at `id`; past the pages the file is known to hold,
    /// under the state lock (an extension of the file).
    fn write_at(&self, id: PageId, page: &mut Page) -> Result<()> {
        if id.0 < self.lock().known_len {
            let cipher = self.keys();
            return write_raw(self.file.as_ref(), id, page, cipher.as_deref());
        }
        let mut s = self.lock();
        if id.0 >= s.known_len {
            s.known_len = extend(self.file.as_ref(), s.known_len, id.0 + 1)?;
        }
        let cipher = self.keys();
        write_raw(self.file.as_ref(), id, page, cipher.as_deref())
    }

    /// The key (and previous key) as they are now.
    fn keys(&self) -> Option<Arc<PageCipher>> {
        self.cipher
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn reseal_lock(&self) -> MutexGuard<'_, ()> {
        self.reseal.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A read that failed while pages are sealed again in place: it may
    /// have met one mid-write — once the batch is done, read again.
    fn read_again<T>(
        &self,
        r: Result<T>,
        read: impl Fn(Option<&PageCipher>) -> Result<T>,
    ) -> Result<T> {
        match r {
            Err(Error::Corrupt { .. }) if self.keys().is_some_and(|c| c.previous().is_some()) => {
                let _batch = self.reseal_lock();
                read(self.keys().as_deref())
            }
            r => r,
        }
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
        let read = |c: Option<&PageCipher>| read_raw(self.file.as_ref(), id, c);
        let r = read(self.keys().as_deref());
        self.read_again(r, read)
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
        let read = |c: Option<&PageCipher>| read_run(self.file.as_ref(), first, n, c);
        let r = read(self.keys().as_deref());
        self.read_again(r, read)
    }

    /// The key and cipher its pages are sealed with — the file's, from its
    /// header (PLAN.md §M14.30): `None` in the clear.  While its pages
    /// are sealed again under a new key, it holds the previous one too.
    pub fn cipher(&self) -> Option<Arc<PageCipher>> {
        self.keys()
    }

    /// Whether its pages are sealed (PLAN.md §M14.30 K2b).
    pub fn is_encrypted(&self) -> bool {
        self.keys().is_some()
    }

    /// Makes `key` the file's key (PLAN.md §M14.30 K3f): the next
    /// generation, the one now kept beside it as the previous until every
    /// page it sealed is sealed again ([`reseal_step`](Self::reseal_step)).
    /// Every write from now on is sealed under `key`; the header says so
    /// from the next commit, and the pages to look at are those the file
    /// holds now.  [`Error::Key`] for a file in the clear, for one whose
    /// pages are still being sealed again, or for the same key.
    pub fn rekey(&self, key: PageCipher) -> Result<()> {
        let _batch = self.reseal_lock();
        let mut s = self.begin()?;
        let mut slot = self.cipher.write().unwrap_or_else(|e| e.into_inner());
        let Some(cur) = slot.clone() else {
            return Err(Error::Key(
                "a file in the clear has no key to replace".into(),
            ));
        };
        if cur.previous().is_some() || s.hdr.previous_check.is_some() {
            return Err(Error::Key(
                "its pages are still being sealed again under the key before".into(),
            ));
        }
        if key.check_value() == cur.check_value() {
            return Err(Error::Key("the new key is the key it has".into()));
        }
        let generation = cur
            .generation()
            .checked_add(1)
            .ok_or_else(|| Error::Key("no key generation left".into()))?;
        let new = key
            .for_code(cur.code())
            .map_err(Error::Key)?
            .with_generation(generation)
            .with_previous((*cur).clone());
        s.hdr.key_check = new.check_value();
        s.hdr.key_generation = generation;
        s.hdr.previous_check = Some(cur.check_value());
        s.hdr.reseal_next = FIRST_DATA_PAGE.0;
        s.hdr.reseal_end = s.hdr.page_count;
        *slot = Some(Arc::new(new));
        Ok(())
    }

    /// Where sealing the pages again under a new key stands.
    pub fn rekey_state(&self) -> Rekey {
        let s = self.lock();
        let Some(c) = self.keys().filter(|c| c.previous().is_some()) else {
            return Rekey::Idle;
        };
        if s.hdr.previous_check.is_none() {
            return Rekey::Finishing;
        }
        Rekey::Running {
            next: s.hdr.reseal_next,
            end: s.hdr.reseal_end,
            commit_needed: s.durable.key_generation != c.generation()
                || s.durable.previous_check.is_none(),
        }
    }

    /// Whether page `id` is durable and in use: in the last committed
    /// state, neither free, nor freed since, nor holding a commit in
    /// flight's free list.
    fn durable_in_use(s: &State, id: u64) -> bool {
        id < s.durable.page_count
            && !s.free.contains(&id)
            && !s.pending.contains(&id)
            && !s.allocated.contains(&id)
            && !s.frozen.as_ref().is_some_and(|f| {
                f.pages.contains(&id) || f.carry.contains(&id) || f.list_pages.contains(&id)
            })
    }

    /// Seals again under the new key up to `max` of the pages the old key
    /// sealed (PLAN.md §M14.30 K3f): the next pages from where the last
    /// step stopped that are durable and in use, those under the old key
    /// read, their old bytes written to `journal` and synced, then the
    /// same contents sealed under the new key written in their place and
    /// synced.  Where it stands after.  A step before the new key's first
    /// commit is durable does nothing (`commit_needed`).
    pub fn reseal_step(&self, journal: &dyn File, max: usize) -> Result<Rekey> {
        let _batch = self.reseal_lock();
        let Some(c) = self.keys().filter(|c| c.previous().is_some()) else {
            return Ok(Rekey::Idle);
        };
        let (ids, upto) = {
            let s = self.begin()?;
            if s.hdr.previous_check.is_none() {
                return Ok(Rekey::Finishing);
            }
            if s.durable.key_generation != c.generation() || s.durable.previous_check.is_none() {
                drop(s);
                return Ok(self.rekey_state());
            }
            let mut ids = Vec::new();
            let mut id = s.hdr.reseal_next;
            while id < s.hdr.reseal_end && ids.len() < max {
                if Self::durable_in_use(&s, id) {
                    ids.push(id);
                }
                id += 1;
            }
            (ids, id)
        };
        // The pages under the old key: their bytes now, and sealed anew.
        let mut batch = Vec::new();
        let mut bytes = vec![0u8; PAGE_SIZE];
        for id in ids {
            self.file.read_exact_at(&mut bytes, PageId(id).offset())?;
            if c.sealed_before(&bytes) != Some(true) {
                continue;
            }
            let corrupt = |reason| Error::Corrupt {
                page: PageId(id),
                reason,
            };
            let page = c.open(PageId(id), &bytes).map_err(corrupt)?;
            let mut new = vec![0u8; PAGE_SIZE];
            c.seal(PageId(id), &page, &mut new).map_err(corrupt)?;
            batch.push((id, bytes.clone(), new));
        }
        if !batch.is_empty() {
            let mut j = Vec::with_capacity(16 + batch.len() * (8 + PAGE_SIZE) + 4);
            j.extend_from_slice(JOURNAL_MAGIC);
            j.extend_from_slice(&(batch.len() as u64).to_le_bytes());
            for (id, old, _) in &batch {
                j.extend_from_slice(&id.to_le_bytes());
                j.extend_from_slice(old);
            }
            let crc = crate::crc32c::crc32c(&j);
            j.extend_from_slice(&crc.to_le_bytes());
            journal.write_all_at(&j, 0)?;
            journal.set_len(j.len() as u64)?;
            journal.sync()?;
            for (id, _, new) in &batch {
                self.file.write_all_at(new, PageId(*id).offset())?;
            }
            self.file.sync()?;
        }
        let mut s = self.begin()?;
        s.hdr.reseal_next = upto;
        if upto >= s.hdr.reseal_end {
            // Every page done: the next commit names no previous key.
            s.hdr.previous_check = None;
            s.hdr.reseal_next = 0;
            s.hdr.reseal_end = 0;
        }
        drop(s);
        Ok(self.rekey_state())
    }

    /// After a crash, puts back from `journal` the pages of a batch sealed
    /// again in place ([`reseal_step`](Self::reseal_step)) that open under
    /// neither key — torn by the crash — and empties it.  A journal not
    /// whole (the crash came while it was written: no page was touched
    /// yet) is ignored.  The pages put back.
    pub fn recover_reseal(&self, journal: &dyn File) -> Result<u64> {
        let _batch = self.reseal_lock();
        let len = journal.size()? as usize;
        let mut j = vec![0u8; len];
        journal.read_exact_at(&mut j, 0)?;
        let whole = len >= 20
            && &j[..8] == JOURNAL_MAGIC
            && read_u64(&j, 8).checked_mul((8 + PAGE_SIZE) as u64) == Some(len as u64 - 20)
            && crate::crc32c::crc32c(&j[..len - 4]) == read_u32(&j, len - 4);
        let mut restored = 0;
        if whole {
            let c = self.keys();
            let mut now = vec![0u8; PAGE_SIZE];
            for e in j[16..len - 4].as_chunks::<{ 8 + PAGE_SIZE }>().0 {
                let id = PageId(read_u64(e, 0));
                if id < FIRST_DATA_PAGE {
                    continue;
                }
                match self.file.read_exact_at(&mut now, id.offset()) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => now.fill(0),
                    Err(e) => return Err(e.into()),
                }
                let opens = c.as_deref().is_some_and(|c| c.open(id, &now).is_ok());
                if !opens {
                    self.file.write_all_at(&e[8..], id.offset())?;
                    restored += 1;
                }
            }
            if restored > 0 {
                self.file.sync()?;
            }
        }
        journal.set_len(0)?;
        journal.sync()?;
        Ok(restored)
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
            // Its birth: the commit that makes it durable (§M14.13).
            if let Some(b) = Self::birth_in(&s, id.0) {
                page.set_lsn(b);
            }
        }
        if page.page_type() < PageType::FIRST_USER {
            return Err(Error::InvalidPage(id));
        }
        self.write_at(id, page)
    }

    /// [`write_page`](Self::write_page) for the pages `first`, `first + 1`,
    /// … — a run of neighbours — with one write to the file: a
    /// checkpoint's pages in the file's order (PostgreSQL's checkpointer
    /// sorts its buffers so since 9.6), not a system call per page.
    pub fn write_run(&self, first: PageId, pages: &mut [Page]) -> Result<()> {
        let Some(last) = (pages.len() as u64).checked_sub(1).map(|n| first.0 + n) else {
            return Ok(());
        };
        {
            let s = self.begin()?;
            for (i, page) in pages.iter_mut().enumerate() {
                let id = PageId(first.0 + i as u64);
                Self::check_data_page(&s, id)?;
                if !Self::may_write(&s, id.0) {
                    return Err(Error::Immutable(id));
                }
                if let Some(b) = Self::birth_in(&s, id.0) {
                    page.set_lsn(b);
                }
                if page.page_type() < PageType::FIRST_USER {
                    return Err(Error::InvalidPage(id));
                }
            }
        }
        if last >= self.lock().known_len {
            let mut s = self.lock();
            if last >= s.known_len {
                s.known_len = extend(self.file.as_ref(), s.known_len, last + 1)?;
            }
        }
        let cipher = self.keys();
        RUN_BUFFER.with_borrow_mut(|buf| {
            buf.resize(pages.len() * PAGE_SIZE, 0);
            for (i, (page, out)) in pages
                .iter_mut()
                .zip(buf.as_chunks_mut::<PAGE_SIZE>().0)
                .enumerate()
            {
                let id = PageId(first.0 + i as u64);
                page.seal(id);
                match cipher.as_deref() {
                    None => out.copy_from_slice(page.bytes()),
                    Some(c) => c
                        .seal(id, page, out)
                        .map_err(|reason| Error::Corrupt { page: id, reason })?,
                }
            }
            Ok(self.file.write_all_at(buf, first.offset())?)
        })
    }

    /// Keeps of `ids` those [`is_frozen`](Self::is_frozen), under one look
    /// at the state.
    pub fn retain_frozen(&self, ids: &mut Vec<PageId>) {
        let s = self.lock();
        match s.frozen.as_ref() {
            Some(f) => ids.retain(|id| f.pages.contains(&id.0)),
            None => ids.clear(),
        }
    }

    /// The commit that will make page `id` durable, if it is not yet:
    /// the one in flight for a frozen page, the next for one allocated
    /// since.
    fn birth_in(s: &State, id: u64) -> Option<u64> {
        let in_flight = s.frozen.as_ref();
        if in_flight.is_some_and(|f| f.pages.contains(&id)) {
            Some(s.durable.seq + 1)
        } else if s.allocated.contains(&id) {
            Some(s.durable.seq + 1 + u64::from(in_flight.is_some()))
        } else {
            None
        }
    }

    /// [`birth_in`](Self::birth_in), for the buffer pool: `None` for a
    /// durable page, whose birth its header holds.
    pub fn birth(&self, id: PageId) -> Option<u64> {
        Self::birth_in(&self.lock(), id.0)
    }

    /// Whether a free in `branch` needs its page's birth
    /// ([`release`](Self::release)).
    pub fn needs_birth(&self, branch: u32) -> bool {
        self.lock().spaces.contains_key(&branch)
    }

    /// Sets (or, with `None`, removes) what a free in `branch` means.
    pub fn set_space(&self, branch: u32, space: Option<Space>) {
        let mut s = self.lock();
        match space {
            Some(sp) if sp.origin.is_some() || !sp.points.is_empty() => {
                s.spaces.insert(branch, sp);
            }
            _ => {
                s.spaces.remove(&branch);
            }
        }
    }

    /// What a free in `branch` means.
    pub fn space(&self, branch: u32) -> Space {
        self.lock().spaces.get(&branch).cloned().unwrap_or_default()
    }

    /// Whether a free of page `id`, born in commit `birth`, by `branch`
    /// leaves it in use ([`release`](Self::release)): an ancestor's, or on
    /// a dead list — so the cache keeps it.
    pub fn keeps(&self, id: PageId, branch: u32, birth: u64) -> bool {
        !matches!(Self::fate(&self.lock(), id, branch, birth), Fate::Free)
    }

    fn fate(s: &State, id: PageId, branch: u32, birth: u64) -> Fate {
        if s.allocated.contains(&id.0) {
            return Fate::Free;
        }
        let Some(sp) = s.spaces.get(&branch) else {
            return Fate::Free;
        };
        if sp.origin.is_some_and(|o| birth <= o) {
            return Fate::Keep;
        }
        match sp.points.last() {
            Some(&point) if birth <= point => Fate::Dead(point),
            _ => Fate::Free,
        }
    }

    /// Frees page `id`, born in commit `birth`, for `branch` (PLAN.md
    /// §M14.13): a page allocated since the last commit is free at once;
    /// one an ancestor's (born at or before the branch's origin) stays;
    /// one of the branch's own that a branch of it may read goes to the
    /// latest branch point's dead list ([`take_dead`](Self::take_dead));
    /// else it is free after the next commit.
    pub fn release(&self, id: PageId, branch: u32, birth: u64) -> Result<()> {
        let mut s = self.begin()?;
        Self::check_data_page(&s, id)?;
        match Self::fate(&s, id, branch, birth) {
            Fate::Keep => Ok(()),
            Fate::Dead(point) => {
                s.dead.push(Dead {
                    branch,
                    point,
                    page: id.0,
                    birth,
                });
                Ok(())
            }
            Fate::Free => Self::free_locked(&mut s, id),
        }
    }

    /// The commit the next freeze makes durable: a branch made in it
    /// starts from it.
    pub fn next_seq(&self) -> u64 {
        self.lock().durable.seq + 1
    }

    /// The pages on dead lists not yet taken ([`take_dead`](Self::take_dead)).
    pub fn dead(&self) -> Vec<Dead> {
        self.lock().dead.clone()
    }

    /// The pages put on dead lists since the last call.
    pub fn take_dead(&self) -> Vec<Dead> {
        std::mem::take(&mut self.lock().dead)
    }

    /// Whether `id` may be changed in place rather than copied: allocated
    /// since the last sync (or freeze).
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
        Self::free_locked(&mut s, id)
    }

    fn free_locked(s: &mut State, id: PageId) -> Result<()> {
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

    /// Writes a zeroed page (of type `UNUSED`) over every free page and
    /// syncs: what a page held before it was freed — a leaf a later
    /// version replaced, a value's overflow chain — is gone from the file
    /// (SQLite's `secure_delete`, done at once rather than at each free;
    /// PLAN.md §M14.24 R2).  Pages freed since the last commit are not
    /// free yet and keep theirs.  Under the state lock, so no page is
    /// allocated meanwhile.  The pages zeroed.
    pub fn zero_free(&self) -> Result<u64> {
        let s = self.begin()?;
        let mut zeroed = 0;
        // (Past the file's known length a page was never written.)
        for &id in s.free.range(..s.known_len) {
            let mut page = Page::new(PageType::UNUSED);
            write_raw(
                self.file.as_ref(),
                PageId(id),
                &mut page,
                self.keys().as_deref(),
            )?;
            zeroed += 1;
        }
        drop(s);
        self.file.sync()?;
        Ok(zeroed)
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
        // No page in a batch sealed again in place becomes free under it.
        let _batch = self.reseal_lock();
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
        // Every page sealed again and a header naming no previous key
        // durable: the previous key is let go (K3f).
        if s.durable.previous_check.is_none() && s.hdr.previous_check.is_none() {
            let mut slot = self.cipher.write().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = slot.as_ref().filter(|c| c.previous().is_some()) {
                *slot = Some(Arc::new((**c).clone().without_previous()));
            }
        }
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
        write_header(self.file.as_ref(), &f.hdr, self.keys().as_deref())?;
        self.file.sync()?; // the header itself
        Ok(())
    }

    /// Checks the free list on disk against the pager's view of it, and that
    /// no page is in two places at once.
    pub fn check(&self) -> Result<()> {
        let s = self.begin()?;
        let (on_disk, pages) =
            read_freelist(self.file.as_ref(), &s.durable, self.keys().as_deref())?;
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

fn read_raw(file: &dyn File, id: PageId, cipher: Option<&PageCipher>) -> Result<Page> {
    let mut pages = read_run(file, id, 1, cipher)?;
    Ok(pages.pop().expect("one page read"))
}

thread_local! {
    /// A thread's buffer for reads of the file, kept between them: a read
    /// neither allocates nor zeroes what it is about to overwrite.
    static READ_BUFFER: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Reads and verifies the `n` pages from `first` on with one read of the
/// file, into the thread's buffer, each copied out once into its page.
fn read_run(
    file: &dyn File,
    first: PageId,
    n: usize,
    cipher: Option<&PageCipher>,
) -> Result<Vec<Page>> {
    READ_BUFFER.with_borrow_mut(|buf| {
        if buf.len() < n * PAGE_SIZE {
            buf.resize(n * PAGE_SIZE, 0);
        }
        let buf = &mut buf[..n * PAGE_SIZE];
        match file.read_exact_at(buf, first.offset()) {
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
                let corrupt = |reason| Error::Corrupt {
                    page: PageId(id),
                    reason,
                };
                if let Some(c) = cipher {
                    return c.open(PageId(id), bytes).map_err(corrupt);
                }
                let page = Page::from_bytes(bytes);
                page.verify(PageId(id)).map_err(corrupt)?;
                Ok(page)
            })
            .collect()
    })
}

/// The fewest pages the file grows by: 1 MiB.
const EXTEND_MIN: u64 = 128;
/// The most: 64 MiB.
const EXTEND_MAX: u64 = 8192;

/// Grows the file, `known` pages long, to hold `upto` pages at least —
/// by zeros written ahead, an eighth of its length (1 to 64 MiB) at a
/// time; returns its new length in pages.  A write that allocates the
/// file's blocks costs many times one into blocks it has (on APFS 8 KiB
/// pages written past the end took 47 µs each, into zeros written ahead
/// in 1 MiB pieces 2 µs, the zeros included 8: §M14.20 W7), as
/// PostgreSQL's extension of a relation by many blocks at once and
/// SQLite's `SQLITE_FCNTL_CHUNK_SIZE` know.  Pages past the header's
/// count are not the database's: zeros there are ignored, and later
/// allocations write over them.
fn extend(file: &dyn File, known: u64, upto: u64) -> Result<u64> {
    let have = file.size()? / PAGE_SIZE as u64;
    let from = known.max(have);
    if from >= upto {
        return Ok(from);
    }
    let to = upto.max(from + (from / 8).clamp(EXTEND_MIN, EXTEND_MAX));
    let zeros = vec![0u8; EXTEND_MIN as usize * PAGE_SIZE];
    let mut at = from;
    while at < to {
        let n = (to - at).min(EXTEND_MIN);
        file.write_all_at(&zeros[..n as usize * PAGE_SIZE], at * PAGE_SIZE as u64)?;
        at += n;
    }
    Ok(to)
}

thread_local! {
    /// A thread's buffer for an encrypted page on its way to the file
    /// (the page itself stays in the clear, in the buffer pool).
    static WRITE_BUFFER: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    /// A thread's buffer for a run of pages on its way to the file
    /// ([`Pager::write_run`]).
    static RUN_BUFFER: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn write_raw(
    file: &dyn File,
    id: PageId,
    page: &mut Page,
    cipher: Option<&PageCipher>,
) -> Result<()> {
    page.seal(id);
    let Some(c) = cipher else {
        return Ok(file.write_all_at(page.bytes(), id.offset())?);
    };
    WRITE_BUFFER.with_borrow_mut(|buf| {
        buf.resize(PAGE_SIZE, 0);
        c.seal(id, page, buf)
            .map_err(|reason| Error::Corrupt { page: id, reason })?;
        Ok(file.write_all_at(buf, id.offset())?)
    })
}

/// Reads the free list of `hdr`: the free page numbers and the pages that
/// hold them.
fn read_freelist(
    file: &dyn File,
    hdr: &Header,
    cipher: Option<&PageCipher>,
) -> Result<(BTreeSet<u64>, Vec<u64>)> {
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
        let page = read_raw(file, PageId(id), cipher)?;
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

fn write_header(file: &dyn File, hdr: &Header, cipher: Option<&PageCipher>) -> Result<()> {
    let slot = PageId(hdr.seq % 2);
    let mut page = hdr.encode();
    if let Some(c) = cipher {
        // Under the key the header names: a commit frozen before a new
        // key was made is the old key's (K3f).
        let c = match c.previous() {
            Some(p) if hdr.key_check == p.check_value() => p,
            _ => c,
        };
        let tag = c.header_tag(slot, &tagged(page.payload()));
        page.payload_mut()[TAGGED..TAGGED + 16].copy_from_slice(&tag);
    }
    page.seal(slot);
    Ok(file.write_all_at(page.bytes(), slot.offset())?)
}

/// Reads both header copies and returns the valid one with the highest
/// sequence number.  With `cipher`, an encrypted copy counts as valid
/// only if its tag is the key's (a copy of a file in the clear, or under
/// another key, is told apart afterwards by its fields).
fn read_best_header(file: &dyn File, cipher: &[&PageCipher]) -> Result<Header> {
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
            Ok(h) if h.seq % 2 == slot.0 && !tag_fails(&page, &h, slot, cipher) => {
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

/// The cipher of the database file at `path` — [`CIPHER_NONE`] in the
/// clear —, `None` when there is no file (PLAN.md §M14.30 K2e).  Its
/// headers only are read: no key needed.
pub fn file_cipher(fs: &dyn FileSystem, path: &Path) -> Result<Option<u32>> {
    if !fs.exists(path)? {
        return Ok(None);
    }
    let file = fs.open(path, OpenMode::ReadOnly)?;
    Ok(Some(read_best_header(file.as_ref(), &[])?.cipher))
}

/// Encrypts the database file at `path`, in the clear and closed, under
/// `cipher` (PLAN.md §M14.30 K2e): every page sealed into
/// `<path>.encrypting`, both header copies written for it, the copy
/// synced and renamed over the file — a crash before leaves the file in
/// the clear as it was (and the copy, removed when this runs again).
/// Pages never written stay zeros.  The pages sealed.
pub fn encrypt_file(fs: &dyn FileSystem, path: &Path, cipher: &PageCipher) -> Result<u64> {
    let file = fs.open(path, OpenMode::ReadOnly)?;
    let hdr = read_best_header(file.as_ref(), &[])?;
    if hdr.cipher != CIPHER_NONE {
        return Err(Error::Key(format!(
            "{} is encrypted already",
            path.display()
        )));
    }
    if file.size()? < hdr.page_count * PAGE_SIZE as u64 {
        return Err(Error::Corrupt {
            page: PageId(hdr.page_count - 1),
            reason: "file shorter than its header says",
        });
    }
    read_freelist(file.as_ref(), &hdr, None)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".encrypting");
    let tmp = PathBuf::from(tmp);
    match fs.remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let out = fs.open(&tmp, OpenMode::CreateNew)?;
    const RUN: u64 = 256;
    let mut plain = vec![0u8; RUN as usize * PAGE_SIZE];
    let mut sealed = vec![0u8; RUN as usize * PAGE_SIZE];
    let mut id = FIRST_DATA_PAGE.0;
    let mut count = 0;
    while id < hdr.page_count {
        let n = (hdr.page_count - id).min(RUN) as usize;
        file.read_exact_at(&mut plain[..n * PAGE_SIZE], id * PAGE_SIZE as u64)?;
        for k in 0..n {
            let bytes = &plain[k * PAGE_SIZE..(k + 1) * PAGE_SIZE];
            let to = &mut sealed[k * PAGE_SIZE..(k + 1) * PAGE_SIZE];
            if bytes.iter().all(|&b| b == 0) {
                to.fill(0);
                continue;
            }
            let pid = PageId(id + k as u64);
            let page = Page::from_bytes(bytes);
            page.verify(pid)
                .map_err(|reason| Error::Corrupt { page: pid, reason })?;
            cipher
                .seal(pid, &page, to)
                .map_err(|reason| Error::Corrupt { page: pid, reason })?;
            count += 1;
        }
        out.write_all_at(&sealed[..n * PAGE_SIZE], id * PAGE_SIZE as u64)?;
        id += n as u64;
    }
    let mut h = hdr.clone();
    h.cipher = cipher.code();
    h.key_check = cipher.check_value();
    // Both copies: the older one first, the same state under the
    // sequence number before.
    let mut older = h.clone();
    older.seq = h.seq - 1;
    write_header(out.as_ref(), &older, Some(cipher))?;
    write_header(out.as_ref(), &h, Some(cipher))?;
    out.sync()?;
    drop(out);
    drop(file);
    fs.rename(&tmp, path)?;
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    fs.sync_dir(dir.unwrap_or(Path::new(".")))?;
    Ok(count)
}

/// Whether an encrypted header copy's tag is not what `cipher` gives
/// (under the key it was written with: one under another key is refused
/// by its key check instead).
fn tag_fails(page: &Page, h: &Header, slot: PageId, cipher: &[&PageCipher]) -> bool {
    let Some(c) = cipher
        .iter()
        .find(|c| h.cipher != CIPHER_NONE && h.key_check == c.check_value())
    else {
        return false;
    };
    let p = page.payload();
    !nucleus_crypto::constant_time_eq(&c.header_tag(slot, &tagged(p)), &p[TAGGED..TAGGED + 16])
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
                let mut page = read_raw(file.as_ref(), id, None).unwrap();
                write_u32(page.payload_mut(), 8, v);
                write_raw(file.as_ref(), id, &mut page, None).unwrap();
            }
            file.sync().unwrap();
        };
        let version = || {
            let file = fs.open(path, OpenMode::ReadOnly).unwrap();
            [PageId(0), PageId(1)]
                .map(|id| read_u32(read_raw(file.as_ref(), id, None).unwrap().payload(), 8))
        };
        set_version(1);
        let pager = Pager::open(fs, path).unwrap();
        pager.set_meta(0, 7).unwrap();
        pager.sync().unwrap();
        pager.close().unwrap();
        assert!(version().contains(&FORMAT_VERSION));
        assert_eq!(Pager::open(fs, path).unwrap().meta(0), 7);
        set_version(ENCRYPTED_FORMAT_VERSION);
        assert!(matches!(Pager::open(fs, path), Err(Error::NotADatabase(_))));
        set_version(ENCRYPTED_FORMAT_VERSION + 1);
        assert!(matches!(Pager::open(fs, path), Err(Error::NotADatabase(_))));
    }

    /// An encrypted file's header copies are authenticated (PLAN.md
    /// §M14.30 K2b): one changed and its checksum made right again is
    /// passed over like a torn one; both so changed, the file does not
    /// open.  A page in the clear in an encrypted file does not open.
    #[test]
    fn an_encrypted_files_headers_are_authenticated() {
        let p = SimPlatform::new(1);
        let fs = p.fs();
        fs.create_dir_all(Path::new("/db")).unwrap();
        let path = Path::new("/db/data");
        let key = || Some(PageCipher::new([5; 32]));
        let pager = Pager::create_with(fs, path, key()).unwrap();
        let id = pager.allocate().unwrap();
        let mut page = Page::new(PageType::FIRST_USER);
        page.payload_mut()[..5].copy_from_slice(b"hello");
        pager.write_page(id, &mut page).unwrap();
        pager.set_meta(0, 7).unwrap();
        pager.sync().unwrap();
        pager.set_meta(0, 8).unwrap();
        pager.sync().unwrap();
        drop(pager);
        let pager = Pager::open_with(fs, path, key()).unwrap();
        assert_eq!(pager.meta(0), 8);
        assert_eq!(&pager.read_page(id).unwrap().payload()[..5], b"hello");
        let seq = pager.durable_header().seq;
        drop(pager);

        let file = fs.open(path, OpenMode::ReadWrite).unwrap();
        let tamper = |slot: u64| {
            let mut page = Page::new(PageType::UNUSED);
            file.read_exact_at(page.bytes_mut(), slot * PAGE_SIZE as u64)
                .unwrap();
            write_u64(page.payload_mut(), 48, 1000);
            page.seal(PageId(slot));
            file.write_all_at(page.bytes(), slot * PAGE_SIZE as u64)
                .unwrap();
        };
        tamper(seq % 2);
        let pager = Pager::open_with(fs, path, key()).unwrap();
        assert_eq!(pager.meta(0), 7, "the older copy, the newer passed over");
        drop(pager);
        tamper(1 - seq % 2);
        assert!(Pager::open_with(fs, path, key()).is_err());

        let mut clear = Page::new(PageType::FIRST_USER);
        write_raw(file.as_ref(), id, &mut clear, None).unwrap();
        let c = PageCipher::new([5; 32]);
        assert!(read_raw(file.as_ref(), id, Some(&c)).is_err());
    }
}
