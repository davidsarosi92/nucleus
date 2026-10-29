//! The platform layer: every way nucleus touches the outside world.
//!
//! File I/O, time and randomness are reachable **only** through the traits in
//! this crate (PLAN.md, principle 4).  Two implementations exist:
//!
//! - [`real::RealPlatform`] — the operating system (POSIX and Windows).
//! - [`sim::SimPlatform`] — an in-memory simulation that is deterministic from
//!   a seed and injects faults: I/O errors, lost `fsync`s, and crashes that
//!   drop or tear everything not yet made durable.
//!
//! Code above this layer must not tell the two apart.  The conformance tests
//! in `tests/conformance.rs` run the same assertions against both.
//!
//! # Durability model
//!
//! A write is visible to readers at once but **durable only after
//! [`File::sync`]**.  A newly created, removed or renamed directory entry is
//! durable only after [`FileSystem::sync_dir`] on its parent directory.  The
//! simulation enforces exactly this, so code that forgets a sync loses data in
//! tests rather than in production.

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

pub mod harness;
pub mod real;
mod rng;
pub mod sim;

pub use rng::SplitMix64;

/// One environment: a file system, a clock and a source of randomness.
pub trait Platform: Send + Sync {
    /// The file system.
    fn fs(&self) -> &dyn FileSystem;
    /// The clock.
    fn clock(&self) -> &dyn Clock;
    /// The source of randomness (not cryptographic).
    fn rng(&self) -> &dyn Rng;
    /// How many threads work can usefully run on at once (the CPUs the
    /// process may use); 1 by default — and in the simulation, so its runs
    /// stay single-threaded unless a test asks for more.
    fn parallelism(&self) -> usize {
        1
    }
}

/// How [`FileSystem::open`] opens a file.  Every mode except `ReadOnly`
/// allows writing; none truncates (use [`File::set_len`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    /// Read an existing file.
    ReadOnly,
    /// Read and write an existing file.
    ReadWrite,
    /// Read and write, creating the file if it does not exist.
    Create,
    /// Read and write a new file; fails with `AlreadyExists` if it exists.
    CreateNew,
}

/// A file system: files and directories under paths.
pub trait FileSystem: Send + Sync {
    /// Opens the file at `path`.  The parent directory must exist.
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn File>>;

    /// Removes the file at `path`.  Durable after `sync_dir` of its parent.
    fn remove_file(&self, path: &Path) -> io::Result<()>;

    /// Renames the file `from` to `to`, atomically replacing `to` if it
    /// exists.  Durable after `sync_dir` of both parents.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Creates the directory `path` and any missing ancestors.
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;

    /// The names of the entries in the directory `path`, sorted.
    fn list_dir(&self, path: &Path) -> io::Result<Vec<OsString>>;

    /// Whether anything exists at `path`.
    fn exists(&self, path: &Path) -> io::Result<bool>;

    /// Makes the entries of directory `path` (creations, removals, renames)
    /// durable.
    fn sync_dir(&self, path: &Path) -> io::Result<()>;

    /// Takes an exclusive lock named by the file at `path` (created if
    /// missing; its contents are not used), held until the returned guard
    /// is dropped.  It excludes other processes and other locks of this
    /// one; if someone holds it, fails at once with `WouldBlock`.  A crash
    /// releases it, as the end of a process does.
    fn lock(&self, path: &Path) -> io::Result<Box<dyn Lock>>;
}

/// A held lock (see [`FileSystem::lock`]); dropping it releases the lock.
pub trait Lock: Send + Sync {}

/// An open file, addressed by offset (there is no cursor).  All methods take
/// `&self`, so one handle can be shared between threads.
pub trait File: Send + Sync {
    /// Reads into `buf` from `offset`; returns the bytes read, 0 at or past
    /// the end of the file.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;

    /// Writes `buf` at `offset`, extending the file (with zeros in any gap)
    /// if needed; returns the bytes written.
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize>;

    /// Makes every write and length change so far durable — all the way to
    /// stable storage, not just to the OS or the drive cache.
    ///
    /// **If this fails, the unsynced writes must be treated as lost**, even if
    /// a later call succeeds: some kernels mark the failed pages clean and
    /// never write them (the PostgreSQL "fsyncgate" lesson).  The simulation
    /// behaves that way.
    fn sync(&self) -> io::Result<()>;

    /// The current length of the file in bytes.
    fn size(&self) -> io::Result<u64>;

    /// Truncates or extends (with zeros) the file to `len` bytes.
    fn set_len(&self, len: u64) -> io::Result<()>;

    /// Reads exactly `buf.len()` bytes from `offset`, or fails with
    /// `UnexpectedEof`.
    fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match self.read_at(buf, offset) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Writes all of `buf` at `offset`.
    fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match self.write_at(buf, offset) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    buf = &buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// A clock.
pub trait Clock: Send + Sync {
    /// Monotonic time since an arbitrary origin fixed when the platform was
    /// created.  Never decreases.
    fn now(&self) -> Duration;

    /// Wall-clock time.  May jump; never use it to measure intervals.
    fn wall(&self) -> SystemTime;

    /// Waits for `d`.  The simulated clock advances by `d` at once, and
    /// waits in real time only up to `sim::SIM_SLEEP_CAP`.
    fn sleep(&self, d: Duration);
}

/// A source of randomness.  **Not cryptographic.**
pub trait Rng: Send + Sync {
    /// A uniformly distributed 64-bit value.
    fn next_u64(&self) -> u64;

    /// A uniformly distributed value in `0..bound`.  Panics if `bound` is 0.
    fn below(&self, bound: u64) -> u64 {
        assert!(bound > 0, "Rng::below(0)");
        ((u128::from(self.next_u64()) * u128::from(bound)) >> 64) as u64
    }

    /// `true` with probability `ppm` per million.
    fn chance_ppm(&self, ppm: u32) -> bool {
        self.below(1_000_000) < u64::from(ppm)
    }

    /// Fills `buf` with random bytes.
    fn fill_bytes(&self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }

    /// Fills `buf` with bytes nobody can predict (for UUIDs, tokens): the
    /// operating system's generator on the real platform; in the
    /// simulation they come from its seed, like all its randomness.
    fn fill_unpredictable(&self, buf: &mut [u8]) {
        self.fill_bytes(buf);
    }
}
