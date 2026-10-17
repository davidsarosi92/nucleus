//! The operating system as a [`Platform`].
//!
//! Built on `std` only, which already does the right thing per OS:
//!
//! - **Durability.**  [`File::sync`] is `std::fs::File::sync_all`: `fsync` on
//!   Linux and d-os, `fcntl(F_FULLFSYNC)` on macOS (plain `fsync` there does
//!   not flush the drive cache), `FlushFileBuffers` on Windows.
//! - **Positional I/O.**  `pread`/`pwrite` on POSIX; on Windows
//!   `ReadFile`/`WriteFile` with the offset in an `OVERLAPPED`.
//! - **Atomic replace.**  `rename` on POSIX; on Windows `std` uses
//!   `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` or its POSIX-semantics successor.
//! - **Directory sync.**  On POSIX the directory is opened and `fsync`ed.  On
//!   Windows a directory cannot be opened as a file, and NTFS journals its
//!   metadata, so [`FileSystem::sync_dir`] does nothing there.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use crate::{Clock, File, FileSystem, OpenMode, Platform, Rng, SplitMix64};

/// The operating system: real files, the real clock, randomness seeded from
/// the OS.
#[derive(Debug)]
pub struct RealPlatform {
    fs: RealFs,
    clock: RealClock,
    rng: SplitMix64,
}

impl RealPlatform {
    /// Creates the platform.  Fails only if the OS refuses to provide a
    /// random seed.
    pub fn new() -> io::Result<Self> {
        let seed = getrandom::u64().map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Self {
            fs: RealFs,
            clock: RealClock {
                origin: Instant::now(),
            },
            rng: SplitMix64::new(seed),
        })
    }
}

impl Platform for RealPlatform {
    fn fs(&self) -> &dyn FileSystem {
        &self.fs
    }
    fn clock(&self) -> &dyn Clock {
        &self.clock
    }
    fn rng(&self) -> &dyn Rng {
        &self.rng
    }
}

/// The OS file system.
#[derive(Debug)]
pub struct RealFs;

/// A lock held through the OS (`flock` / `LockFileEx`) on an open file.
struct RealLock {
    _file: fs::File,
}

impl crate::Lock for RealLock {}

impl FileSystem for RealFs {
    fn lock(&self, path: &Path) -> io::Result<Box<dyn crate::Lock>> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Box::new(RealLock { _file: file })),
            Err(fs::TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{} is locked by another process", path.display()),
            )),
            Err(fs::TryLockError::Error(e)) => Err(e),
        }
    }

    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn File>> {
        let mut o = fs::OpenOptions::new();
        o.read(true);
        match mode {
            OpenMode::ReadOnly => {}
            OpenMode::ReadWrite => {
                o.write(true);
            }
            OpenMode::Create => {
                o.write(true).create(true);
            }
            OpenMode::CreateNew => {
                o.write(true).create_new(true);
            }
        }
        let file = o.open(path)?;
        if file.metadata()?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("{} is a directory", path.display()),
            ));
        }
        Ok(Box::new(RealFile { file }))
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<OsString>> {
        let mut names = fs::read_dir(path)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        path.try_exists()
    }

    #[cfg(unix)]
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        fs::File::open(path)?.sync_all()
    }

    #[cfg(not(unix))]
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        // See the module documentation: nothing to do, but the directory
        // must exist, as on POSIX.
        if fs::metadata(path)?.is_dir() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", path.display()),
            ))
        }
    }
}

#[derive(Debug)]
struct RealFile {
    file: fs::File,
}

impl File for RealFile {
    #[cfg(unix)]
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        std::os::unix::fs::FileExt::read_at(&self.file, buf, offset)
    }

    #[cfg(windows)]
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        // std already turns ERROR_HANDLE_EOF (reading at or past the end)
        // into Ok(0), as on POSIX.
        std::os::windows::fs::FileExt::seek_read(&self.file, buf, offset)
    }

    #[cfg(unix)]
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        std::os::unix::fs::FileExt::write_at(&self.file, buf, offset)
    }

    #[cfg(windows)]
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        std::os::windows::fs::FileExt::seek_write(&self.file, buf, offset)
    }

    fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }
}

/// The OS clock.
#[derive(Debug)]
pub struct RealClock {
    origin: Instant,
}

impl Clock for RealClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
    fn wall(&self) -> SystemTime {
        SystemTime::now()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}
