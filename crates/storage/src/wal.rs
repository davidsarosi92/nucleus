//! The write-ahead log: an append-only sequence of checksummed records in
//! segment files.
//!
//! # Format
//!
//! A segment is a file named after the LSN of its first byte
//! (`00000000000000000000.wal`, 20 decimal digits, so names sort by LSN).  An
//! **LSN** is a byte position in the whole log; a record's LSN is where it
//! starts.  A record is a 16-byte header and a payload (little-endian):
//!
//! | Offset | Size | Field |
//! |-------:|-----:|-------|
//! | 0  | 4 | payload length |
//! | 4  | 4 | CRC-32C of the LSN (8 bytes) followed by the payload |
//! | 8  | 8 | the record's LSN |
//!
//! Folding the LSN into the checksum means a stale record left at some
//! position by an earlier life of the file never passes for the record that
//! belongs there.
//!
//! # Durability
//!
//! [`Wal::append`] writes a record and returns the LSN just past it; the
//! record is durable once [`Wal::sync_to`] with that LSN returns.  `sync_to`
//! is a **group commit**: one `fsync` covers every record appended so far,
//! and callers waiting for earlier LSNs return without another.
//!
//! Directory changes are ordered so that a crash can only ever lose a
//! *suffix* of the log (unsynced writes) or leave back some of the
//! segments being deleted: before a new segment is created, the previous
//! one is synced and so is the directory entry naming it; segments wholly
//! before a checkpoint are deleted together, with one directory sync
//! (§M14.20 W4) — a crash may keep any of them, and opening from the
//! checkpoint deletes again those that lie before the segment holding it.
//!
//! # Preallocated and recycled segments
//!
//! Where the file system writes over a file's blocks in place
//! ([`FileSystem::writes_in_place`](nucleus_platform::FileSystem::writes_in_place):
//! ext4, XFS, NTFS), a segment is grown ahead of its records by zeros, a
//! mebibyte at a time, and the segments a checkpoint drops are kept as
//! spares (`<20 digits>.spare`) and renamed into the next segments, as
//! many as the last checkpoint dropped (PostgreSQL's recycled segments).
//! A commit's sync then writes its block and no metadata: on ext4 4 KiB
//! and ~80 µs a small commit, against 17 KiB and ~400 µs into a file that
//! grows (PLAN.md §M14.28 L0).  A spare's old records carry older LSNs,
//! so none passes for a record at its new place.  On a copy-on-write file
//! system (APFS) an overwrite costs more than an append, and segments grow
//! as they are written and are deleted when dropped.
//!
//! # Opening
//!
//! [`Wal::open`] reads every segment.  The first record that fails its
//! checksum, is cut short, or carries the wrong LSN ends the log: in the last
//! segment that is a torn tail of writes that were never reported durable,
//! and it is cut off (or, in a preallocated segment, written over by
//! zeros); a segment before the last must end where the next begins, or
//! it is corruption.  A failed `fsync` poisons the log.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use nucleus_platform::{File, OpenMode, Platform};

use crate::crc32c::crc32c;
use crate::error::{Error, Result};
use crate::page::PageId;

const RECORD_HEADER: u64 = 16;

/// A preallocated segment grows ahead of its records by this much.
const GROW: u64 = 1 << 20;

/// A log sequence number: a byte position in the log.
pub type Lsn = u64;

struct Segment {
    start: Lsn,
    path: PathBuf,
    file: Arc<dyn File>,
    /// The file's length (preallocated segments: how far it is zeros or
    /// old records ahead of the log).
    len: u64,
}

struct State {
    segments: Vec<Segment>,
    /// Where the next record goes.
    end: Lsn,
    /// A segment was created whose directory entry is not yet synced.
    dir_dirty: bool,
    poisoned: bool,
    /// Dropped segments kept for reuse (preallocated segments only).
    spares: Vec<PathBuf>,
}

/// The write-ahead log.
pub struct Wal {
    platform: Arc<dyn Platform>,
    dir: PathBuf,
    segment_size: u64,
    /// Segments preallocated and recycled ([`FileSystem::writes_in_place`](nucleus_platform::FileSystem::writes_in_place)).
    in_place: bool,
    state: Mutex<State>,
    /// Serialises `fsync`s; holds the LSN known durable.
    durable: Mutex<Lsn>,
}

impl std::fmt::Debug for Wal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wal")
            .field("dir", &self.dir)
            .field("end", &self.end())
            .finish_non_exhaustive()
    }
}

fn segment_name(start: Lsn) -> String {
    format!("{start:020}.wal")
}

fn spare_name(start: Lsn) -> String {
    format!("{start:020}.spare")
}

/// The LSN a file name of `suffix` stands for.
fn named(name: &str, suffix: &str) -> Option<Result<Lsn>> {
    let n = name.strip_suffix(suffix).filter(|n| n.len() == 20)?;
    Some(
        n.parse::<Lsn>()
            .map_err(|_| corrupt("bad segment file name")),
    )
}

fn corrupt(reason: &'static str) -> Error {
    // The log has no pages; page 0 stands for "the log".
    Error::Corrupt {
        page: PageId(0),
        reason,
    }
}

impl Wal {
    /// Opens (or creates) the log in directory `dir`, which must exist.
    /// Segments roll over at about `segment_size` bytes.
    pub fn open(platform: Arc<dyn Platform>, dir: &Path, segment_size: u64) -> Result<Self> {
        Self::open_from(platform, dir, segment_size, 0)
    }

    /// [`open`](Self::open) for a reader starting at `from` (a checkpoint's
    /// LSN): the segments before the one holding it are left over from a
    /// deletion a crash interrupted, and are deleted.
    pub fn open_from(
        platform: Arc<dyn Platform>,
        dir: &Path,
        segment_size: u64,
        from: Lsn,
    ) -> Result<Self> {
        let fs = platform.fs();
        let in_place = fs.writes_in_place();
        let mut starts = Vec::new();
        let mut spares = Vec::new();
        for name in fs.list_dir(dir)? {
            let name = name.to_string_lossy();
            if let Some(start) = named(&name, ".wal") {
                starts.push(start?);
            } else if let Some(start) = named(&name, ".spare") {
                let path = dir.join(spare_name(start?));
                if in_place {
                    spares.push(path);
                } else {
                    // Moved from a file system that writes in place.
                    fs.remove_file(&path)?;
                }
            }
        }
        starts.sort_unstable();
        if let Some(keep) = starts.iter().rposition(|&s| s <= from)
            && keep > 0
        {
            for &start in &starts[..keep] {
                fs.remove_file(&dir.join(segment_name(start)))?;
            }
            fs.sync_dir(dir)?;
            starts.drain(..keep);
        }

        let mut segments = Vec::new();
        // No segment left: the log starts at the reader's start (a crash
        // lost the entry of a segment created there — before anything in
        // it was durable, as a sync makes its entry durable too).
        let mut end = starts.first().copied().unwrap_or(from);
        for (i, &start) in starts.iter().enumerate() {
            if start != end {
                return Err(corrupt("gap between log segments"));
            }
            let path = dir.join(segment_name(start));
            let file: Arc<dyn File> = Arc::from(fs.open(&path, OpenMode::ReadWrite)?);
            let valid = scan(file.as_ref(), start, |_, _| Ok(()))?;
            let mut len = file.size()?;
            if let Some(&next) = starts.get(i + 1) {
                // A segment before the last ends where the next begins
                // (what follows its records, if anything, is zeros or
                // older records of a recycled file).
                if start + valid != next {
                    return Err(corrupt("damaged record before the last log segment"));
                }
            } else if valid < len {
                // The torn tail.
                if !in_place {
                    file.set_len(valid)?;
                    file.sync()?;
                    len = valid;
                } else if !zeros_or_older(file.as_ref(), start, valid)? {
                    // A torn record of this log's own: zeros over the
                    // rest, so nothing of it is read past a later record.
                    zero(file.as_ref(), valid, len)?;
                    file.sync()?;
                }
            }
            end = start + valid;
            segments.push(Segment {
                start,
                path,
                file,
                len,
            });
        }
        let wal = Self {
            platform,
            dir: dir.to_path_buf(),
            segment_size: segment_size.max(RECORD_HEADER + 1),
            in_place,
            state: Mutex::new(State {
                segments,
                end,
                dir_dirty: false,
                poisoned: false,
                spares,
            }),
            durable: Mutex::new(end),
        };
        if wal.lock().segments.is_empty() {
            let mut s = wal.lock();
            wal.new_segment(&mut s, end)?;
        }
        Ok(wal)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn new_segment(&self, s: &mut State, start: Lsn) -> Result<()> {
        let fs = self.platform.fs();
        let path = self.dir.join(segment_name(start));
        let file: Arc<dyn File> = match s.spares.pop() {
            Some(spare) => {
                fs.rename(&spare, &path)?;
                Arc::from(fs.open(&path, OpenMode::ReadWrite)?)
            }
            None => Arc::from(fs.open(&path, OpenMode::CreateNew)?),
        };
        let len = file.size()?;
        s.segments.push(Segment {
            start,
            path,
            file,
            len,
        });
        s.dir_dirty = true;
        Ok(())
    }

    /// The LSN the next record will get.
    pub fn end(&self) -> Lsn {
        self.lock().end
    }

    /// The LSN of the first record still in the log.
    pub fn start(&self) -> Lsn {
        self.lock().segments[0].start
    }

    /// Appends a record; returns the LSN just past it (pass it to
    /// [`Wal::sync_to`]).
    pub fn append(&self, payload: &[u8]) -> Result<Lsn> {
        let mut s = self.lock();
        if s.poisoned {
            return Err(Error::Poisoned);
        }
        let len = RECORD_HEADER + payload.len() as u64;
        let current = s.segments.last().expect("the log always has a segment");
        if s.end > current.start && s.end - current.start + len > self.segment_size {
            // Move to a new segment; the old one must be complete on disk
            // before anything in the new one can count as durable.
            // And the old segment's own directory entry must be durable
            // before the new one exists, or a crash could keep the new entry
            // and lose the old: a gap in the log.
            let file = Arc::clone(&current.file);
            let dir_dirty = s.dir_dirty;
            // (Ordering is all it needs: the sync that makes a record of
            // the new segment durable flushes these too — a barrier where
            // the platform has one, §M14.20 W4.)
            let synced = file.sync_order().and_then(|()| {
                if dir_dirty {
                    self.platform.fs().sync_dir(&self.dir)
                } else {
                    Ok(())
                }
            });
            if let Err(e) = synced {
                s.poisoned = true;
                return Err(e.into());
            }
            s.dir_dirty = false;
            let start = s.end;
            self.new_segment(&mut s, start)?;
        }
        let lsn = s.end;
        let mut rec = Vec::with_capacity(len as usize);
        rec.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        rec.extend_from_slice(&record_crc(lsn, payload).to_le_bytes());
        rec.extend_from_slice(&lsn.to_le_bytes());
        rec.extend_from_slice(payload);
        let in_place = self.in_place;
        let segment_size = self.segment_size;
        let seg = s.segments.last_mut().expect("the log always has a segment");
        let at = lsn - seg.start;
        if in_place && at + len > seg.len {
            // Zeros ahead of the records, so their syncs carry no length.
            let to = (at + len).max((seg.len + GROW).min(segment_size));
            zero(seg.file.as_ref(), seg.len, to)?;
            seg.len = to;
        }
        seg.file.write_all_at(&rec, at)?;
        s.end = lsn + len;
        Ok(s.end)
    }

    /// Returns once every record before `lsn` is durable.
    pub fn sync_to(&self, lsn: Lsn) -> Result<()> {
        let mut durable = self.durable.lock().unwrap_or_else(|e| e.into_inner());
        if *durable >= lsn {
            return Ok(());
        }
        // Everything appended so far goes in this one sync.
        let (file, end, dir_dirty) = {
            let mut s = self.lock();
            if s.poisoned {
                return Err(Error::Poisoned);
            }
            let seg = s.segments.last().expect("the log always has a segment");
            let r = (Arc::clone(&seg.file), s.end, s.dir_dirty);
            s.dir_dirty = false;
            r
        };
        let result = file.sync().and_then(|()| {
            if dir_dirty {
                self.platform.fs().sync_dir(&self.dir)
            } else {
                Ok(())
            }
        });
        if let Err(e) = result {
            self.lock().poisoned = true;
            return Err(e.into());
        }
        *durable = end;
        Ok(())
    }

    /// The LSN known durable.
    pub fn durable(&self) -> Lsn {
        *self.durable.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Calls `f(lsn, payload)` for every record at or after `from`, in order.
    /// `from` must be a record boundary (an LSN `append` returned, or the
    /// log's start).
    pub fn replay(&self, from: Lsn, mut f: impl FnMut(Lsn, &[u8]) -> Result<()>) -> Result<()> {
        let segments: Vec<(Lsn, Arc<dyn File>)> = {
            let s = self.lock();
            s.segments
                .iter()
                .map(|g| (g.start, Arc::clone(&g.file)))
                .collect()
        };
        if from < segments[0].0 {
            return Err(corrupt("replay starts before the log"));
        }
        for (i, (start, file)) in segments.iter().enumerate() {
            let next = segments.get(i + 1).map(|g| g.0);
            if next.is_some_and(|n| n <= from) {
                continue;
            }
            scan(file.as_ref(), *start, |lsn, payload| {
                if lsn >= from { f(lsn, payload) } else { Ok(()) }
            })?;
        }
        Ok(())
    }

    /// Deletes the segments that hold only records before `lsn`.  The
    /// segment being written is always kept.
    pub fn truncate_before(&self, lsn: Lsn) -> Result<()> {
        let fs = self.platform.fs();
        // The segment holding `lsn` may be new, its directory entry not yet
        // durable: it must be before any older one leaves, or a crash could
        // keep an older segment and lose it — a log ending before the
        // checkpoint.
        if self.lock().segments.get(1).is_some_and(|g| g.start <= lsn) {
            fs.sync_dir(&self.dir)?;
        }
        let mut s = self.lock();
        let mut doomed = Vec::new();
        while s.segments.len() > 1 && s.segments[1].start <= lsn {
            doomed.push(s.segments.remove(0));
        }
        if doomed.is_empty() {
            return Ok(());
        }
        // All, then one directory sync: a crash may keep any of them (an
        // unsynced directory's changes survive in any combination), and
        // opening from the checkpoint deletes them again (`open_from`).
        // Preallocated, as many are kept as spares as this checkpoint
        // dropped — what the next will need — and the rest deleted.  (The
        // state lock is held: a new segment may not take a spare whose
        // rename is still to come.)
        let keep = if self.in_place { doomed.len() } else { 0 };
        for seg in doomed {
            if s.spares.len() < keep {
                let spare = self.dir.join(spare_name(seg.start));
                fs.rename(&seg.path, &spare)?;
                s.spares.push(spare);
            } else {
                fs.remove_file(&seg.path)?;
            }
        }
        while s.spares.len() > keep {
            let spare = s.spares.remove(0);
            fs.remove_file(&spare)?;
        }
        drop(s);
        fs.sync_dir(&self.dir)?;
        Ok(())
    }

    /// Bytes in the log (all segments).
    pub fn size(&self) -> u64 {
        let s = self.lock();
        s.end - s.segments[0].start
    }
}

/// Whether what follows the records of the segment starting at `start`,
/// at `at`, is zeros (or nothing) or a record older than the segment (a
/// recycled file's) — not a torn record of its own.
fn zeros_or_older(file: &dyn File, start: Lsn, at: u64) -> Result<bool> {
    let mut header = [0u8; RECORD_HEADER as usize];
    let n = file.read_at(&mut header, at)?;
    if header[..n].iter().all(|&b| b == 0) {
        return Ok(true);
    }
    Ok(n == header.len() && u64::from_le_bytes(header[8..16].try_into().unwrap()) < start)
}

/// Writes zeros over `from..to`.
fn zero(file: &dyn File, from: u64, to: u64) -> Result<()> {
    let zeros = vec![0u8; (to - from).min(GROW) as usize];
    let mut at = from;
    while at < to {
        let n = (to - at).min(GROW) as usize;
        file.write_all_at(&zeros[..n], at)?;
        at += n as u64;
    }
    Ok(())
}

fn record_crc(lsn: Lsn, payload: &[u8]) -> u32 {
    let mut buf = Vec::with_capacity(8 + payload.len());
    buf.extend_from_slice(&lsn.to_le_bytes());
    buf.extend_from_slice(payload);
    crc32c(&buf)
}

/// Reads the valid records of the segment starting at `start`, calling `f`
/// for each; returns the length of the valid prefix.
fn scan(file: &dyn File, start: Lsn, mut f: impl FnMut(Lsn, &[u8]) -> Result<()>) -> Result<u64> {
    let len = file.size()?;
    let mut off = 0u64;
    let mut header = [0u8; RECORD_HEADER as usize];
    while off + RECORD_HEADER <= len {
        file.read_exact_at(&mut header, off)?;
        let plen = u32::from_le_bytes(header[0..4].try_into().unwrap()) as u64;
        let crc = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let lsn = u64::from_le_bytes(header[8..16].try_into().unwrap());
        if lsn != start + off || off + RECORD_HEADER + plen > len {
            break;
        }
        let mut payload = vec![0u8; plen as usize];
        file.read_exact_at(&mut payload, off + RECORD_HEADER)?;
        if record_crc(lsn, &payload) != crc {
            break;
        }
        f(lsn, &payload)?;
        off += RECORD_HEADER + plen;
    }
    Ok(off)
}
