//! A database in memory (PLAN.md §M14.23): the platform the embedded
//! editions open `:memory:` on.
//!
//! [`MemoryFs`] keeps each file as one byte vector — no second, durable
//! image as the simulation keeps, no faults — so a database in memory
//! costs what its files hold.  A sync does nothing (there is no crash to
//! survive: the files go when the process does).  Time and randomness are
//! the machine's ([`RealPlatform`](crate::real::RealPlatform)'s), or any
//! others given.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use crate::{Clock, File, FileSystem, OpenMode, Platform, Rng};

/// A file's bytes, shared by its open handles: a removed or renamed file
/// stays readable through a handle opened before (as on POSIX).
type Bytes = Arc<RwLock<Vec<u8>>>;

#[derive(Default)]
struct State {
    files: BTreeMap<PathBuf, Bytes>,
    dirs: BTreeSet<PathBuf>,
    locks: BTreeSet<PathBuf>,
}

impl State {
    fn dir_exists(&self, p: &Path) -> bool {
        p.as_os_str().is_empty() || self.dirs.contains(p)
    }
}

/// A file system in memory (see the module's documentation).
#[derive(Clone, Default)]
pub struct MemoryFs {
    state: Arc<Mutex<State>>,
}

impl MemoryFs {
    /// An empty one: only the root directory.
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Every change completes under the lock, so a panic elsewhere
        // leaves nothing half done.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The bytes all its files hold.
    pub fn bytes(&self) -> u64 {
        self.state()
            .files
            .values()
            .map(|b| b.read().unwrap_or_else(|e| e.into_inner()).len() as u64)
            .sum()
    }
}

fn err(kind: io::ErrorKind, path: &Path, what: &str) -> io::Error {
    io::Error::new(kind, format!("{}: {what}", path.display()))
}

/// The path's names, without root, `.` or prefix; `..` is refused.
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
                    "`..` is not supported in memory",
                ));
            }
        }
    }
    Ok(out)
}

fn parent_of(p: &Path) -> &Path {
    p.parent().unwrap_or(Path::new(""))
}

impl FileSystem for MemoryFs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn File>> {
        let p = normalize(path)?;
        let mut s = self.state();
        if s.dirs.contains(&p) || p.as_os_str().is_empty() {
            return Err(err(io::ErrorKind::IsADirectory, path, "is a directory"));
        }
        let bytes = match (s.files.get(&p), mode) {
            (Some(_), OpenMode::CreateNew) => {
                return Err(err(io::ErrorKind::AlreadyExists, path, "exists"));
            }
            (Some(b), _) => Arc::clone(b),
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
                let b = Bytes::default();
                s.files.insert(p, Arc::clone(&b));
                b
            }
        };
        Ok(Box::new(MemoryFile {
            bytes,
            writable: mode != OpenMode::ReadOnly,
        }))
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let p = normalize(path)?;
        match self.state().files.remove(&p) {
            Some(_) => Ok(()),
            None => Err(err(io::ErrorKind::NotFound, path, "no such file")),
        }
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
        let Some(b) = s.files.remove(&f) else {
            return Err(err(io::ErrorKind::NotFound, from, "no such file"));
        };
        s.files.insert(t, b);
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
        let mut names: Vec<OsString> = s
            .files
            .keys()
            .chain(s.dirs.iter())
            .filter(|q| parent_of(q) == p && !q.as_os_str().is_empty())
            .filter_map(|q| q.file_name().map(OsString::from))
            .collect();
        names.sort();
        Ok(names)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        let p = normalize(path)?;
        let s = self.state();
        Ok(s.files.contains_key(&p) || s.dir_exists(&p))
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        let p = normalize(path)?;
        if self.state().dir_exists(&p) {
            Ok(())
        } else {
            Err(err(io::ErrorKind::NotFound, path, "no such directory"))
        }
    }

    fn writes_in_place(&self) -> bool {
        true
    }

    fn lock(&self, path: &Path) -> io::Result<Box<dyn crate::Lock>> {
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
        Ok(Box::new(MemoryLock {
            state: Arc::clone(&self.state),
            path: p,
        }))
    }
}

struct MemoryLock {
    state: Arc<Mutex<State>>,
    path: PathBuf,
}

impl crate::Lock for MemoryLock {}

impl Drop for MemoryLock {
    fn drop(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .locks
            .remove(&self.path);
    }
}

struct MemoryFile {
    bytes: Bytes,
    writable: bool,
}

impl MemoryFile {
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

fn index(offset: u64) -> io::Result<usize> {
    usize::try_from(offset)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset past memory"))
}

impl File for MemoryFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let b = self.bytes.read().unwrap_or_else(|e| e.into_inner());
        let start = index(offset)?.min(b.len());
        let n = buf.len().min(b.len() - start);
        buf[..n].copy_from_slice(&b[start..start + n]);
        Ok(n)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        self.writable()?;
        let start = index(offset)?;
        let end = start
            .checked_add(buf.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset past memory"))?;
        let mut b = self.bytes.write().unwrap_or_else(|e| e.into_inner());
        if b.len() < end {
            b.resize(end, 0);
        }
        b[start..end].copy_from_slice(buf);
        Ok(buf.len())
    }

    fn sync(&self) -> io::Result<()> {
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.bytes.read().unwrap_or_else(|e| e.into_inner()).len() as u64)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.writable()?;
        let n = index(len)?;
        self.bytes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .resize(n, 0);
        Ok(())
    }
}

/// Files in memory, under some clock and randomness — the machine's by
/// default.
pub struct MemoryPlatform<C = crate::real::RealPlatform> {
    fs: MemoryFs,
    time: C,
}

impl MemoryPlatform {
    /// An empty file system under the machine's clock and randomness.
    pub fn new() -> io::Result<Self> {
        Ok(Self::with(crate::real::RealPlatform::new()?))
    }
}

impl<C: Platform> MemoryPlatform<C> {
    /// An empty file system; time and randomness from `time` (its files
    /// unused).
    pub fn with(time: C) -> Self {
        Self {
            fs: MemoryFs::new(),
            time,
        }
    }

    /// The file system.
    pub fn memory_fs(&self) -> &MemoryFs {
        &self.fs
    }
}

impl<C: Platform> Platform for MemoryPlatform<C> {
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
