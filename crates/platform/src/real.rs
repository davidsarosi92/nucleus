//! The operating system as a [`Platform`].
//!
//! Built on `std` only, which already does the right thing per OS:
//!
//! - **Durability.**  [`File::sync`] is `std::fs::File::sync_data`:
//!   `fdatasync` on Linux and d-os (a length change made durable, the times
//!   left — PostgreSQL's and SQLite's choice there), `fcntl(F_FULLFSYNC)`
//!   on macOS (plain `fsync` there does not flush the drive cache),
//!   `FlushFileBuffers` on Windows — under
//!   [`SyncPolicy::Ordered`] (the default) on macOS
//!   `fcntl(F_BARRIERFSYNC)` instead: ordered writes, the drive's cache not
//!   flushed.  That call and [`physical_memory`]'s on macOS are the
//!   crate's only unsafe code (`std` offers neither), allowed where they
//!   are made.
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

use crate::{Clock, File, FileSystem, OpenMode, Platform, Rng, SplitMix64, SyncPolicy};

/// The operating system: real files, the real clock, randomness seeded from
/// the OS.
#[derive(Debug)]
pub struct RealPlatform {
    fs: RealFs,
    clock: RealClock,
    rng: OsRng,
    /// [`Platform::parallelism`], asked of the OS once (a system call
    /// each time otherwise, and statements ask).
    parallelism: usize,
}

/// Fast randomness (seeded from the operating system once), and the
/// operating system's generator where it must be unpredictable.
#[derive(Debug)]
struct OsRng(SplitMix64);

impl Rng for OsRng {
    fn next_u64(&self) -> u64 {
        self.0.next_u64()
    }
    fn fill_unpredictable(&self, buf: &mut [u8]) {
        if getrandom::fill(buf).is_err() {
            // No OS generator: still random, though not unpredictable.
            self.fill_bytes(buf);
        }
    }
}

impl RealPlatform {
    /// Creates the platform.  Fails only if the OS refuses to provide a
    /// random seed.
    pub fn new() -> io::Result<Self> {
        Self::with_sync(SyncPolicy::default())
    }

    /// [`new`](Self::new), its files synced as `sync` says.
    pub fn with_sync(sync: SyncPolicy) -> io::Result<Self> {
        let seed = getrandom::u64().map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Self {
            fs: RealFs { sync },
            clock: RealClock {
                origin: Instant::now(),
            },
            rng: OsRng(SplitMix64::new(seed)),
            parallelism: std::thread::available_parallelism().map_or(1, usize::from),
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

    fn parallelism(&self) -> usize {
        self.parallelism
    }
}

/// The OS file system, its files synced as [`SyncPolicy`] says.
#[derive(Debug)]
pub struct RealFs {
    sync: SyncPolicy,
}

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
        Ok(Box::new(RealFile {
            file,
            sync: self.sync,
        }))
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
        let dir = fs::File::open(path)?;
        #[cfg(target_vendor = "apple")]
        return sync_file(&dir, self.sync);
        // A directory's entries by the full `fsync`, as PostgreSQL syncs
        // them: `fdatasync`'s "data" of a directory is not specified.
        #[cfg(not(target_vendor = "apple"))]
        dir.sync_all()
    }

    fn writes_in_place(&self) -> bool {
        // APFS writes a changed block elsewhere (copy-on-write): measured,
        // an overwrite synced costs it 16 KiB, an append 10–11 (§M14.28 L0).
        !cfg!(target_vendor = "apple")
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
    sync: SyncPolicy,
}

/// Syncs `file` as `policy` says ([`SyncPolicy`]).
fn sync_file(file: &fs::File, policy: SyncPolicy) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    if policy == SyncPolicy::Ordered {
        return barrier_sync(file);
    }
    let _ = policy;
    file.sync_data()
}

/// The memory this process may use, in bytes, where the operating system
/// says: the machine's (`hw.memsize` on macOS, `MemTotal` on Linux), or a
/// container's limit below it (cgroup v2's `memory.max`, v1's
/// `memory.limit_in_bytes`) — what DuckDB sizes its `memory_limit` by.
/// `None` elsewhere, or where it cannot be read.
pub fn physical_memory() -> Option<u64> {
    let machine = machine_memory()?;
    Some(cgroup_limit().map_or(machine, |l| l.min(machine)))
}

#[cfg(target_vendor = "apple")]
#[allow(unsafe_code)]
fn machine_memory() -> Option<u64> {
    unsafe extern "C" {
        fn sysctlbyname(
            name: *const std::ffi::c_char,
            old: *mut std::ffi::c_void,
            oldlen: *mut usize,
            new: *mut std::ffi::c_void,
            newlen: usize,
        ) -> i32;
    }
    let mut bytes = 0u64;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: a NUL-terminated name; `old` points at `len` writable bytes
    // that live through the call; nothing is set (`new` null, 0 long).
    let r = unsafe {
        sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut bytes).cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (r == 0 && len == std::mem::size_of::<u64>() && bytes > 0).then_some(bytes)
}

#[cfg(not(target_vendor = "apple"))]
fn machine_memory() -> Option<u64> {
    let info = fs::read_to_string("/proc/meminfo").ok()?;
    let line = info.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024).filter(|&b| b > 0)
}

/// A container's memory limit (cgroup v2, then v1), if one is set.
fn cgroup_limit() -> Option<u64> {
    [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ]
    .iter()
    .filter_map(|p| fs::read_to_string(p).ok())
    // "max" (v2) or a page-rounded i64::MAX (v1): no limit.
    .filter_map(|s| s.trim().parse::<u64>().ok())
    .find(|&b| b > 0 && b < 1 << 60)
}

/// `fcntl(F_BARRIERFSYNC)`: the file's writes so far reach the drive before
/// any later one, without flushing the drive's cache.  A file system that
/// does not support it gets the full sync (as SQLite does).
#[cfg(target_vendor = "apple")]
#[allow(unsafe_code)]
fn barrier_sync(file: &fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    /// From `<sys/fcntl.h>`.
    const F_BARRIERFSYNC: i32 = 85;
    // SAFETY: a command without an argument, on a descriptor `file` owns
    // and keeps open for the call.
    if unsafe { fcntl(file.as_raw_fd(), F_BARRIERFSYNC) } == -1 {
        return file.sync_all();
    }
    Ok(())
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

    /// WASI (the embedded edition in WebAssembly, PLAN.md §M14.23): a
    /// seek and a read — one thread, so nothing comes between them.
    #[cfg(target_os = "wasi")]
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = &self.file;
        f.seek(SeekFrom::Start(offset))?;
        f.read(buf)
    }

    #[cfg(target_os = "wasi")]
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = &self.file;
        f.seek(SeekFrom::Start(offset))?;
        f.write(buf)
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
        sync_file(&self.file, self.sync)
    }

    fn sync_order(&self) -> io::Result<()> {
        sync_file(&self.file, SyncPolicy::Ordered)
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

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(any(target_vendor = "apple", target_os = "linux"))]
    fn the_machine_has_memory() {
        let m = super::physical_memory().expect("the OS says");
        // More than 64 MB, less than a petabyte.
        assert!(m > 64 << 20 && m < 1 << 50, "{m}");
    }
}
