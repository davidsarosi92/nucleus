//! Files in a pool of slots (PLAN.md §M14.23): the browser edition's
//! file system over the Origin Private File System.
//!
//! A browser opens an OPFS file for synchronous access only from a worker,
//! and only by an asynchronous call — which a synchronous engine cannot
//! wait for mid-statement.  So the host opens a pool of files ahead
//! ([`Slots`]), and [`SlotFs`] keeps the database's files in them: each
//! slot is one file, its path in the slot's header, its bytes after
//! [`HEADER`].  Opening, creating, renaming and removing a file are then
//! header writes on handles already open.  (The idea is SQLite's
//! `opfs-sahpool` VFS, by way of wa-sqlite's access-handle pool.)
//!
//! A header is kept twice, each copy with a generation and a checksum,
//! and written to the copy not holding the latest: a write torn by a
//! crash leaves the other copy — the slot as it was.  A rename writes the
//! new path into the moved file's slot (a newer generation than any
//! before), then frees the slot it replaces; a crash between leaves two
//! slots naming the path, and the newer one wins when the pool is next
//! read.  Directories are not stored: a directory exists where it was
//! made in this run or where a file lies under it.
//!
//! A removed or replaced file stays readable through handles opened
//! before (as on POSIX): its slot is reused only when they are dropped.
//! When every slot is taken, a file is refused (`StorageFull`) until the
//! host adds slots ([`SlotFs::free_slots`] says how many are left).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::{Clock, File, FileSystem, OpenMode, Platform, Rng};

/// The bytes before a slot's file: two header copies.
pub const HEADER: u64 = 4096;
/// One header copy's bytes.
const COPY: usize = 2048;
const MAGIC: &[u8; 8] = b"NCSLOT01";
/// A path's most bytes (what a copy holds besides its fields).
const MAX_PATH: usize = COPY - 8 - 8 - 4 - 8;

/// The host's pool: files opened ahead, addressed by number.  Slots may
/// be added between calls (`count` grows); none is taken away.
pub trait Slots: Send + Sync {
    /// The slots there are.
    fn count(&self) -> usize;
    /// Reads from slot `slot` at `offset`: the bytes read (0 at its end).
    fn read(&self, slot: usize, buf: &mut [u8], offset: u64) -> io::Result<usize>;
    /// Writes `buf` to slot `slot` at `offset`, growing it as needed.
    fn write(&self, slot: usize, buf: &[u8], offset: u64) -> io::Result<()>;
    /// Slot `slot`'s length.
    fn size(&self, slot: usize) -> io::Result<u64>;
    /// Sets slot `slot`'s length.
    fn truncate(&self, slot: usize, len: u64) -> io::Result<()>;
    /// Makes slot `slot`'s writes durable.
    fn flush(&self, slot: usize) -> io::Result<()>;
}

/// Slots in memory (for tests: a pool that outlives the file system
/// opened on it, as the browser's does a page).
#[derive(Default)]
pub struct MemorySlots {
    slots: Mutex<Vec<Vec<u8>>>,
}

impl MemorySlots {
    /// `n` empty slots.
    pub fn new(n: usize) -> Self {
        Self {
            slots: Mutex::new(vec![Vec::new(); n]),
        }
    }

    /// Adds `n` empty slots.
    pub fn add(&self, n: usize) {
        let mut s = self.lock();
        let len = s.len();
        s.resize(len + n, Vec::new());
    }

    /// Writes `buf` at `offset` of slot `slot` as a crash might leave it:
    /// only its first `keep` bytes.
    pub fn torn_write(&self, slot: usize, buf: &[u8], offset: u64, keep: usize) {
        let _ = self.write(slot, &buf[..keep.min(buf.len())], offset);
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Vec<u8>>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn no_slot(slot: usize) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("no slot {slot}"))
}

impl Slots for MemorySlots {
    fn count(&self) -> usize {
        self.lock().len()
    }

    fn read(&self, slot: usize, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let s = self.lock();
        let b = s.get(slot).ok_or_else(|| no_slot(slot))?;
        let start = (offset as usize).min(b.len());
        let n = buf.len().min(b.len() - start);
        buf[..n].copy_from_slice(&b[start..start + n]);
        Ok(n)
    }

    fn write(&self, slot: usize, buf: &[u8], offset: u64) -> io::Result<()> {
        let mut s = self.lock();
        let b = s.get_mut(slot).ok_or_else(|| no_slot(slot))?;
        let start = offset as usize;
        if b.len() < start + buf.len() {
            b.resize(start + buf.len(), 0);
        }
        b[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn size(&self, slot: usize) -> io::Result<u64> {
        let s = self.lock();
        Ok(s.get(slot).ok_or_else(|| no_slot(slot))?.len() as u64)
    }

    fn truncate(&self, slot: usize, len: u64) -> io::Result<()> {
        let mut s = self.lock();
        s.get_mut(slot)
            .ok_or_else(|| no_slot(slot))?
            .resize(len as usize, 0);
        Ok(())
    }

    fn flush(&self, slot: usize) -> io::Result<()> {
        if slot < self.count() {
            Ok(())
        } else {
            Err(no_slot(slot))
        }
    }
}

/// FNV-1a, 64 bits: a header copy's checksum.
fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// A header copy: its generation and path (empty: the slot is free).
fn encode(generation: u64, path: &str) -> Vec<u8> {
    let mut c = Vec::with_capacity(COPY);
    c.extend_from_slice(MAGIC);
    c.extend_from_slice(&generation.to_le_bytes());
    c.extend_from_slice(&(path.len() as u32).to_le_bytes());
    c.extend_from_slice(path.as_bytes());
    let sum = fnv(&c);
    c.extend_from_slice(&sum.to_le_bytes());
    c
}

fn decode(c: &[u8]) -> Option<(u64, String)> {
    if c.len() < 28 || &c[..8] != MAGIC {
        return None;
    }
    let generation = u64::from_le_bytes(c[8..16].try_into().ok()?);
    let len = u32::from_le_bytes(c[16..20].try_into().ok()?) as usize;
    if len > MAX_PATH {
        return None;
    }
    let end = 20 + len;
    let sum = u64::from_le_bytes(c.get(end..end + 8)?.try_into().ok()?);
    if fnv(&c[..end]) != sum {
        return None;
    }
    Some((generation, String::from_utf8(c[20..end].to_vec()).ok()?))
}

/// What a slot's header says.
#[derive(Clone, Copy, Default)]
struct Head {
    /// The latest valid copy's generation (0: none valid).
    generation: u64,
    /// Which copy holds it.
    copy: usize,
}

#[derive(Default)]
struct State {
    /// Each known slot's header.
    heads: Vec<Head>,
    files: BTreeMap<PathBuf, usize>,
    /// Slots holding no file, ready to take.
    free: BTreeSet<usize>,
    /// Open handles of each slot; a slot freed while some are open is
    /// taken only after they close.
    open: BTreeMap<usize, usize>,
    /// Slots freed with handles open.
    freed: BTreeSet<usize>,
    dirs: BTreeSet<PathBuf>,
    locks: BTreeSet<PathBuf>,
    /// The next header generation.
    next: u64,
}

impl State {
    fn dir_exists(&self, p: &Path) -> bool {
        p.as_os_str().is_empty()
            || self.dirs.contains(p)
            || self.files.keys().any(|f| f.starts_with(p) && f != p)
    }
}

/// Files in a pool of slots (see the module's documentation).
pub struct SlotFs {
    slots: Arc<dyn Slots>,
    state: Arc<Mutex<State>>,
}

impl SlotFs {
    /// The files the pool holds, read from its slots' headers.
    pub fn open(slots: Arc<dyn Slots>) -> io::Result<Self> {
        let fs = Self {
            slots,
            state: Arc::default(),
        };
        {
            let mut s = fs.state();
            fs.read_new(&mut s)?;
        }
        Ok(fs)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Slots free to take (the host adds more when they run low).
    pub fn free_slots(&self) -> usize {
        let mut s = self.state();
        let _ = self.read_new(&mut s);
        s.free.len()
    }

    /// Reads the headers of slots not yet known (all, at first).
    fn read_new(&self, s: &mut State) -> io::Result<()> {
        let count = self.slots.count();
        let mut copy = vec![0u8; COPY];
        // Two slots naming one path (a rename a crash cut short): the newer.
        let mut named: BTreeMap<PathBuf, (u64, usize)> = BTreeMap::new();
        let mut stale = Vec::new();
        for slot in s.heads.len()..count {
            let mut head = Head::default();
            let mut path = String::new();
            for c in 0..2 {
                copy.fill(0);
                let n = self.slots.read(slot, &mut copy, (c * COPY) as u64)?;
                if let Some((g, p)) = decode(&copy[..n]).filter(|(g, _)| *g > head.generation) {
                    head = Head {
                        generation: g,
                        copy: c,
                    };
                    path = p;
                }
            }
            s.heads.push(head);
            s.next = s.next.max(head.generation + 1);
            if path.is_empty() {
                s.free.insert(slot);
                continue;
            }
            let p = PathBuf::from(path);
            match named.get(&p) {
                Some(&(g, _)) if g > head.generation => stale.push(slot),
                Some(&(_, other)) => {
                    stale.push(other);
                    named.insert(p, (head.generation, slot));
                }
                None => {
                    named.insert(p, (head.generation, slot));
                }
            }
        }
        for (p, (_, slot)) in named {
            s.files.insert(p, slot);
        }
        for slot in stale {
            self.write_head(s, slot, "")?;
            s.free.insert(slot);
        }
        Ok(())
    }

    /// Writes slot `slot`'s header (path `""`: free) into the copy not
    /// holding its latest, and flushes it.
    fn write_head(&self, s: &mut State, slot: usize, path: &str) -> io::Result<()> {
        if path.len() > MAX_PATH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{path}: a path in the pool holds at most {MAX_PATH} bytes"),
            ));
        }
        let generation = s.next;
        s.next += 1;
        let head = &mut s.heads[slot];
        let copy = if head.generation == 0 {
            0
        } else {
            1 - head.copy
        };
        self.slots
            .write(slot, &encode(generation, path), (copy * COPY) as u64)?;
        self.slots.flush(slot)?;
        *head = Head { generation, copy };
        Ok(())
    }

    /// Frees `slot`: now if no handle has it open, else when the last
    /// closes.
    fn release(&self, s: &mut State, slot: usize) -> io::Result<()> {
        self.write_head(s, slot, "")?;
        if s.open.contains_key(&slot) {
            s.freed.insert(slot);
        } else {
            self.slots.truncate(slot, HEADER)?;
            s.free.insert(slot);
        }
        Ok(())
    }
}

fn err(kind: io::ErrorKind, path: &Path, what: &str) -> io::Error {
    io::Error::new(kind, format!("{}: {what}", path.display()))
}

/// The path's names joined by `/`, without root or `.`; `..` is refused.
fn normalize(path: &Path) -> io::Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(err(
                    io::ErrorKind::InvalidInput,
                    path,
                    "`..` is not supported in the pool",
                ));
            }
        }
    }
    Ok(out)
}

fn parent_of(p: &Path) -> &Path {
    p.parent().unwrap_or(Path::new(""))
}

fn text(p: &Path) -> io::Result<&str> {
    p.to_str()
        .ok_or_else(|| err(io::ErrorKind::InvalidInput, p, "not UTF-8"))
}

impl FileSystem for SlotFs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn File>> {
        let p = normalize(path)?;
        let mut s = self.state();
        if p.as_os_str().is_empty() || s.dirs.contains(&p) {
            return Err(err(io::ErrorKind::IsADirectory, path, "is a directory"));
        }
        let slot = match (s.files.get(&p).copied(), mode) {
            (Some(_), OpenMode::CreateNew) => {
                return Err(err(io::ErrorKind::AlreadyExists, path, "exists"));
            }
            (Some(slot), _) => slot,
            (None, OpenMode::ReadOnly | OpenMode::ReadWrite) => {
                return Err(err(io::ErrorKind::NotFound, path, "no such file"));
            }
            (None, OpenMode::Create | OpenMode::CreateNew) => {
                if !s.dir_exists(parent_of(&p)) {
                    return Err(err(
                        io::ErrorKind::NotFound,
                        path,
                        "parent directory does not exist",
                    ));
                }
                if s.free.is_empty() {
                    self.read_new(&mut s)?;
                }
                let Some(slot) = s.free.pop_first() else {
                    return Err(err(
                        io::ErrorKind::StorageFull,
                        path,
                        "every slot of the pool is taken",
                    ));
                };
                self.slots.truncate(slot, HEADER)?;
                if let Err(e) = self.write_head(&mut s, slot, text(&p)?) {
                    s.free.insert(slot);
                    return Err(e);
                }
                s.files.insert(p, slot);
                slot
            }
        };
        *s.open.entry(slot).or_default() += 1;
        Ok(Box::new(SlotFile {
            slots: Arc::clone(&self.slots),
            state: Arc::clone(&self.state),
            slot,
            writable: mode != OpenMode::ReadOnly,
        }))
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let p = normalize(path)?;
        let mut s = self.state();
        let Some(slot) = s.files.remove(&p) else {
            return Err(err(io::ErrorKind::NotFound, path, "no such file"));
        };
        self.release(&mut s, slot)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (f, t) = (normalize(from)?, normalize(to)?);
        let mut s = self.state();
        if !s.dir_exists(parent_of(&t)) {
            return Err(err(
                io::ErrorKind::NotFound,
                to,
                "parent directory does not exist",
            ));
        }
        let Some(&slot) = s.files.get(&f) else {
            return Err(err(io::ErrorKind::NotFound, from, "no such file"));
        };
        if f == t {
            return Ok(());
        }
        // The moved file's header first: from here on it is `to`.
        self.write_head(&mut s, slot, text(&t)?)?;
        s.files.remove(&f);
        if let Some(old) = s.files.insert(t, slot) {
            self.release(&mut s, old)?;
        }
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        let p = normalize(path)?;
        let mut s = self.state();
        let mut at = PathBuf::new();
        for c in p.components() {
            at.push(c);
            if s.files.contains_key(&at) {
                return Err(err(io::ErrorKind::AlreadyExists, &at, "is a file"));
            }
            s.dirs.insert(at.clone());
        }
        Ok(())
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<OsString>> {
        let p = normalize(path)?;
        let s = self.state();
        if !s.dir_exists(&p) {
            return Err(err(io::ErrorKind::NotFound, path, "no such directory"));
        }
        let mut names = BTreeSet::new();
        for q in s.files.keys().chain(s.dirs.iter()) {
            // The entry under `p` that `q` is or lies in.
            if let Ok(rest) = q.strip_prefix(&p)
                && let Some(Component::Normal(first)) = rest.components().next()
            {
                names.insert(OsString::from(first));
            }
        }
        Ok(names.into_iter().collect())
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        let p = normalize(path)?;
        let s = self.state();
        Ok(s.files.contains_key(&p) || s.dir_exists(&p))
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        // Each header is flushed as it is written.
        let p = normalize(path)?;
        if self.state().dir_exists(&p) {
            Ok(())
        } else {
            Err(err(io::ErrorKind::NotFound, path, "no such directory"))
        }
    }

    fn lock(&self, path: &Path) -> io::Result<Box<dyn crate::Lock>> {
        // The pool is one page's (its handles are exclusive): the lock
        // keeps out only others in it.
        let p = normalize(path)?;
        let mut s = self.state();
        if !s.dir_exists(parent_of(&p)) {
            return Err(err(
                io::ErrorKind::NotFound,
                path,
                "parent directory does not exist",
            ));
        }
        if !s.locks.insert(p.clone()) {
            return Err(err(io::ErrorKind::WouldBlock, path, "locked"));
        }
        Ok(Box::new(SlotLock {
            state: Arc::clone(&self.state),
            path: p,
        }))
    }
}

struct SlotLock {
    state: Arc<Mutex<State>>,
    path: PathBuf,
}

impl crate::Lock for SlotLock {}

impl Drop for SlotLock {
    fn drop(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .locks
            .remove(&self.path);
    }
}

struct SlotFile {
    slots: Arc<dyn Slots>,
    state: Arc<Mutex<State>>,
    slot: usize,
    writable: bool,
}

impl SlotFile {
    fn writable(&self) -> io::Result<()> {
        if self.writable {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the file is open for reading",
            ))
        }
    }
}

fn past(offset: u64) -> io::Result<u64> {
    offset
        .checked_add(HEADER)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset too large"))
}

impl File for SlotFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.slots.read(self.slot, buf, past(offset)?)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        self.writable()?;
        self.slots.write(self.slot, buf, past(offset)?)?;
        Ok(buf.len())
    }

    fn sync(&self) -> io::Result<()> {
        self.slots.flush(self.slot)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.slots.size(self.slot)?.saturating_sub(HEADER))
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.writable()?;
        self.slots.truncate(self.slot, past(len)?)
    }
}

impl Drop for SlotFile {
    fn drop(&mut self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let left = s.open.get_mut(&self.slot).map(|n| {
            *n -= 1;
            *n
        });
        if left == Some(0) {
            s.open.remove(&self.slot);
            if s.freed.remove(&self.slot) {
                let _ = self.slots.truncate(self.slot, HEADER);
                s.free.insert(self.slot);
            }
        }
    }
}

/// Files in a pool of slots, under some clock and randomness — the
/// machine's by default.
pub struct SlotPlatform<C = crate::real::RealPlatform> {
    fs: SlotFs,
    time: C,
}

impl SlotPlatform {
    /// The pool's files under the machine's clock and randomness.
    pub fn new(slots: Arc<dyn Slots>) -> io::Result<Self> {
        Self::with(slots, crate::real::RealPlatform::new()?)
    }
}

impl<C: Platform> SlotPlatform<C> {
    /// The pool's files; time and randomness from `time` (its files
    /// unused).
    pub fn with(slots: Arc<dyn Slots>, time: C) -> io::Result<Self> {
        Ok(Self {
            fs: SlotFs::open(slots)?,
            time,
        })
    }

    /// The file system.
    pub fn slot_fs(&self) -> &SlotFs {
        &self.fs
    }
}

impl<C: Platform> Platform for SlotPlatform<C> {
    fn fs(&self) -> &dyn FileSystem {
        &self.fs
    }
    fn clock(&self) -> &dyn Clock {
        self.time.clock()
    }
    fn rng(&self) -> &dyn Rng {
        self.time.rng()
    }
    fn parallelism(&self) -> usize {
        self.time.parallelism()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(n: usize) -> (Arc<MemorySlots>, SlotFs) {
        let slots = Arc::new(MemorySlots::new(n));
        let fs = SlotFs::open(slots.clone()).unwrap();
        (slots, fs)
    }

    fn put(fs: &SlotFs, path: &str, bytes: &[u8]) {
        let f = fs.open(Path::new(path), OpenMode::Create).unwrap();
        f.write_all_at(bytes, 0).unwrap();
        f.sync().unwrap();
    }

    fn get(fs: &SlotFs, path: &str) -> Vec<u8> {
        let f = fs.open(Path::new(path), OpenMode::ReadOnly).unwrap();
        let mut b = vec![0; f.size().unwrap() as usize];
        f.read_exact_at(&mut b, 0).unwrap();
        b
    }

    #[test]
    fn files_outlive_the_file_system_opened_on_the_pool() {
        let (slots, fs) = pool(4);
        fs.create_dir_all(Path::new("/db/wal")).unwrap();
        put(&fs, "/db/data", b"pages");
        put(&fs, "/db/wal/0001", b"log");
        put(&fs, "/db/tmp", b"x");
        fs.rename(Path::new("/db/tmp"), Path::new("/db/data"))
            .unwrap();
        fs.remove_file(Path::new("/db/wal/0001")).unwrap();
        assert_eq!(fs.free_slots(), 3);
        drop(fs);
        let fs = SlotFs::open(slots).unwrap();
        assert_eq!(get(&fs, "/db/data"), b"x");
        assert!(!fs.exists(Path::new("/db/wal/0001")).unwrap());
        assert_eq!(fs.list_dir(Path::new("/db")).unwrap(), vec!["data"]);
        assert_eq!(fs.free_slots(), 3);
    }

    #[test]
    fn a_full_pool_refuses_until_slots_are_added() {
        let (slots, fs) = pool(1);
        put(&fs, "/a", b"1");
        let e = fs.open(Path::new("/b"), OpenMode::Create).err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::StorageFull);
        slots.add(2);
        assert_eq!(fs.free_slots(), 2);
        put(&fs, "/b", b"2");
        assert_eq!(get(&fs, "/b"), b"2");
    }

    #[test]
    fn a_removed_file_keeps_its_slot_while_open() {
        let (_, fs) = pool(1);
        put(&fs, "/a", b"kept");
        let f = fs.open(Path::new("/a"), OpenMode::ReadOnly).unwrap();
        fs.remove_file(Path::new("/a")).unwrap();
        assert_eq!(fs.free_slots(), 0);
        let mut b = [0u8; 4];
        f.read_exact_at(&mut b, 0).unwrap();
        assert_eq!(&b, b"kept");
        drop(f);
        assert_eq!(fs.free_slots(), 1);
    }

    #[test]
    fn a_torn_header_leaves_the_slot_as_it_was() {
        let (slots, fs) = pool(2);
        put(&fs, "/a", b"1");
        let slot = *fs.state().files.get(Path::new("a")).unwrap();
        // A rename's header write torn: the other copy still says `/a`.
        let head = fs.state().heads[slot];
        let next = encode(head.generation + 1, "b");
        slots.torn_write(slot, &next, ((1 - head.copy) * COPY) as u64, 20);
        drop(fs);
        let fs = SlotFs::open(slots).unwrap();
        assert_eq!(get(&fs, "/a"), b"1");
        assert!(!fs.exists(Path::new("/b")).unwrap());
    }

    #[test]
    fn a_rename_cut_short_keeps_the_new_file() {
        let (slots, fs) = pool(3);
        put(&fs, "/t", b"old");
        put(&fs, "/n", b"new");
        let (old, new) = {
            let s = fs.state();
            (s.files[Path::new("t")], s.files[Path::new("n")])
        };
        // The moved file's header written, the replaced one's not freed.
        let mut s = fs.state();
        fs.write_head(&mut s, new, "t").unwrap();
        drop(s);
        drop(fs);
        let fs = SlotFs::open(slots.clone()).unwrap();
        assert_eq!(get(&fs, "/t"), b"new");
        assert!(!fs.exists(Path::new("/n")).unwrap());
        assert!(fs.state().free.contains(&old));
        // The stale slot was freed on disk too.
        drop(fs);
        let fs = SlotFs::open(slots).unwrap();
        assert_eq!(fs.free_slots(), 2);
    }
}
