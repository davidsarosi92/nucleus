//! The simulated file system.
//!
//! Every file has two images: **current** (what reads return) and
//! **durable** (what survives a crash).  Writes change the current image and
//! queue a pending operation; [`File::sync`] applies the pending operations
//! to the durable image.  The namespace (which paths exist) has the same two
//! layers, and [`FileSystem::sync_dir`] makes one directory's entries
//! durable.
//!
//! A crash replaces everything current with what is durable — or, in
//! [`CrashMode::Torn`], with a random mix: each unsynced write survives,
//! vanishes, or survives in part (sector by sector), and each unsynced
//! directory change survives or not.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::{File, FileSystem, OpenMode, Rng, SplitMix64};

/// The unit in which a torn crash keeps or loses parts of a write.
pub const SECTOR_SIZE: u64 = 512;

/// What a crash does to state that was not yet durable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CrashMode {
    /// Every unsynced write and directory change is lost.
    #[default]
    DropUnsynced,
    /// Each unsynced write independently survives, is lost, or survives only
    /// in some of its sectors; each unsynced length change and directory
    /// change independently survives or is lost.
    Torn,
}

/// Which faults the simulated file system injects.  Probabilities are per
/// million operations; zero (the default) means never.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Faults {
    /// A read fails.
    pub read_error_ppm: u32,
    /// A write fails before changing anything.
    pub write_error_ppm: u32,
    /// A file or directory sync fails.  A failed file sync **forgets** the
    /// pending writes: they stay readable but never become durable.
    pub sync_error_ppm: u32,
    /// What the next crash does.
    pub crash_mode: CrashMode,
}

/// Why a simulated operation failed, when the simulation (not the caller)
/// caused it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimFault {
    /// The operation triggered a scheduled crash ([`SimFs::crash_after_ops`]).
    Crash,
    /// The handle was opened before a crash; the "process" it belonged to is
    /// gone.
    StaleHandle,
    /// An error injected by [`Faults`].
    Injected,
}

impl fmt::Display for SimFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SimFault::Crash => "simulated crash",
            SimFault::StaleHandle => "handle opened before a simulated crash",
            SimFault::Injected => "injected I/O error",
        })
    }
}

impl std::error::Error for SimFault {}

/// The [`SimFault`] behind `err`, if the simulation caused it.
pub fn sim_fault(err: &io::Error) -> Option<SimFault> {
    err.get_ref()?.downcast_ref::<SimFault>().copied()
}

fn fault(f: SimFault) -> io::Error {
    io::Error::other(f)
}

fn err(kind: io::ErrorKind, path: &Path, what: &str) -> io::Error {
    io::Error::new(kind, format!("{}: {what}", path.display()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Entry {
    Dir,
    File(u64),
}

#[derive(Debug)]
enum Pending {
    Write { offset: u64, data: Vec<u8> },
    SetLen(u64),
}

#[derive(Debug, Default)]
struct FileData {
    current: Vec<u8>,
    durable: Vec<u8>,
    pending: Vec<Pending>,
}

#[derive(Debug)]
struct State {
    /// Paths are normalised (see `normalize`); the root is the empty path
    /// and is implicit.  BTreeMaps, never HashMaps: iteration order must not
    /// depend on anything but the operations performed.
    current: BTreeMap<PathBuf, Entry>,
    durable: BTreeMap<PathBuf, Entry>,
    files: BTreeMap<u64, FileData>,
    next_file_id: u64,
    epoch: u64,
    ops: u64,
    crash_at: Option<u64>,
    crashes: u64,
    faults: Faults,
    rng: SplitMix64,
}

/// The simulated file system.  Cloning gives another handle to the same one.
#[derive(Clone, Debug)]
pub struct SimFs {
    state: Arc<Mutex<State>>,
}

impl SimFs {
    /// An empty file system whose randomness (faults, torn crashes) derives
    /// from `seed`.
    pub fn new(seed: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                current: BTreeMap::new(),
                durable: BTreeMap::new(),
                files: BTreeMap::new(),
                next_file_id: 1,
                epoch: 0,
                ops: 0,
                crash_at: None,
                crashes: 0,
                faults: Faults::default(),
                rng: SplitMix64::new(seed),
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A test that panicked while holding the lock leaves consistent
        // state behind (every mutation completes under the lock), so the
        // poison flag carries no information here.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The faults currently injected.
    pub fn faults(&self) -> Faults {
        self.lock().faults
    }

    /// Changes the faults injected from now on.
    pub fn set_faults(&self, faults: Faults) {
        self.lock().faults = faults;
    }

    /// Crashes now: everything not durable is lost (or torn, per
    /// [`Faults::crash_mode`]) and every open handle becomes stale.  The file
    /// system stays usable, as after a restart.
    pub fn crash(&self) {
        self.lock().crash();
    }

    /// Schedules a crash: the `n`-th operation from now (counting from 1)
    /// crashes instead of running and fails with [`SimFault::Crash`].
    /// Every call on the file system or a file is one operation.
    pub fn crash_after_ops(&self, n: u64) {
        let mut s = self.lock();
        s.crash_at = Some(s.ops + n.max(1));
    }

    /// Operations performed so far.
    pub fn op_count(&self) -> u64 {
        self.lock().ops
    }

    /// Crashes so far.
    pub fn crash_count(&self) -> u64 {
        self.lock().crashes
    }
}

/// Simulated paths are component lists: `/a/b`, `a/b` and `a/./b` are the
/// same path, and `..` is refused (resolving it would need symlink rules the
/// simulation does not model).
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
                    "`..` is not supported in simulated paths",
                ));
            }
        }
    }
    Ok(out)
}

fn is_root(p: &Path) -> bool {
    p.as_os_str().is_empty()
}

fn parent_of(p: &Path) -> &Path {
    p.parent().unwrap_or(Path::new(""))
}

fn write_into(image: &mut Vec<u8>, offset: u64, data: &[u8]) {
    let start = usize::try_from(offset).expect("simulated file offset exceeds usize");
    let end = start + data.len();
    if image.len() < end {
        image.resize(end, 0);
    }
    image[start..end].copy_from_slice(data);
}

fn set_len(image: &mut Vec<u8>, len: u64) {
    image.resize(
        usize::try_from(len).expect("simulated file length exceeds usize"),
        0,
    );
}

fn apply(image: &mut Vec<u8>, op: &Pending) {
    match op {
        Pending::Write { offset, data } => write_into(image, *offset, data),
        Pending::SetLen(len) => set_len(image, *len),
    }
}

impl State {
    /// Counts one operation, running a scheduled crash if it is due.
    fn tick(&mut self) -> io::Result<()> {
        self.ops += 1;
        if self.crash_at.is_some_and(|at| self.ops >= at) {
            self.crash_at = None;
            self.crash();
            return Err(fault(SimFault::Crash));
        }
        Ok(())
    }

    /// Draws a fault with probability `ppm`.  Draws nothing when `ppm` is 0,
    /// so turning faults off does not shift the random sequence.
    fn inject(&self, ppm: u32) -> io::Result<()> {
        if ppm > 0 && self.rng.chance_ppm(ppm) {
            Err(fault(SimFault::Injected))
        } else {
            Ok(())
        }
    }

    fn coin(&self) -> bool {
        self.rng.next_u64() & 1 == 1
    }

    fn dir_exists(&self, p: &Path) -> bool {
        is_root(p) || self.current.get(p) == Some(&Entry::Dir)
    }

    fn crash(&mut self) {
        let torn = self.faults.crash_mode == CrashMode::Torn;

        // The namespace: durable entries, plus (torn) a random subset of the
        // unsynced changes.
        let mut survivors = self.durable.clone();
        if torn {
            let paths: BTreeSet<PathBuf> = self
                .current
                .keys()
                .chain(self.durable.keys())
                .cloned()
                .collect();
            for p in paths {
                let cur = self.current.get(&p).copied();
                if cur != self.durable.get(&p).copied() && self.coin() {
                    match cur {
                        Some(e) => survivors.insert(p, e),
                        None => survivors.remove(&p),
                    };
                }
            }
        }
        // An entry whose parent directory did not survive did not either.
        // Sorted order visits parents before children.
        let mut namespace = BTreeMap::new();
        for (p, e) in survivors {
            let parent = parent_of(&p);
            if is_root(parent) || namespace.get(parent) == Some(&Entry::Dir) {
                namespace.insert(p, e);
            }
        }

        // File contents: the durable image, plus (torn) a random subset of
        // the pending operations, writes possibly in part.
        for data in self.files.values_mut() {
            let mut image = std::mem::take(&mut data.durable);
            for op in std::mem::take(&mut data.pending) {
                if !torn || self.rng.next_u64() & 1 == 0 {
                    continue;
                }
                match op {
                    Pending::Write { offset, data } => {
                        // Sector-aligned pieces, each kept or lost alone.
                        let mut start = 0usize;
                        while start < data.len() {
                            let at = offset + start as u64;
                            let room = SECTOR_SIZE - at % SECTOR_SIZE;
                            let end = data.len().min(start + room as usize);
                            if self.rng.next_u64() & 1 == 1 {
                                write_into(&mut image, at, &data[start..end]);
                            }
                            start = end;
                        }
                    }
                    op @ Pending::SetLen(_) => apply(&mut image, &op),
                }
            }
            data.current = image.clone();
            data.durable = image;
        }

        // Files no surviving path names are gone.
        let live: BTreeSet<u64> = namespace
            .values()
            .filter_map(|e| match e {
                Entry::File(id) => Some(*id),
                Entry::Dir => None,
            })
            .collect();
        self.files.retain(|id, _| live.contains(id));

        self.current = namespace.clone();
        self.durable = namespace;
        self.epoch += 1;
        self.crashes += 1;
    }
}

impl FileSystem for SimFs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn File>> {
        let p = normalize(path)?;
        let mut s = self.lock();
        s.tick()?;
        let id = match (s.current.get(&p).copied(), mode) {
            _ if is_root(&p) => {
                return Err(err(io::ErrorKind::IsADirectory, path, "is a directory"));
            }
            (Some(Entry::Dir), _) => {
                return Err(err(io::ErrorKind::IsADirectory, path, "is a directory"));
            }
            (Some(Entry::File(_)), OpenMode::CreateNew) => {
                return Err(err(io::ErrorKind::AlreadyExists, path, "already exists"));
            }
            (Some(Entry::File(id)), _) => id,
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
                let id = s.next_file_id;
                s.next_file_id += 1;
                s.files.insert(id, FileData::default());
                s.current.insert(p, Entry::File(id));
                id
            }
        };
        Ok(Box::new(SimFile {
            state: Arc::clone(&self.state),
            id,
            epoch: s.epoch,
            writable: mode != OpenMode::ReadOnly,
        }))
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let p = normalize(path)?;
        let mut s = self.lock();
        s.tick()?;
        match s.current.get(&p) {
            Some(Entry::File(_)) => {
                s.current.remove(&p);
                Ok(())
            }
            Some(Entry::Dir) => Err(err(io::ErrorKind::IsADirectory, path, "is a directory")),
            None if is_root(&p) => Err(err(io::ErrorKind::IsADirectory, path, "is a directory")),
            None => Err(err(io::ErrorKind::NotFound, path, "no such file")),
        }
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let f = normalize(from)?;
        let t = normalize(to)?;
        let mut s = self.lock();
        s.tick()?;
        let id = match s.current.get(&f) {
            Some(Entry::File(id)) => *id,
            Some(Entry::Dir) => {
                return Err(err(
                    io::ErrorKind::Unsupported,
                    from,
                    "renaming directories is not simulated",
                ));
            }
            None => return Err(err(io::ErrorKind::NotFound, from, "no such file")),
        };
        if is_root(&t) || s.current.get(&t) == Some(&Entry::Dir) {
            return Err(err(io::ErrorKind::IsADirectory, to, "is a directory"));
        }
        if !s.dir_exists(parent_of(&t)) {
            return Err(err(
                io::ErrorKind::NotFound,
                to,
                "parent directory does not exist",
            ));
        }
        s.current.remove(&f);
        s.current.insert(t, Entry::File(id));
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        let p = normalize(path)?;
        let mut s = self.lock();
        s.tick()?;
        let mut prefix = PathBuf::new();
        for c in p.components() {
            prefix.push(c);
            match s.current.get(&prefix) {
                Some(Entry::Dir) => {}
                Some(Entry::File(_)) => {
                    return Err(err(io::ErrorKind::NotADirectory, &prefix, "is a file"));
                }
                None => {
                    s.current.insert(prefix.clone(), Entry::Dir);
                }
            }
        }
        Ok(())
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<OsString>> {
        let p = normalize(path)?;
        let mut s = self.lock();
        s.tick()?;
        match s.current.get(&p) {
            _ if is_root(&p) => {}
            Some(Entry::Dir) => {}
            Some(Entry::File(_)) => {
                return Err(err(io::ErrorKind::NotADirectory, path, "is a file"));
            }
            None => return Err(err(io::ErrorKind::NotFound, path, "no such directory")),
        }
        // Already sorted: BTreeMap order, and children of one parent compare
        // by their last component.
        Ok(s.current
            .keys()
            .filter(|k| !is_root(k) && parent_of(k) == p)
            .filter_map(|k| k.file_name().map(|n| n.to_os_string()))
            .collect())
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        let p = normalize(path)?;
        let mut s = self.lock();
        s.tick()?;
        Ok(is_root(&p) || s.current.contains_key(&p))
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        let p = normalize(path)?;
        let mut s = self.lock();
        s.tick()?;
        if !s.dir_exists(&p) {
            return Err(err(io::ErrorKind::NotFound, path, "no such directory"));
        }
        s.inject(s.faults.sync_error_ppm)?;
        let children: BTreeSet<PathBuf> = s
            .current
            .keys()
            .chain(s.durable.keys())
            .filter(|k| !is_root(k) && parent_of(k) == p)
            .cloned()
            .collect();
        for k in children {
            match s.current.get(&k).copied() {
                Some(e) => s.durable.insert(k, e),
                None => s.durable.remove(&k),
            };
        }
        Ok(())
    }
}

#[derive(Debug)]
struct SimFile {
    state: Arc<Mutex<State>>,
    id: u64,
    epoch: u64,
    writable: bool,
}

impl SimFile {
    /// The state, after checking the handle is from this "process" and
    /// counting the operation.
    fn begin(&self) -> io::Result<MutexGuard<'_, State>> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.epoch != self.epoch {
            return Err(fault(SimFault::StaleHandle));
        }
        s.tick()?;
        Ok(s)
    }

    fn check_writable(&self) -> io::Result<()> {
        if self.writable {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "file opened read-only",
            ))
        }
    }
}

/// A removed file's data outlives its name for as long as a handle may use
/// it (POSIX semantics); only a crash collects it, and a crash also makes
/// every handle stale — so the data is always there for a live handle.
fn data(s: &mut State, id: u64) -> &mut FileData {
    s.files.get_mut(&id).expect("live handle to collected file")
}

impl File for SimFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let mut s = self.begin()?;
        s.inject(s.faults.read_error_ppm)?;
        let image = &data(&mut s, self.id).current;
        let Ok(start) = usize::try_from(offset) else {
            return Ok(0);
        };
        if start >= image.len() {
            return Ok(0);
        }
        let n = buf.len().min(image.len() - start);
        buf[..n].copy_from_slice(&image[start..start + n]);
        Ok(n)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        let mut s = self.begin()?;
        self.check_writable()?;
        s.inject(s.faults.write_error_ppm)?;
        let d = data(&mut s, self.id);
        write_into(&mut d.current, offset, buf);
        d.pending.push(Pending::Write {
            offset,
            data: buf.to_vec(),
        });
        Ok(buf.len())
    }

    fn sync(&self) -> io::Result<()> {
        let mut s = self.begin()?;
        let failed = s.inject(s.faults.sync_error_ppm);
        let d = data(&mut s, self.id);
        if failed.is_err() {
            // fsyncgate: the kernel dropped the dirty pages' error state and
            // will never write them.  Readers still see them.
            d.pending.clear();
            return failed;
        }
        for op in std::mem::take(&mut d.pending) {
            apply(&mut d.durable, &op);
        }
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        let mut s = self.begin()?;
        Ok(data(&mut s, self.id).current.len() as u64)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        let mut s = self.begin()?;
        self.check_writable()?;
        s.inject(s.faults.write_error_ppm)?;
        let d = data(&mut s, self.id);
        set_len(&mut d.current, len);
        d.pending.push(Pending::SetLen(len));
        Ok(())
    }
}
