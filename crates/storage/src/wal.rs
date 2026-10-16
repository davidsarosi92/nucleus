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
//! *suffix* of the log (unsynced writes) or a *prefix* (segments being
//! deleted), never leave a gap: before a new segment is created, the
//! previous one is synced and so is the directory entry naming it; segments
//! are deleted oldest first, one directory sync each.
//!
//! # Opening
//!
//! [`Wal::open`] reads every segment.  The first record that fails its
//! checksum, is cut short, or carries the wrong LSN ends the log: in the last
//! segment that is a torn tail of writes that were never reported durable,
//! and it is cut off; anywhere else it is corruption.  A failed `fsync`
//! poisons the log.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use nucleus_platform::{File, OpenMode, Platform};

use crate::crc32c::crc32c;
use crate::error::{Error, Result};
use crate::page::PageId;

const RECORD_HEADER: u64 = 16;

/// A log sequence number: a byte position in the log.
pub type Lsn = u64;

struct Segment {
    start: Lsn,
    path: PathBuf,
    file: Arc<dyn File>,
}

struct State {
    segments: Vec<Segment>,
    /// Where the next record goes.
    end: Lsn,
    /// A segment was created whose directory entry is not yet synced.
    dir_dirty: bool,
    poisoned: bool,
}

/// The write-ahead log.
pub struct Wal {
    platform: Arc<dyn Platform>,
    dir: PathBuf,
    segment_size: u64,
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
        let fs = platform.fs();
        let mut starts = Vec::new();
        for name in fs.list_dir(dir)? {
            let name = name.to_string_lossy();
            if let Some(n) = name.strip_suffix(".wal").filter(|n| n.len() == 20) {
                starts.push(
                    n.parse::<Lsn>()
                        .map_err(|_| corrupt("bad segment file name"))?,
                );
            }
        }
        starts.sort_unstable();

        let mut segments = Vec::new();
        let mut end = starts.first().copied().unwrap_or(0);
        let count = starts.len();
        for (i, start) in starts.into_iter().enumerate() {
            if start != end {
                return Err(corrupt("gap between log segments"));
            }
            let path = dir.join(segment_name(start));
            let file: Arc<dyn File> = Arc::from(fs.open(&path, OpenMode::ReadWrite)?);
            let valid = scan(file.as_ref(), start, |_, _| Ok(()))?;
            let len = file.size()?;
            if valid < len {
                if i + 1 < count {
                    return Err(corrupt("damaged record before the last log segment"));
                }
                file.set_len(valid)?; // the torn tail
                file.sync()?;
            }
            end = start + valid;
            segments.push(Segment { start, path, file });
        }
        let wal = Self {
            platform,
            dir: dir.to_path_buf(),
            segment_size: segment_size.max(RECORD_HEADER + 1),
            state: Mutex::new(State {
                segments,
                end,
                dir_dirty: false,
                poisoned: false,
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
        let path = self.dir.join(segment_name(start));
        let file: Arc<dyn File> = Arc::from(self.platform.fs().open(&path, OpenMode::CreateNew)?);
        s.segments.push(Segment { start, path, file });
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
            let synced = file.sync().and_then(|()| {
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
        let seg = s.segments.last().expect("the log always has a segment");
        seg.file.write_all_at(&rec, lsn - seg.start)?;
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
        let doomed: Vec<PathBuf> = {
            let mut s = self.lock();
            let mut doomed = Vec::new();
            while s.segments.len() > 1 && s.segments[1].start <= lsn {
                doomed.push(s.segments.remove(0).path);
            }
            doomed
        };
        if doomed.is_empty() {
            return Ok(());
        }
        // One at a time, oldest first, each removal durable before the
        // next: unsynced directory changes can survive a crash in any
        // combination, and only removing a prefix keeps the log contiguous.
        for path in &doomed {
            self.platform.fs().remove_file(path)?;
            self.platform.fs().sync_dir(&self.dir)?;
        }
        Ok(())
    }

    /// Bytes in the log (all segments).
    pub fn size(&self) -> u64 {
        let s = self.lock();
        s.end - s.segments[0].start
    }
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
