//! The LSM tree (PLAN.md §M14.28 L2): an engine behind the store for
//! tables written far more than they are read per key.
//!
//! Writes go to a **memtable** (a sorted map in memory: puts, point
//! tombstones, and range tombstones for `DROP` / `TRUNCATE` / a covering
//! write); the store's WAL is its log, so it has none of its own.  At a
//! checkpoint the memtable is set apart under the branch's write lock
//! ([`Engine::prepare`], a pointer swap, with the runs as they are:
//! [`Engine::saver`]) and written as an immutable, sorted **run** with no
//! lock held — reads and writes go on (§M14.28 L4a) — and a **manifest**
//! naming the runs is written in the file the checkpoint's LSN names,
//! before the checkpoint commits.  The run joins the levels at the next
//! change.
//! A run is never changed: branches share them by name (a branch forks
//! its parent's runs), and the store removes a run's file once no
//! branch's manifest names it.
//!
//! **Reads merge** the memtable(s) and the runs, newest first: the first
//! source holding a key decides (a value, or a tombstone), and a range
//! tombstone hides the key in every older source.  Within one source a
//! point is newer than its range tombstones (a memtable drops the points
//! a range tombstone covers when it takes it), so a source's own points
//! are never hidden by its own ranges.
//!
//! **A run's file**: blocks of about 4 KiB of entries (a key, put or
//! tombstone, a value), each compressed when that pays and closed by a
//! CRC-32C (D6's checksummed blocks); then the block index (each block's
//! first key), a split-block Bloom filter of the keys (Apache Parquet's:
//! a 256-bit block a key, eight bits set, one in each 32-bit word — one
//! cache line a probe, about 1 % false positives at 10 bits a key), the
//! range tombstones, its own CRC-32C, and a footer with the format's
//! magic.  Versioned: a later format reads this one (D15).
//!
//! **Compaction**: lazy leveling (Dostoevsky, Dayan and Idreos 2018) with
//! a fan-in [`FANIN`]: the upper levels are tiered — a level holds up to
//! `FANIN` runs, merged into one run of the next level when full — and
//! the last level is one run, which the runs arriving from the level
//! above merge into while it is at most `FANIN` times their size (a new
//! level forms above it when it is larger).  A merge into the last level
//! drops tombstones (nothing older is left for them to hide).  A merge is
//! a background **job** ([`Engine::job`], run by `Store::compact` on a
//! thread of its own, within an I/O budget): planned from the levels as
//! they are, written with no lock held, and installed under the write
//! lock ([`Engine::install`]) — its inputs are still the oldest runs of
//! their tier then (flushes since went first; one job at a time).  Its
//! run is durable with the next manifest; until then a crash opens the
//! last one, whose runs [`Engine::files`] keeps.
//!
//! **Bulk loads** (§M14.28 L4b): keys written in ascending order past all
//! the memtable holds (an index build's, a fresh range's) are appended to
//! a sorted tail, and past [`SPILL`] the loader writes that tail as a run
//! ([`Engine::spill`]) — a load is not held in memory whole.  Otherwise
//! the memtable is flushed at each checkpoint, so its memory is bounded
//! by the checkpoint interval.
//!
//! **Reads** go through a block cache for the process ([`CACHE`],
//! §M14.28 L4c): a block decoded once, searched in place.
//!
//! A merge drops what the layer above knows to be garbage (its
//! [`LeafFilter`], §M14.28 L4d: versions no snapshot sees); above the last
//! level it keeps what is garbage only because nothing older is left.
//! A scan is cut into parts for workers ([`Engine::split_keys`]) at its
//! runs' blocks' first keys and its memtables' sampled keys.
//!
//! A commit's filter drops it from the memtable too
//! ([`Engine::apply_filtered`], §M14.28 L5), among the versions of the
//! keys it writes.
//!
//! On an encrypted database its files are sealed below it (the store's
//! [`SealedFs`](crate::sealed::SealedFs), §M14.28 L4e): nothing here
//! knows.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::path::Path;
use std::sync::{Arc, Mutex};

use nucleus_platform::{File, FileSystem, OpenMode};

use crate::btree::{LeafEntry, LeafFilter};
use crate::crc32c::crc32c;
use crate::engines::{Done, Engine, Entries, Job, JobRoom, Ops, Saver};
use crate::{Error, PageId, Result, lz};

/// Runs a tiered level holds before they merge into one run of the next.
pub const FANIN: usize = 4;
/// A block's entries, before compression, about this many bytes.
const BLOCK: usize = 4096;
/// Bloom filter bits a key.
const BLOOM_BITS: usize = 10;
const RUN_MAGIC: &[u8; 8] = b"ncrun\0\0\x01";
const MANIFEST_MAGIC: &[u8; 8] = b"nclsm\0\0\x01";
/// The footer: the meta section's offset and length, the entries, the
/// magic.
const FOOTER: usize = 8 + 8 + 8 + 8;

fn corrupt(reason: &'static str) -> Error {
    Error::Corrupt {
        page: PageId(0),
        reason,
    }
}

/// A value, or a tombstone (`None`).
type Val = Option<Vec<u8>>;

// ------------------------------------------------------------ hashing

fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// A 64-bit hash of a key (SplitMix64's finalizer over its words).
fn hash64(b: &[u8]) -> u64 {
    let mut h = mix((b.len() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
    let (words, r) = b.as_chunks::<8>();
    for w in words {
        h = mix(h ^ u64::from_le_bytes(*w));
    }
    if !r.is_empty() {
        let mut w = [0u8; 8];
        w[..r.len()].copy_from_slice(r);
        h = mix(h ^ u64::from_le_bytes(w) ^ 0xa5a5_a5a5_0000_0000);
    }
    h
}

/// `key` up to and including its first `00 00` — the transaction
/// layer's terminator, after which a version's timestamp follows
/// (`nucleus_txn::mvcc`): what a user key's versions share —, else all
/// of it.
fn prefix(key: &[u8]) -> &[u8] {
    match key.windows(2).position(|w| w == [0, 0]) {
        Some(i) => &key[..i + 2],
        None => key,
    }
}

// ------------------------------------------------------------ the filter

const SALT: [u32; 8] = [
    0x47b6_137b,
    0x4497_4d91,
    0x8824_ad5b,
    0xa2b7_289d,
    0x7054_95c7,
    0x2df1_424b,
    0x9efc_4947,
    0x5c6b_fb31,
];

/// A split-block Bloom filter (Apache Parquet's): 256-bit blocks.
#[derive(Clone, Default)]
struct Bloom {
    blocks: Vec<[u32; 8]>,
}

impl Bloom {
    fn with_hashes(hashes: &[u64]) -> Self {
        let n = (hashes.len() * BLOOM_BITS).div_ceil(256).max(1);
        let mut b = Bloom {
            blocks: vec![[0; 8]; n],
        };
        for &h in hashes {
            let i = b.block(h);
            let m = Self::mask(h as u32);
            for (w, m) in b.blocks[i].iter_mut().zip(m) {
                *w |= m;
            }
        }
        b
    }

    fn block(&self, h: u64) -> usize {
        (((h >> 32) * self.blocks.len() as u64) >> 32) as usize
    }

    fn mask(h: u32) -> [u32; 8] {
        SALT.map(|s| 1u32 << (h.wrapping_mul(s) >> 27))
    }

    /// Whether the key whose hash is `h` may be in the set.
    fn may(&self, h: u64) -> bool {
        if self.blocks.is_empty() {
            return true;
        }
        let blk = &self.blocks[self.block(h)];
        blk.iter()
            .zip(Self::mask(h as u32))
            .all(|(w, m)| w & m == m)
    }
}

// ------------------------------------------------------------ ranges

/// Range tombstones: `[start, end)` ranges, apart and sorted.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
struct Ranges(Vec<(Vec<u8>, Vec<u8>)>);

impl Ranges {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Adds `[start, end)`, joining the ranges it meets.
    fn add(&mut self, start: &[u8], end: &[u8]) {
        if start >= end {
            return;
        }
        let (mut s, mut e) = (start.to_vec(), end.to_vec());
        // The ranges that end at or after `start` and begin at or before
        // `end` meet it.
        let first = self.0.partition_point(|r| r.1.as_slice() < start);
        let last = self.0.partition_point(|r| r.0.as_slice() <= end);
        if first < last {
            if self.0[first].0 < s {
                s = self.0[first].0.clone();
            }
            if self.0[last - 1].1 > e {
                e = self.0[last - 1].1.clone();
            }
        }
        self.0.splice(first..last, [(s, e)]);
    }

    fn covers(&self, key: &[u8]) -> bool {
        let i = self.0.partition_point(|r| r.0.as_slice() <= key);
        i > 0 && key < self.0[i - 1].1.as_slice()
    }

    fn union(&mut self, other: &Ranges) {
        for (s, e) in &other.0 {
            self.add(s, e);
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.0.len() as u32).to_le_bytes());
        for (s, e) in &self.0 {
            put_bytes(out, s);
            put_bytes(out, e);
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let n = r.u32()?;
        let mut v = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let s = r.bytes()?.to_vec();
            let e = r.bytes()?.to_vec();
            v.push((s, e));
        }
        Ok(Ranges(v))
    }
}

// ------------------------------------------------------------ bytes

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(corrupt("an LSM file ends early"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4")))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8")))
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

fn read_all(file: &dyn File, at: u64, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0; len];
    let mut done = 0;
    while done < len {
        let n = file.read_at(&mut buf[done..], at + done as u64)?;
        if n == 0 {
            return Err(corrupt("an LSM run ends early"));
        }
        done += n;
    }
    Ok(buf)
}

// ------------------------------------------------------------ memtable

/// Writes not yet in a run.
///
/// Keys written in ascending order past every key it holds — a bulk load
/// into a fresh range, an index built (D16 writes come so) — go to a
/// sorted vector at its end, appended, not into the map: no search, no
/// node a key (§M14.28 L4b).  A key that falls among them moves them into
/// the map first.  Grown past [`SPILL`], the vector is written as a run
/// ([`Engine::spill`]), so a load is not held in memory whole.
#[derive(Clone, Default)]
struct Memtable {
    points: BTreeMap<Vec<u8>, Val>,
    /// About every [`SPLIT_STRIDE`]th key put in `points` (those whose
    /// hash it divides), where a scan may be cut into parts
    /// ([`Engine::split_keys`]) without walking `points`; a key since
    /// removed stays (a cut needs no entry).
    sample: BTreeSet<Vec<u8>>,
    /// Keys above every key of `points`, ascending (shared with a spill
    /// writing them).
    tail: Arc<Vec<(Vec<u8>, Val)>>,
    /// The bytes of `tail`'s keys and values.
    tail_bytes: usize,
    ranges: Ranges,
}

/// A memtable's entries counted as one block when a scan is cut into
/// parts ([`Engine::split_keys`]): about a block's of small rows.
const SPLIT_STRIDE: usize = 64;

/// The bytes of a memtable's ascending tail past which it is spilled as a
/// run (a few KiB in the unit tests, so the model test spills often).
pub const SPILL: usize = if cfg!(test) { 1 << 10 } else { 64 << 20 };

fn entry_bytes(k: &[u8], v: &Val) -> usize {
    k.len() + v.as_ref().map_or(0, Vec::len)
}

impl Memtable {
    fn is_empty(&self) -> bool {
        self.points.is_empty() && self.tail.is_empty() && self.ranges.is_empty()
    }

    /// The last key it holds.
    fn last(&self) -> Option<&[u8]> {
        match self.tail.last() {
            Some((k, _)) => Some(k),
            None => self.points.last_key_value().map(|(k, _)| k.as_slice()),
        }
    }

    /// The tail moved into the map.
    fn untail(&mut self) {
        let tail = std::mem::take(&mut self.tail);
        for (k, _) in tail.iter() {
            self.sampled(k);
        }
        self.points.extend(Arc::unwrap_or_clone(tail));
        self.tail_bytes = 0;
    }

    fn put(&mut self, key: &[u8], v: Val) {
        if self.last().is_none_or(|l| key > l) {
            self.tail_bytes += entry_bytes(key, &v);
            Arc::make_mut(&mut self.tail).push((key.to_vec(), v));
            return;
        }
        if self.tail.first().is_some_and(|(f, _)| key >= f.as_slice()) {
            self.untail();
        }
        self.sampled(key);
        self.points.insert(key.to_vec(), v);
    }

    /// `key`, put in `points`, kept in `sample` if it is one of the keys
    /// sampled.
    fn sampled(&mut self, key: &[u8]) {
        if hash64(key).is_multiple_of(SPLIT_STRIDE as u64) {
            self.sample.insert(key.to_vec());
        }
    }

    /// What `filter` finds garbage among the entries sharing a prefix
    /// ([`prefix`]: a user key's versions) with a key of `ops`, dropped
    /// from the map — as a B+tree drops it from the leaves a commit
    /// rewrites.  Without it a key updated often keeps every version here
    /// until a save, and a read of it steps over them all (YCSB A on a hot
    /// key: 5000 versions, a read 12 → 420 µs).  Older entries of the key
    /// may be in runs, so the filter sees them as above a last level; the
    /// tail (a load's keys, written once) is left as it is.
    fn drop_garbage(&mut self, ops: &Ops, filter: &dyn LeafFilter) {
        let mut gone: Vec<Vec<u8>> = Vec::new();
        let mut last: Option<&[u8]> = None;
        for (k, _) in ops {
            let p = prefix(k);
            if p.len() == k.len() || last == Some(p) {
                continue; // not a version, or its prefix seen
            }
            last = Some(p);
            let mut end = p.to_vec();
            *end.last_mut().expect("a prefix ends 00 00") = 1;
            let live: Vec<(&Vec<u8>, &[u8])> = self
                .points
                .range::<[u8], _>((Bound::Included(p), Bound::Excluded(end.as_slice())))
                .filter_map(|(k, v)| Some((k, v.as_deref()?)))
                .collect();
            if live.len() < 2 {
                continue;
            }
            let entries: Vec<LeafEntry> = live
                .iter()
                .map(|&(key, v)| LeafEntry {
                    key,
                    inline: Some(v),
                })
                .collect();
            let mut drop = vec![false; entries.len()];
            filter.drops_above(&entries, &mut drop);
            gone.extend(
                live.iter()
                    .zip(&drop)
                    .filter(|&(_, &d)| d)
                    .map(|((k, _), _)| (*k).clone()),
            );
        }
        for k in gone {
            self.points.remove(&k);
        }
    }

    /// The tail's entries in `(start, end)`: their positions.
    fn tail_span(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> std::ops::Range<usize> {
        let t = &self.tail;
        let a = match start {
            Bound::Included(s) => t.partition_point(|(k, _)| k.as_slice() < s),
            Bound::Excluded(s) => t.partition_point(|(k, _)| k.as_slice() <= s),
            Bound::Unbounded => 0,
        };
        let b = match end {
            Bound::Included(e) => t.partition_point(|(k, _)| k.as_slice() <= e),
            Bound::Excluded(e) => t.partition_point(|(k, _)| k.as_slice() < e),
            Bound::Unbounded => t.len(),
        };
        a..b.max(a)
    }

    /// Whether it holds a point or a range tombstone in `[first, last]`.
    fn touches(&self, first: &[u8], last: &[u8]) -> bool {
        let span = (Bound::Included(first), Bound::Included(last));
        self.points.range::<[u8], _>(span).next().is_some()
            || !self.tail_span(span.0, span.1).is_empty()
            || self
                .ranges
                .0
                .iter()
                .any(|(s, e)| s.as_slice() <= last && e.as_slice() > first)
    }

    fn remove_range(&mut self, start: &[u8], end: &[u8]) {
        if start >= end {
            return;
        }
        let keys: Vec<Vec<u8>> = self
            .points
            .range::<[u8], _>((Bound::Included(start), Bound::Excluded(end)))
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys {
            self.points.remove(&k);
        }
        let span = self.tail_span(Bound::Included(start), Bound::Excluded(end));
        if !span.is_empty() {
            let gone: usize = Arc::make_mut(&mut self.tail)
                .drain(span)
                .map(|(k, v)| entry_bytes(&k, &v))
                .sum();
            self.tail_bytes -= gone;
        }
        self.ranges.add(start, end);
    }

    /// Every point, ascending.
    fn entries(&self) -> impl Iterator<Item = (&Vec<u8>, &Val)> {
        self.points
            .iter()
            .chain(self.tail.iter().map(|(k, v)| (k, v)))
    }

    /// `newer` written over this one.
    fn absorb(&mut self, newer: &Memtable) {
        for (s, e) in &newer.ranges.0 {
            self.remove_range(s, e);
        }
        for (k, v) in newer.entries() {
            self.put(k, v.clone());
        }
    }

    fn lookup(&self, key: &[u8]) -> Option<Val> {
        if let Some(v) = self.points.get(key) {
            return Some(v.clone());
        }
        if let Ok(i) = self.tail.binary_search_by(|(k, _)| k.as_slice().cmp(key)) {
            return Some(self.tail[i].1.clone());
        }
        self.ranges.covers(key).then_some(None)
    }
}

// ------------------------------------------------------------ runs

/// A block's place in a run.
#[derive(Clone)]
struct BlockRef {
    first: Vec<u8>,
    offset: u64,
    len: u32,
}

/// An immutable sorted run in its file.
pub(crate) struct Run {
    /// Its number in this process, for the block cache.
    id: u64,
    name: String,
    file: Box<dyn File>,
    index: Vec<BlockRef>,
    last: Vec<u8>,
    bloom: Bloom,
    ranges: Ranges,
    size: u64,
}

impl Run {
    fn open(fs: &dyn FileSystem, dir: &Path, name: &str) -> Result<Arc<Run>> {
        let file = fs.open(&dir.join(name), OpenMode::ReadOnly)?;
        let size = file.size()?;
        if size < FOOTER as u64 {
            return Err(corrupt("an LSM run too short"));
        }
        let foot = read_all(&*file, size - FOOTER as u64, FOOTER)?;
        let mut f = Reader(&foot);
        let (meta_at, meta_len, _entries) = (f.u64()?, f.u64()?, f.u64()?);
        if f.take(8)? != RUN_MAGIC {
            return Err(corrupt("not an LSM run"));
        }
        if meta_at.checked_add(meta_len) != Some(size - FOOTER as u64) || meta_len < 4 {
            return Err(corrupt("an LSM run's footer is wrong"));
        }
        let meta = read_all(&*file, meta_at, meta_len as usize)?;
        let (body, sum) = meta.split_at(meta.len() - 4);
        if crc32c(body).to_le_bytes() != sum {
            return Err(corrupt("an LSM run's index fails its checksum"));
        }
        let mut r = Reader(body);
        let n = r.u32()? as usize;
        let mut index = Vec::with_capacity(n);
        for _ in 0..n {
            let first = r.bytes()?.to_vec();
            let offset = r.u64()?;
            let len = r.u32()?;
            index.push(BlockRef { first, offset, len });
        }
        let last = r.bytes()?.to_vec();
        let words = r.u32()? as usize;
        let mut blocks = Vec::with_capacity(words / 8);
        for _ in 0..words / 8 {
            let mut b = [0u32; 8];
            for w in &mut b {
                *w = r.u32()?;
            }
            blocks.push(b);
        }
        let ranges = Ranges::decode(&mut r)?;
        Ok(Arc::new(Run {
            id: next_run_id(),
            name: name.to_owned(),
            file,
            index,
            last,
            bloom: Bloom { blocks },
            ranges,
            size,
        }))
    }

    /// Block `i`, read, checked and decoded.
    fn read_block(&self, i: usize) -> Result<Decoded> {
        let b = &self.index[i];
        let stored = read_all(&*self.file, b.offset, b.len as usize)?;
        if stored.len() < 9 {
            return Err(corrupt("an LSM block too short"));
        }
        let (body, sum) = stored.split_at(stored.len() - 4);
        if crc32c(body).to_le_bytes() != sum {
            return Err(corrupt("an LSM block fails its checksum"));
        }
        let codec = body[0];
        let raw_len = u32::from_le_bytes(body[1..5].try_into().expect("4")) as usize;
        let payload = &body[5..];
        let data = match codec {
            0 => payload.to_vec(),
            1 => {
                let mut out = Vec::new();
                lz::decompress(payload, raw_len, &mut out)
                    .ok_or(corrupt("an LSM block does not decompress"))?;
                out
            }
            _ => return Err(corrupt("an LSM block's codec is unknown")),
        };
        Decoded::new(data)
    }

    /// Block `i`, from the block cache (put there when `fill`: lookups and
    /// scans, not compactions).
    fn block(&self, i: usize, fill: bool) -> Result<Arc<Decoded>> {
        let key = (self.id, i as u32);
        if let Some(d) = CACHE.get(key) {
            return Ok(d);
        }
        let d = Arc::new(self.read_block(i)?);
        if fill {
            CACHE.put(key, Arc::clone(&d));
        }
        Ok(d)
    }

    /// The block that would hold `key`.
    fn block_of(&self, key: &[u8]) -> Option<usize> {
        let i = self.index.partition_point(|b| b.first.as_slice() <= key);
        (i > 0).then(|| i - 1)
    }

    fn lookup(&self, key: &[u8], h: u64) -> Result<Option<Val>> {
        if !self.index.is_empty()
            && key >= self.index[0].first.as_slice()
            && key <= self.last.as_slice()
            && self.bloom.may(h)
            && let Some(i) = self.block_of(key)
        {
            let d = self.block(i, true)?;
            if let Ok(j) = d.find(key) {
                return Ok(Some(d.value(j).map(<[u8]>::to_vec)));
            }
        }
        Ok(self.ranges.covers(key).then_some(None))
    }
}

/// Writes a run's file entry by entry.
struct RunWriter<'f> {
    fs: &'f dyn FileSystem,
    tmp: std::path::PathBuf,
    path: std::path::PathBuf,
    name: String,
    file: Box<dyn File>,
    /// Bytes written so far, `out` among them.
    at: u64,
    /// What is written and not yet in the file: one large write instead of
    /// one a block.
    out: Vec<u8>,
    block: Vec<u8>,
    first: Option<Vec<u8>>,
    index: Vec<BlockRef>,
    hashes: Vec<u64>,
    last: Vec<u8>,
    entries: u64,
}

impl<'f> RunWriter<'f> {
    fn new(fs: &'f dyn FileSystem, dir: &Path, name: &str) -> Result<Self> {
        let path = dir.join(name);
        let tmp = dir.join(format!("{name}.tmp"));
        if fs.exists(&tmp)? {
            fs.remove_file(&tmp)?;
        }
        let file = fs.open(&tmp, OpenMode::CreateNew)?;
        Ok(RunWriter {
            fs,
            tmp,
            path,
            name: name.to_owned(),
            file,
            at: 0,
            out: Vec::new(),
            block: Vec::new(),
            first: None,
            index: Vec::new(),
            hashes: Vec::new(),
            last: Vec::new(),
            entries: 0,
        })
    }

    fn add(&mut self, key: &[u8], v: &Val) -> Result<()> {
        debug_assert!(self.entries == 0 || key > self.last.as_slice());
        if self.first.is_none() {
            self.first = Some(key.to_vec());
        }
        put_bytes(&mut self.block, key);
        match v {
            Some(v) => {
                self.block.push(0);
                put_bytes(&mut self.block, v);
            }
            None => self.block.push(1),
        }
        self.hashes.push(hash64(key));
        self.last.clear();
        self.last.extend_from_slice(key);
        self.entries += 1;
        if self.block.len() >= BLOCK {
            self.flush_block()?;
        }
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        /// Bytes gathered before they go to the file.
        const OUT: usize = 1 << 20;
        self.out.extend_from_slice(bytes);
        self.at += bytes.len() as u64;
        if self.out.len() >= OUT {
            self.flush_out()?;
        }
        Ok(())
    }

    fn flush_out(&mut self) -> Result<()> {
        if !self.out.is_empty() {
            let from = self.at - self.out.len() as u64;
            self.file.write_all_at(&self.out, from)?;
            self.out.clear();
        }
        Ok(())
    }

    fn flush_block(&mut self) -> Result<()> {
        let Some(first) = self.first.take() else {
            return Ok(());
        };
        let mut packed = Vec::new();
        lz::compress(&self.block, &mut packed);
        let mut stored = Vec::with_capacity(self.block.len() + 9);
        if packed.len() + packed.len() / 8 < self.block.len() {
            stored.push(1);
            stored.extend_from_slice(&(self.block.len() as u32).to_le_bytes());
            stored.extend_from_slice(&packed);
        } else {
            stored.push(0);
            stored.extend_from_slice(&(self.block.len() as u32).to_le_bytes());
            stored.extend_from_slice(&self.block);
        }
        stored.extend_from_slice(&crc32c(&stored).to_le_bytes());
        self.index.push(BlockRef {
            first,
            offset: self.at,
            len: stored.len() as u32,
        });
        self.write(&stored)?;
        self.block.clear();
        Ok(())
    }

    /// Its file removed: the run not made.
    fn abandon(self) -> Result<()> {
        drop(self.file);
        Ok(self.fs.remove_file(&self.tmp)?)
    }

    /// The run, durable under its name (its directory entry made durable
    /// by the manifest's write after it); `None` when it holds nothing.
    fn finish(mut self, ranges: Ranges) -> Result<Option<Arc<Run>>> {
        self.flush_block()?;
        if self.entries == 0 && ranges.is_empty() {
            drop(self.file);
            self.fs.remove_file(&self.tmp)?;
            return Ok(None);
        }
        let bloom = Bloom::with_hashes(&self.hashes);
        let mut meta = Vec::new();
        meta.extend_from_slice(&(self.index.len() as u32).to_le_bytes());
        for b in &self.index {
            put_bytes(&mut meta, &b.first);
            meta.extend_from_slice(&b.offset.to_le_bytes());
            meta.extend_from_slice(&b.len.to_le_bytes());
        }
        put_bytes(&mut meta, &self.last);
        meta.extend_from_slice(&((bloom.blocks.len() * 8) as u32).to_le_bytes());
        for b in &bloom.blocks {
            for w in b {
                meta.extend_from_slice(&w.to_le_bytes());
            }
        }
        ranges.encode(&mut meta);
        meta.extend_from_slice(&crc32c(&meta).to_le_bytes());
        let meta_at = self.at;
        self.write(&meta)?;
        let mut foot = Vec::with_capacity(FOOTER);
        foot.extend_from_slice(&meta_at.to_le_bytes());
        foot.extend_from_slice(&(meta.len() as u64).to_le_bytes());
        foot.extend_from_slice(&self.entries.to_le_bytes());
        foot.extend_from_slice(RUN_MAGIC);
        self.write(&foot)?;
        self.flush_out()?;
        self.file.sync()?;
        let size = self.at;
        drop(self.file);
        self.fs.rename(&self.tmp, &self.path)?;
        let file = self.fs.open(&self.path, OpenMode::ReadOnly)?;
        Ok(Some(Arc::new(Run {
            id: next_run_id(),
            name: self.name,
            file,
            index: self.index,
            last: self.last,
            bloom,
            ranges,
            size,
        })))
    }
}

// ------------------------------------------------------------ scans

/// One source's entries in `(start, end)`, one direction.
enum Src<'a> {
    Mem(std::iter::Peekable<Box<dyn Iterator<Item = (&'a Vec<u8>, &'a Val)> + 'a>>),
    Run(RunScan<'a>),
}

impl Src<'_> {
    /// The next entry into `key` and `val` (their buffers reused); false
    /// at the end.
    fn next_into(&mut self, key: &mut Vec<u8>, val: &mut Val) -> Result<bool> {
        match self {
            Src::Mem(it) => Ok(it.next().is_some_and(|(k, v)| {
                set_entry(key, val, k, v.as_deref());
                true
            })),
            Src::Run(r) => r.next_into(key, val),
        }
    }

    /// [`Entries::leaf_entries`] forward: a run's block, or up to
    /// [`MEM_BATCH`] of a memtable's entries; tombstones passed over.
    fn batch(
        &mut self,
        key: &mut Vec<u8>,
        prev: &mut Vec<u8>,
        max: usize,
        each: &mut dyn FnMut(&[u8], &[u8], Option<&[u8]>) -> Result<bool>,
    ) -> Result<bool> {
        match self {
            Src::Mem(it) => {
                for _ in 0..MEM_BATCH {
                    let Some(&(k, v)) = it.peek() else {
                        return Ok(false);
                    };
                    if let Some(v) = v
                        && !give(key, prev, k, v, max, each)?
                    {
                        return Ok(true);
                    }
                    it.next();
                }
                Ok(true)
            }
            Src::Run(r) => r.batch(key, prev, max, each),
        }
    }
}

/// A memtable's entries [`Src::batch`] gives a call.
const MEM_BATCH: usize = 256;

/// An entry `k`, `v` to `each` as [`Entries::leaf_entries`] gives it:
/// false if declined (`key` and `prev` as they were).
fn give(
    key: &mut Vec<u8>,
    prev: &mut Vec<u8>,
    k: &[u8],
    v: &[u8],
    max: usize,
    each: &mut dyn FnMut(&[u8], &[u8], Option<&[u8]>) -> Result<bool>,
) -> Result<bool> {
    std::mem::swap(key, prev);
    key.clear();
    key.extend_from_slice(k);
    if each(prev, key, (v.len() <= max).then_some(v))? {
        return Ok(true);
    }
    std::mem::swap(key, prev);
    Ok(false)
}

/// `k` and `v` into `key` and `val`, their buffers reused.
fn set_entry(key: &mut Vec<u8>, val: &mut Val, k: &[u8], v: Option<&[u8]>) {
    key.clear();
    key.extend_from_slice(k);
    match (v, val.as_mut()) {
        (Some(v), Some(buf)) => {
            buf.clear();
            buf.extend_from_slice(v);
        }
        (v, _) => *val = v.map(<[u8]>::to_vec),
    }
}

fn mem_scan<'a>(m: &'a Memtable, start: Bound<&[u8]>, end: Bound<&[u8]>, rev: bool) -> Src<'a> {
    if empty_bounds(start, end) {
        let it: Box<dyn Iterator<Item = _>> = Box::new(std::iter::empty());
        return Src::Mem(it.peekable());
    }
    let r = m.points.range::<[u8], _>((start, end));
    let t = m.tail[m.tail_span(start, end)].iter().map(|(k, v)| (k, v));
    if rev {
        let it: Box<dyn Iterator<Item = _>> = Box::new(t.rev().chain(r.rev()));
        Src::Mem(it.peekable())
    } else {
        let it: Box<dyn Iterator<Item = _>> = Box::new(r.chain(t));
        Src::Mem(it.peekable())
    }
}

/// Whether `(start, end)` holds no key (`BTreeMap::range` refuses them).
fn empty_bounds(start: Bound<&[u8]>, end: Bound<&[u8]>) -> bool {
    match (start, end) {
        (Bound::Included(s), Bound::Included(e)) => s > e,
        (Bound::Included(s), Bound::Excluded(e))
        | (Bound::Excluded(s), Bound::Included(e))
        | (Bound::Excluded(s), Bound::Excluded(e)) => s >= e,
        _ => false,
    }
}

fn above(start: &Bound<Vec<u8>>, k: &[u8]) -> bool {
    match start {
        Bound::Unbounded => true,
        Bound::Included(s) => k >= s.as_slice(),
        Bound::Excluded(s) => k > s.as_slice(),
    }
}

fn below(end: &Bound<Vec<u8>>, k: &[u8]) -> bool {
    match end {
        Bound::Unbounded => true,
        Bound::Included(e) => k <= e.as_slice(),
        Bound::Excluded(e) => k < e.as_slice(),
    }
}

/// A run's entries in `(start, end)`, a block at a time.
struct RunScan<'a> {
    run: &'a Run,
    start: Bound<Vec<u8>>,
    end: Bound<Vec<u8>>,
    rev: bool,
    /// Whether the blocks read go into the block cache.
    fill: bool,
    /// The next block to read (forward) or the one past it (reverse).
    next: usize,
    /// The block being read, and the next entry of it (forward) or the
    /// one past it (reverse).
    block: Option<Arc<Decoded>>,
    at: usize,
    /// Whether the block is the first read (where the range starts).
    first: bool,
    done: bool,
}

impl<'a> RunScan<'a> {
    fn new(run: &'a Run, start: Bound<&[u8]>, end: Bound<&[u8]>, rev: bool, fill: bool) -> Self {
        let own = |b: Bound<&[u8]>| b.map(<[u8]>::to_vec);
        let next = if rev {
            match end {
                Bound::Unbounded => run.index.len(),
                Bound::Included(k) | Bound::Excluded(k) => run.block_of(k).map_or(0, |i| i + 1),
            }
        } else {
            match start {
                Bound::Unbounded => 0,
                Bound::Included(k) | Bound::Excluded(k) => run.block_of(k).unwrap_or(0),
            }
        };
        RunScan {
            run,
            start: own(start),
            end: own(end),
            rev,
            fill,
            next,
            block: None,
            at: 0,
            first: true,
            done: empty_bounds(start, end),
        }
    }

    fn next_into(&mut self, key: &mut Vec<u8>, val: &mut Val) -> Result<bool> {
        loop {
            if self.done {
                return Ok(false);
            }
            if let Some(d) = &self.block {
                let j = if self.rev {
                    if self.at == 0 {
                        self.block = None;
                        continue;
                    }
                    self.at -= 1;
                    self.at
                } else {
                    if self.at >= d.len() {
                        self.block = None;
                        continue;
                    }
                    self.at += 1;
                    self.at - 1
                };
                let k = d.key(j);
                let inside = if self.rev {
                    above(&self.start, k)
                } else {
                    below(&self.end, k)
                };
                if !inside {
                    self.done = true;
                    return Ok(false);
                }
                set_entry(key, val, k, d.value(j));
                return Ok(true);
            }
            self.load()?;
        }
    }

    /// Forward, the rest of the block reached (the next read if none) to
    /// `each` ([`Src::batch`]); false once the range is done.
    fn batch(
        &mut self,
        key: &mut Vec<u8>,
        prev: &mut Vec<u8>,
        max: usize,
        each: &mut dyn FnMut(&[u8], &[u8], Option<&[u8]>) -> Result<bool>,
    ) -> Result<bool> {
        while self.block.is_none() && !self.done {
            self.load()?;
        }
        let Some(d) = self.block.clone() else {
            return Ok(false);
        };
        while self.at < d.len() {
            let k = d.key(self.at);
            if !below(&self.end, k) {
                self.done = true;
                return Ok(false);
            }
            if let Some(v) = d.value(self.at)
                && !give(key, prev, k, v, max, each)?
            {
                return Ok(true);
            }
            self.at += 1;
        }
        self.block = None;
        Ok(true)
    }

    /// The next block read (`done` if none is left), from where the range
    /// starts in it if it is the first.
    fn load(&mut self) -> Result<()> {
        {
            let i = if self.rev {
                if self.next == 0 {
                    self.done = true;
                    return Ok(());
                }
                self.next -= 1;
                self.next
            } else {
                if self.next >= self.run.index.len() {
                    self.done = true;
                    return Ok(());
                }
                self.next += 1;
                self.next - 1
            };
            let d = self.run.block(i, self.fill)?;
            // The first block: from where the range starts in it.
            self.at = if self.rev {
                if self.first {
                    match &self.end {
                        Bound::Unbounded => d.len(),
                        Bound::Included(e) => d.find(e).map_or_else(|p| p, |p| p + 1),
                        Bound::Excluded(e) => d.find(e).unwrap_or_else(|p| p),
                    }
                } else {
                    d.len()
                }
            } else if self.first {
                match &self.start {
                    Bound::Unbounded => 0,
                    Bound::Included(s) => d.find(s).unwrap_or_else(|p| p),
                    Bound::Excluded(s) => d.find(s).map_or_else(|p| p, |p| p + 1),
                }
            } else {
                0
            };
            self.first = false;
            self.block = Some(d);
        }
        Ok(())
    }
}

// ------------------------------------------------------------ blocks

/// A block decoded: its entries' bytes as stored (a key, a kind, a value)
/// and where each entry starts — searched and read in place, no entry
/// copied until it is returned.
pub(crate) struct Decoded {
    data: Vec<u8>,
    /// Each entry's key: (start, length); its kind byte follows the key.
    keys: Vec<(u32, u32)>,
}

impl Decoded {
    /// `data` checked whole: every entry's lengths within it, every kind
    /// known.
    fn new(data: Vec<u8>) -> Result<Self> {
        let mut keys = Vec::new();
        let mut r = Reader(&data);
        while !r.0.is_empty() {
            let at = data.len() - r.0.len();
            let k = r.bytes()?;
            keys.push(((at + 4) as u32, k.len() as u32));
            match r.u8()? {
                0 => {
                    r.bytes()?;
                }
                1 => {}
                _ => return Err(corrupt("an LSM entry's kind is unknown")),
            }
        }
        Ok(Decoded { data, keys })
    }

    fn len(&self) -> usize {
        self.keys.len()
    }

    fn key(&self, j: usize) -> &[u8] {
        let (a, n) = self.keys[j];
        &self.data[a as usize..(a + n) as usize]
    }

    fn value(&self, j: usize) -> Option<&[u8]> {
        let (a, n) = self.keys[j];
        let at = (a + n) as usize;
        match self.data[at] {
            0 => {
                let n = u32::from_le_bytes(self.data[at + 1..at + 5].try_into().expect("4"));
                Some(&self.data[at + 5..at + 5 + n as usize])
            }
            _ => None,
        }
    }

    /// Where `key` is (`Ok`), or would go (`Err`).
    fn find(&self, key: &[u8]) -> std::result::Result<usize, usize> {
        let (mut lo, mut hi) = (0, self.keys.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.key(mid).cmp(key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Ok(mid),
            }
        }
        Err(lo)
    }

    /// Bytes it holds in memory.
    fn bytes(&self) -> usize {
        self.data.len() + self.keys.len() * 8 + 64
    }
}

static RUN_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_run_id() -> u64 {
    RUN_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The block cache (PLAN.md §M14.28 L4c): runs' blocks decoded, shared by
/// every LSM tree of the process — a block read once is not read, checked
/// and decompressed again while it stays.  CLOCK replacement (a block
/// used since the hand passed it gets a second chance), in shards each
/// under its own lock, within [`set_block_cache_bytes`].  Compactions
/// read through it without filling it.
pub(crate) static CACHE: BlockCache = BlockCache::new();

const SHARDS: usize = 16;

pub(crate) struct BlockCache {
    shards: [Mutex<Shard>; SHARDS],
    /// Bytes it may hold, all shards together.
    capacity: std::sync::atomic::AtomicUsize,
}

struct Shard {
    map: Option<std::collections::HashMap<(u64, u32), usize>>,
    slots: Vec<Slot>,
    hand: usize,
    bytes: usize,
}

struct Slot {
    key: (u64, u32),
    block: Arc<Decoded>,
    used: bool,
}

/// What the block cache holds at most, unless set: 64 MiB.
pub const BLOCK_CACHE_BYTES: usize = 64 << 20;

/// Sets what the block cache holds at most: the largest a store asked
/// for ([`crate::StoreOptions::lsm_cache_bytes`]), [`BLOCK_CACHE_BYTES`]
/// until one does.
pub fn set_block_cache_bytes(bytes: usize) {
    CACHE
        .capacity
        .fetch_max(bytes, std::sync::atomic::Ordering::Relaxed);
}

impl BlockCache {
    const fn new() -> Self {
        BlockCache {
            shards: [const {
                Mutex::new(Shard {
                    map: None,
                    slots: Vec::new(),
                    hand: 0,
                    bytes: 0,
                })
            }; SHARDS],
            capacity: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn shard(&self, key: (u64, u32)) -> std::sync::MutexGuard<'_, Shard> {
        let h = mix(key.0 ^ (u64::from(key.1) << 32)) as usize % SHARDS;
        self.shards[h].lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, key: (u64, u32)) -> Option<Arc<Decoded>> {
        let mut s = self.shard(key);
        let i = *s.map.as_ref()?.get(&key)?;
        let slot = &mut s.slots[i];
        slot.used = true;
        Some(Arc::clone(&slot.block))
    }

    fn put(&self, key: (u64, u32), block: Arc<Decoded>) {
        let cap = match self.capacity.load(std::sync::atomic::Ordering::Relaxed) {
            0 => BLOCK_CACHE_BYTES,
            c => c,
        } / SHARDS;
        let size = block.bytes();
        if size > cap {
            return;
        }
        let mut guard = self.shard(key);
        let s = &mut *guard;
        let map = s.map.get_or_insert_with(Default::default);
        if map.contains_key(&key) {
            return;
        }
        // Room made: the hand goes round, clearing the used and taking
        // the first not used since.
        while s.bytes + size > cap && !s.slots.is_empty() {
            if s.hand >= s.slots.len() {
                s.hand = 0;
            }
            if s.slots[s.hand].used {
                s.slots[s.hand].used = false;
                s.hand += 1;
                continue;
            }
            let gone = s.slots.swap_remove(s.hand);
            map.remove(&gone.key);
            s.bytes -= gone.block.bytes();
            if s.hand < s.slots.len() {
                map.insert(s.slots[s.hand].key, s.hand);
            }
        }
        s.bytes += size;
        map.insert(key, s.slots.len());
        s.slots.push(Slot {
            key,
            block,
            used: false,
        });
    }
}

/// A source's next entry in a merge (its buffers reused).
#[derive(Clone, Default)]
struct Head {
    key: Vec<u8>,
    val: Val,
    /// Whether it holds one (else the source is done).
    some: bool,
}

/// Sources merged, newest first: what a reader sees.
struct Merged<'a> {
    srcs: Vec<Src<'a>>,
    heads: Vec<Head>,
    /// Each source's range tombstones (they hide older sources' keys).
    ranges: Vec<&'a Ranges>,
    rev: bool,
    /// Tombstones and hidden keys passed over (a compaction keeps them).
    keep_tombstones: bool,
    started: bool,
    /// The entry given last.
    key: Vec<u8>,
    cur: Val,
}

impl<'a> Merged<'a> {
    fn new(srcs: Vec<Src<'a>>, ranges: Vec<&'a Ranges>, rev: bool, keep_tombstones: bool) -> Self {
        let n = srcs.len();
        Merged {
            srcs,
            heads: vec![Head::default(); n],
            ranges,
            rev,
            keep_tombstones,
            started: false,
            key: Vec::new(),
            cur: None,
        }
    }

    /// The next entry, into `key` and `cur` (a tombstone only when kept);
    /// false at the end.  The entry's buffers are swapped with the
    /// source's head's, so no entry is copied but into them.
    fn advance(&mut self) -> Result<bool> {
        // One source: nothing to merge, its entries straight in.
        if let [src] = self.srcs.as_mut_slice() {
            while src.next_into(&mut self.key, &mut self.cur)? {
                if self.cur.is_some() || self.keep_tombstones {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        if !self.started {
            for (h, src) in self.heads.iter_mut().zip(&mut self.srcs) {
                h.some = src.next_into(&mut h.key, &mut h.val)?;
            }
            self.started = true;
        }
        loop {
            // The least key (greatest, reverse), from the newest source
            // holding it.
            let mut best: Option<usize> = None;
            for (i, h) in self.heads.iter().enumerate() {
                if !h.some {
                    continue;
                }
                best = match best {
                    None => Some(i),
                    Some(b) => {
                        let bk = &self.heads[b].key;
                        let better = if self.rev { h.key > *bk } else { h.key < *bk };
                        Some(if better { i } else { b })
                    }
                };
            }
            let Some(b) = best else {
                return Ok(false);
            };
            std::mem::swap(&mut self.key, &mut self.heads[b].key);
            std::mem::swap(&mut self.cur, &mut self.heads[b].val);
            let h = &mut self.heads[b];
            h.some = self.srcs[b].next_into(&mut h.key, &mut h.val)?;
            // The same key in older sources: passed over.
            for (h, src) in self.heads.iter_mut().zip(&mut self.srcs).skip(b + 1) {
                if h.some && h.key == self.key {
                    h.some = src.next_into(&mut h.key, &mut h.val)?;
                }
            }
            if self.ranges[..b].iter().any(|r| r.covers(&self.key)) {
                continue;
            }
            if self.cur.is_none() && !self.keep_tombstones {
                continue;
            }
            return Ok(true);
        }
    }

    /// The next entry, owned (a compaction's).
    fn next_entry(&mut self) -> Result<Option<(Vec<u8>, Val)>> {
        Ok(self
            .advance()?
            .then(|| (self.key.clone(), self.cur.clone())))
    }
}

impl Entries for Merged<'_> {
    fn next_key_into(&mut self, key: &mut Vec<u8>) -> Result<bool> {
        if self.advance()? {
            // Its buffer given, the caller's taken for the next.
            std::mem::swap(key, &mut self.key);
            Ok(true)
        } else {
            self.cur = None;
            Ok(false)
        }
    }

    fn value_into(&mut self, val: &mut Vec<u8>) -> Result<()> {
        val.clear();
        if let Some(v) = &self.cur {
            val.extend_from_slice(v);
        }
        Ok(())
    }

    /// One source's, forward, a batch at a time ([`Src::batch`]); several
    /// sources merged give an entry at a time.
    fn leaf_entries(
        &mut self,
        key: &mut Vec<u8>,
        prev: &mut Vec<u8>,
        max: usize,
        each: &mut dyn FnMut(&[u8], &[u8], Option<&[u8]>) -> Result<bool>,
    ) -> Result<Option<bool>> {
        match self.srcs.as_mut_slice() {
            [src] if !self.rev && !self.keep_tombstones => {
                Ok(Some(src.batch(key, prev, max, each)?))
            }
            _ => Ok(None),
        }
    }
}

// ------------------------------------------------------------ levels

/// The runs: tiered levels (each newest first), then the last level's
/// one run.
#[derive(Clone, Default)]
struct Levels {
    tiers: Vec<Vec<Arc<Run>>>,
    bottom: Option<Arc<Run>>,
}

impl Levels {
    fn is_empty(&self) -> bool {
        self.tiers.iter().all(Vec::is_empty) && self.bottom.is_none()
    }

    /// Newest first.
    fn runs(&self) -> impl Iterator<Item = &Arc<Run>> {
        self.tiers.iter().flatten().chain(self.bottom.iter())
    }

    fn names(&self) -> Vec<String> {
        self.runs().map(|r| r.name.clone()).collect()
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = MANIFEST_MAGIC.to_vec();
        out.extend_from_slice(&(self.tiers.len() as u32).to_le_bytes());
        for t in &self.tiers {
            out.extend_from_slice(&(t.len() as u32).to_le_bytes());
            for r in t {
                put_bytes(&mut out, r.name.as_bytes());
            }
        }
        match &self.bottom {
            Some(r) => {
                out.push(1);
                put_bytes(&mut out, r.name.as_bytes());
            }
            None => out.push(0),
        }
        out
    }

    fn decode(fs: &dyn FileSystem, dir: &Path, bytes: &[u8]) -> Result<Self> {
        let mut r = Reader(bytes);
        if r.take(8)? != MANIFEST_MAGIC {
            return Err(corrupt("not an LSM manifest"));
        }
        let name = |r: &mut Reader<'_>| -> Result<String> {
            String::from_utf8(r.bytes()?.to_vec()).map_err(|_| corrupt("an LSM run's name"))
        };
        let mut tiers = Vec::new();
        for _ in 0..r.u32()? {
            let mut t = Vec::new();
            for _ in 0..r.u32()? {
                t.push(Run::open(fs, dir, &name(&mut r)?)?);
            }
            tiers.push(t);
        }
        let bottom = match r.u8()? {
            0 => None,
            _ => Some(Run::open(fs, dir, &name(&mut r)?)?),
        };
        Ok(Levels { tiers, bottom })
    }
}

/// Merges `runs` (newest first) into one run named `name`; tombstones
/// dropped unless `keep`.  `None` when it holds nothing, or when `wrote`
/// (told the bytes written as they go) says to stop: then its file is
/// removed.
fn merge_runs(
    fs: &dyn FileSystem,
    dir: &Path,
    name: &str,
    runs: &[Arc<Run>],
    keep: bool,
    filter: Option<&dyn LeafFilter>,
    wrote: &dyn Fn(u64) -> bool,
) -> Result<Option<Arc<Run>>> {
    /// Bytes between two calls of `wrote`.
    const STEP: u64 = 1 << 20;
    /// Entries the filter is shown at a time (as a B+tree leaf).
    const CHUNK: usize = 512;
    let srcs = runs
        .iter()
        .map(|r| {
            Src::Run(RunScan::new(
                r,
                Bound::Unbounded,
                Bound::Unbounded,
                false,
                false,
            ))
        })
        .collect();
    let ranges = runs.iter().map(|r| &r.ranges).collect();
    let mut m = Merged::new(srcs, ranges, false, keep);
    let mut w = RunWriter::new(fs, dir, name)?;
    let mut told = 0;
    let mut chunk: Vec<(Vec<u8>, Val)> = Vec::with_capacity(CHUNK);
    let mut drop = Vec::with_capacity(CHUNK);
    loop {
        let next = m.next_entry()?;
        let end = next.is_none();
        if let Some(e) = next {
            chunk.push(e);
        }
        if chunk.len() < CHUNK && !end {
            continue;
        }
        // The garbage the layer above knows of dropped (§M14.28 L4d): it
        // sees the entries with values (tombstones pass as they are); a
        // merge above the last level keeps what only nothing older left
        // makes garbage.
        drop.clear();
        drop.resize(chunk.len(), false);
        if let Some(f) = filter {
            let live: Vec<usize> = (0..chunk.len()).filter(|&i| chunk[i].1.is_some()).collect();
            let entries: Vec<LeafEntry> = live
                .iter()
                .map(|&i| LeafEntry {
                    key: &chunk[i].0,
                    inline: chunk[i].1.as_deref(),
                })
                .collect();
            let mut gone = vec![false; entries.len()];
            if keep {
                f.drops_above(&entries, &mut gone);
            } else {
                f.drops(&entries, &mut gone);
            }
            for (j, &i) in live.iter().enumerate() {
                drop[i] = gone[j];
            }
        }
        for (i, (k, v)) in chunk.drain(..).enumerate() {
            if !drop[i] {
                w.add(&k, &v)?;
            }
        }
        if w.at >= told + STEP {
            if !wrote(w.at - told) {
                w.abandon()?;
                return Ok(None);
            }
            told = w.at;
        }
        if end {
            break;
        }
    }
    let mut ranges = Ranges::default();
    if keep {
        for r in runs {
            ranges.union(&r.ranges);
        }
    }
    let at = w.at;
    let run = w.finish(ranges)?;
    wrote(at - told);
    Ok(run)
}

// ------------------------------------------------------------ the engine

/// [`crate::EngineKind::Lsm`].
pub struct LsmEngine {
    mem: Memtable,
    /// The memtable a checkpoint set apart, until its run is taken in.
    sealed: Option<Arc<Memtable>>,
    levels: Levels,
    /// What its saves and its last manifest leave, shared with its savers.
    side: Arc<Mutex<Side>>,
}

/// What an engine's savers leave it.
#[derive(Default)]
struct Side {
    /// The run the last save wrote of the memtable set apart (`None`
    /// within when it held nothing), taken in at the next change.
    saved: Option<Option<Arc<Run>>>,
    /// The runs its last manifest names (written or loaded): kept while a
    /// crash would open it, whatever compaction has made of them since.
    manifest: Vec<String>,
}

/// Where a compaction's run goes.
enum Place {
    /// First in this tier.
    Tier(usize),
    /// The last level (the old one among the inputs).
    Bottom,
    /// A new tier after the last one, above the last level.
    NewTier,
}

/// A spill's result ([`Engine::install`]): the first `n` entries of the
/// memtable's tail, the last of them `last`, written as `run`.
struct Spilled {
    n: usize,
    last: Vec<u8>,
    run: Option<Arc<Run>>,
}

/// A compaction's result ([`Engine::install`]).
struct Compacted {
    /// The tier merged: its oldest runs, these.
    tier: usize,
    inputs: Vec<String>,
    /// The last level's run, when merged too.
    bottom: Option<String>,
    run: Option<Arc<Run>>,
    place: Place,
}

impl Default for LsmEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl LsmEngine {
    /// An empty one.
    pub fn new() -> Self {
        LsmEngine {
            mem: Memtable::default(),
            sealed: None,
            levels: Levels::default(),
            side: Arc::default(),
        }
    }

    /// The one a manifest in `path` names, its runs in `dir`.
    pub fn load(fs: &dyn FileSystem, dir: &Path, path: &Path) -> Result<Self> {
        let bytes = crate::engines::read_file(fs, path)?;
        let mut e = Self::new();
        e.levels = Levels::decode(fs, dir, &bytes)?;
        e.side.lock().unwrap_or_else(|e| e.into_inner()).manifest = e.levels.names();
        Ok(e)
    }

    fn side(&self) -> std::sync::MutexGuard<'_, Side> {
        self.side.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The last save's run taken in (a change follows).
    fn settle(&mut self) {
        let saved = self.side().saved.take();
        if let Some(run) = saved {
            Self::put_first(&mut self.levels, run);
            self.sealed = None;
        }
    }

    /// `run` (a flush's) first in the first tier.
    fn put_first(levels: &mut Levels, run: Option<Arc<Run>>) {
        if let Some(r) = run {
            if levels.tiers.is_empty() {
                levels.tiers.push(Vec::new());
            }
            levels.tiers[0].insert(0, r);
        }
    }

    /// The runs now, with a save's not yet taken in.
    fn current(&self) -> Levels {
        let mut l = self.levels.clone();
        if let Some(run) = self.side().saved.clone() {
            Self::put_first(&mut l, run);
        }
        l
    }

    /// Newest first: the memtable, the one set apart, the runs.
    fn mems(&self) -> impl Iterator<Item = &Memtable> {
        std::iter::once(&self.mem).chain(self.sealed.as_deref())
    }

    /// The runs, per level: (tiers, bottom).
    pub fn shape(&self) -> (Vec<usize>, bool) {
        let l = self.current();
        (l.tiers.iter().map(Vec::len).collect(), l.bottom.is_some())
    }

    /// The next merge lazy leveling asks of `levels`: the first tier
    /// holding [`FANIN`] runs or more, merged whole — into the next tier,
    /// or at the last into the last level (when that is at most `FANIN`
    /// times their size; above it otherwise).
    fn plan(levels: &Levels) -> Option<(usize, Vec<Arc<Run>>, Option<Arc<Run>>, Place)> {
        let i = levels.tiers.iter().position(|t| t.len() >= FANIN)?;
        let runs = levels.tiers[i].clone();
        if i + 1 < levels.tiers.len() {
            return Some((i, runs, None, Place::Tier(i + 1)));
        }
        let size: u64 = runs.iter().map(|r| r.size).sum();
        Some(match &levels.bottom {
            None => (i, runs, None, Place::Bottom),
            Some(b) if b.size <= FANIN as u64 * size => {
                (i, runs, Some(Arc::clone(b)), Place::Bottom)
            }
            Some(_) => (i, runs, None, Place::NewTier),
        })
    }
}

impl Engine for LsmEngine {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        for m in self.mems() {
            if let Some(v) = m.lookup(key) {
                return Ok(v);
            }
        }
        let h = hash64(key);
        for r in self.levels.runs() {
            if let Some(v) = r.lookup(key, h)? {
                return Ok(v);
            }
        }
        Ok(None)
    }

    fn scan<'a>(
        &'a self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        reverse: bool,
    ) -> Result<Box<dyn Entries + 'a>> {
        let mut srcs = Vec::new();
        let mut ranges = Vec::new();
        // Sources that hold nothing here left out (one source is read
        // unmerged): an empty memtable, a run whose keys are all outside
        // and that has no range tombstone (which would hide older runs').
        for m in self.mems().filter(|m| !m.is_empty()) {
            srcs.push(mem_scan(m, start, end, reverse));
            ranges.push(&m.ranges);
        }
        let (s, e) = (start.map(<[u8]>::to_vec), end.map(<[u8]>::to_vec));
        for r in self.levels.runs() {
            let outside = r
                .index
                .first()
                .is_none_or(|f| !below(&e, &f.first) || !above(&s, &r.last));
            if outside && r.ranges.is_empty() {
                continue;
            }
            srcs.push(Src::Run(RunScan::new(r, start, end, reverse, true)));
            ranges.push(&r.ranges);
        }
        Ok(Box::new(Merged::new(srcs, ranges, reverse, false)))
    }

    fn apply(&mut self, ops: &Ops) -> Result<()> {
        self.settle();
        for (k, v) in ops {
            self.mem.put(k, v.clone());
        }
        Ok(())
    }

    /// The runs' blocks' first keys and every [`SPLIT_STRIDE`]th key of the
    /// memtables, evenly chosen: a block of any run, or as many entries in
    /// memory, about as much reading as another (§M14.28 L5 — without
    /// them an LSM table's scans ran on one thread, `count(*)` of a
    /// million rows 98 ms against the B+tree's 6.4).
    fn split_keys(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        parts: usize,
        min_blocks: usize,
    ) -> Result<Vec<Vec<u8>>> {
        if end.is_some_and(|e| e <= start) {
            return Ok(Vec::new());
        }
        let inside = |k: &[u8]| k > start && end.is_none_or(|e| k < e);
        let mut keys: Vec<&[u8]> = self
            .levels
            .runs()
            .flat_map(|r| r.index.iter().map(|b| b.first.as_slice()))
            .filter(|k| inside(k))
            .collect();
        let span = (
            Bound::Excluded(start),
            end.map_or(Bound::Unbounded, Bound::Excluded),
        );
        for m in self.mems() {
            let tail = &m.tail[m.tail_span(span.0, span.1)];
            keys.extend(tail.iter().step_by(SPLIT_STRIDE).map(|(k, _)| k.as_slice()));
            keys.extend(m.sample.range::<[u8], _>(span).map(Vec::as_slice));
        }
        keys.sort_unstable();
        keys.dedup();
        let parts = parts.min((keys.len() + 1) / min_blocks.max(1)).max(1);
        Ok((1..parts)
            .map(|j| keys[j * keys.len() / parts].to_vec())
            .collect())
    }

    fn apply_filtered(&mut self, ops: &Ops, filter: &dyn LeafFilter) -> Result<()> {
        self.apply(ops)?;
        self.mem.drop_garbage(ops, filter);
        Ok(())
    }

    fn remove_range(&mut self, start: &[u8], end: &[u8]) -> Result<()> {
        self.settle();
        self.mem.remove_range(start, end);
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.mems().all(Memtable::is_empty) && self.current().is_empty()
    }

    fn fork(&self) -> Box<dyn Engine> {
        let saved = self.side().saved.is_some();
        Box::new(LsmEngine {
            mem: self.mem.clone(),
            sealed: if saved { None } else { self.sealed.clone() },
            levels: self.current(),
            side: Arc::default(),
        })
    }

    fn prepare(&mut self) -> Result<()> {
        self.settle();
        if self.mem.is_empty() {
            return Ok(());
        }
        let mem = std::mem::take(&mut self.mem);
        self.sealed = Some(Arc::new(match self.sealed.take() {
            // A save that did not happen (its checkpoint failed): the
            // newer writes over the ones set apart then.
            Some(old) => {
                let mut m = Arc::unwrap_or_clone(old);
                m.absorb(&mem);
                m
            }
            None => mem,
        }));
        Ok(())
    }

    fn saver(&self) -> Saver {
        let mut levels = self.levels.clone();
        let sealed = self.sealed.clone();
        let side = Arc::clone(&self.side);
        Box::new(move |fs, dir, path, name| {
            let mut run = None;
            if let Some(m) = sealed.as_deref().filter(|m| !m.is_empty()) {
                // Tombstones kept while older runs may hold what they hide.
                let keep = !levels.is_empty();
                // `run-{name}-0`, or the first such name no file has: two
                // checkpoints may share an LSN (writes without the WAL
                // between them), and the earlier one's run is live.
                let mut file = format!("run-{name}-0");
                for n in 1.. {
                    if !fs.exists(&dir.join(&file))?
                        && !fs.exists(&dir.join(format!("{file}.tmp")))?
                    {
                        break;
                    }
                    file = format!("run-{name}-{n}");
                }
                let mut w = RunWriter::new(fs, dir, &file)?;
                for (k, v) in m.entries() {
                    if v.is_some() || keep {
                        w.add(k, v)?;
                    }
                }
                let ranges = if keep {
                    m.ranges.clone()
                } else {
                    Ranges::default()
                };
                run = w.finish(ranges)?;
                Self::put_first(&mut levels, run.clone());
            }
            crate::engines::write_file(fs, path, &levels.encode())?;
            let mut s = side.lock().unwrap_or_else(|e| e.into_inner());
            s.manifest = levels.names();
            s.saved = Some(run);
            Ok(())
        })
    }

    fn files(&self) -> Vec<String> {
        let mut names = self.current().names();
        names.extend(self.side().manifest.iter().cloned());
        names
    }

    fn job(&self) -> Option<Job> {
        let (tier, runs, bottom, place) = Self::plan(&self.current())?;
        Some(Box::new(move |room: &dyn JobRoom| {
            let inputs = runs.iter().map(|r| r.name.clone()).collect();
            let keep = !matches!(place, Place::Bottom);
            let mut all = runs;
            all.extend(bottom.iter().cloned());
            let name = room.name()?;
            let wrote = |n| room.wrote(n);
            let filter = room.filter();
            let run = merge_runs(
                room.fs(),
                room.dir(),
                &name,
                &all,
                keep,
                filter.as_deref().map(|f| f as &dyn LeafFilter),
                &wrote,
            )?;
            if run.is_none() && !room.wrote(0) {
                return Ok(None); // stopped
            }
            let done: Done = Box::new(Compacted {
                tier,
                inputs,
                bottom: bottom.map(|b| b.name.clone()),
                run,
                place,
            });
            Ok(Some(done))
        }))
    }

    fn spill(&self) -> Option<Job> {
        let m = &self.mem;
        if m.tail_bytes < SPILL {
            return None;
        }
        let (first, last) = (m.tail.first()?.0.clone(), m.tail.last()?.0.clone());
        // Below the memtable, the run must not be hidden by its range
        // tombstones, nor hide what the memtable set apart holds: neither
        // may touch its keys (a load into a fresh range never does).
        if m.ranges.0.iter().any(|(s, e)| *s <= last && *e > first)
            || self
                .sealed
                .as_ref()
                .is_some_and(|s| s.touches(&first, &last))
        {
            return None;
        }
        let tail = Arc::clone(&m.tail);
        Some(Box::new(move |room: &dyn JobRoom| {
            let name = room.name()?;
            let mut w = RunWriter::new(room.fs(), room.dir(), &name)?;
            for (k, v) in tail.iter() {
                w.add(k, v)?;
            }
            let run = w.finish(Ranges::default())?;
            let done: Done = Box::new(Spilled {
                n: tail.len(),
                last,
                run,
            });
            Ok(Some(done))
        }))
    }

    fn install(&mut self, done: Done) -> Result<()> {
        let c = match done.downcast::<Spilled>() {
            Ok(sp) => {
                self.settle();
                let t = &self.mem.tail;
                if t.len() >= sp.n && sp.n > 0 && t[sp.n - 1].0 == sp.last {
                    let tail = Arc::make_mut(&mut self.mem.tail);
                    let gone: usize = tail.drain(..sp.n).map(|(k, v)| entry_bytes(&k, &v)).sum();
                    self.mem.tail_bytes -= gone;
                    Self::put_first(&mut self.levels, sp.run);
                }
                return Ok(());
            }
            Err(d) => d,
        };
        let Ok(c) = c.downcast::<Compacted>() else {
            return Ok(());
        };
        self.settle();
        let l = &mut self.levels;
        // The inputs, as planned: the oldest runs of their tier (flushes
        // since went first), and the last level's run.
        let Some(t) = l.tiers.get(c.tier) else {
            return Ok(());
        };
        let n = c.inputs.len();
        let bottom = l.bottom.as_ref().map(|b| &b.name);
        let fits = t.len() >= n
            && t[t.len() - n..].iter().map(|r| &r.name).eq(c.inputs.iter())
            && match (&c.place, &c.bottom) {
                (_, Some(b)) => bottom == Some(b),
                (Place::Bottom, None) => bottom.is_none(),
                _ => true,
            };
        if !fits {
            return Ok(());
        }
        let keep = l.tiers[c.tier].len() - n;
        l.tiers[c.tier].truncate(keep);
        match c.place {
            Place::Tier(i) => {
                if let Some(r) = c.run {
                    l.tiers[i].insert(0, r);
                }
            }
            Place::Bottom => l.bottom = c.run,
            Place::NewTier => {
                if let Some(r) = c.run {
                    l.tiers.push(vec![r]);
                }
            }
        }
        while l.tiers.last().is_some_and(Vec::is_empty) {
            l.tiers.pop();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use nucleus_platform::sim::SimPlatform;
    use nucleus_platform::{Platform, Rng, SplitMix64};

    use super::*;

    type Model = BTreeMap<Vec<u8>, Vec<u8>>;

    /// The keys the model test draws from: enough that the data outgrows
    /// a flush many times, so compaction makes every level.
    const KEYS: u64 = 4000;

    /// Keys shaped as the transaction layer's: a user key, `00 00`, a
    /// version — four to a user key.  In order with `i`.
    fn key(i: u64) -> Vec<u8> {
        let mut k = format!("k{:04}", i / 4).into_bytes();
        k.extend_from_slice(&[0, 0, (i % 4) as u8]);
        k
    }

    fn bound(rng: &SplitMix64) -> Bound<Vec<u8>> {
        match rng.below(3) {
            0 => Bound::Unbounded,
            1 => Bound::Included(key(rng.below(KEYS))),
            _ => Bound::Excluded(key(rng.below(KEYS))),
        }
    }

    fn borrow(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
        match b {
            Bound::Included(k) => Bound::Included(k.as_slice()),
            Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
            Bound::Unbounded => Bound::Unbounded,
        }
    }

    fn collect(
        e: &LsmEngine,
        s: &Bound<Vec<u8>>,
        t: &Bound<Vec<u8>>,
        rev: bool,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut it = e.scan(borrow(s), borrow(t), rev).unwrap();
        let mut out = Vec::new();
        let (mut k, mut v) = (Vec::new(), Vec::new());
        while it.next_key_into(&mut k).unwrap() {
            it.value_into(&mut v).unwrap();
            out.push((k.clone(), v.clone()));
        }
        out
    }

    /// Batches [`collect_batched`] was given (the path is taken).
    static BATCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// A forward scan read a batch at a time where it can be
    /// ([`Entries::leaf_entries`]), each entry declined now and then (read
    /// on its own then, as the transaction layer reads a large value).
    fn collect_batched(
        e: &LsmEngine,
        s: &Bound<Vec<u8>>,
        t: &Bound<Vec<u8>>,
        rng: &SplitMix64,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut it = e.scan(borrow(s), borrow(t), false).unwrap();
        let mut out = Vec::new();
        let (mut k, mut prev, mut v) = (Vec::new(), Vec::new(), Vec::new());
        loop {
            let mut declined = false;
            let mut each = |p: &[u8], key: &[u8], val: Option<&[u8]>| {
                if rng.below(50) == 0 {
                    declined = true;
                    return Ok(false);
                }
                assert!(
                    out.last()
                        .is_none_or(|(l, _): &(Vec<u8>, _)| l.as_slice() == p)
                );
                out.push((key.to_vec(), val.expect("a value fits").to_vec()));
                Ok(true)
            };
            let got = it
                .leaf_entries(&mut k, &mut prev, usize::MAX, &mut each)
                .unwrap();
            if got.is_some() {
                BATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            match got {
                Some(true) if !declined => {}
                Some(false) => return out,
                Some(true) => {
                    // The declined entry, on its own.
                    assert!(it.next_key_into(&mut k).unwrap());
                    it.value_into(&mut v).unwrap();
                    out.push((k.clone(), v.clone()));
                    prev.clone_from(&k);
                }
                None => {
                    while it.next_key_into(&mut k).unwrap() {
                        it.value_into(&mut v).unwrap();
                        out.push((k.clone(), v.clone()));
                    }
                    return out;
                }
            }
        }
    }

    fn check(e: &LsmEngine, model: &Model, rng: &SplitMix64, ctx: &str) {
        for _ in 0..300 {
            let i = rng.below(KEYS);
            assert_eq!(
                e.get(&key(i)).unwrap().as_ref(),
                model.get(&key(i)),
                "{ctx}: get {i}"
            );
        }
        for _ in 0..20 {
            let (s, t) = (bound(rng), bound(rng));
            let want: Vec<(Vec<u8>, Vec<u8>)> = if empty_bounds(borrow(&s), borrow(&t)) {
                Vec::new()
            } else {
                model
                    .range::<[u8], _>((borrow(&s), borrow(&t)))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            };
            assert_eq!(collect(e, &s, &t, false), want, "{ctx}: scan {s:?} {t:?}");
            assert_eq!(
                collect_batched(e, &s, &t, rng),
                want,
                "{ctx}: batched {s:?} {t:?}"
            );
            let mut rev = collect(e, &s, &t, true);
            rev.reverse();
            assert_eq!(rev, want, "{ctx}: reverse scan {s:?} {t:?}");
        }
        // Cut into parts: inside the range, ascending, as many as asked.
        for parts in [2, 5, 9] {
            let (s, t) = (
                key(rng.below(KEYS / 2)),
                key(KEYS / 2 + rng.below(KEYS / 2)),
            );
            let cuts = e.split_keys(&s, Some(&t), parts, 1).unwrap();
            assert!(cuts.len() < parts, "{ctx}: {} cuts", cuts.len());
            assert!(
                cuts.windows(2).all(|w| w[0] < w[1]),
                "{ctx}: cuts out of order"
            );
            assert!(
                cuts.iter().all(|c| *c > s && *c < t),
                "{ctx}: a cut outside"
            );
        }
        // Within one prefix, as a point read reads a user key's versions:
        // from its first, or from one of them, up to the next user key.
        for _ in 0..100 {
            let i = rng.below(KEYS);
            let p = &key(i)[..5];
            let s = if rng.below(2) == 0 {
                Bound::Included([p, &[0, 0]].concat())
            } else {
                Bound::Included(key(i))
            };
            let t = Bound::Excluded([p, &[0, 1]].concat());
            let want: Vec<(Vec<u8>, Vec<u8>)> = model
                .range::<[u8], _>((borrow(&s), borrow(&t)))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            assert_eq!(collect(e, &s, &t, false), want, "{ctx}: versions of {p:?}");
        }
    }

    /// Jobs' room in a test: names counted up, no budget.
    struct TestRoom<'a> {
        fs: &'a dyn FileSystem,
        dir: &'a Path,
        n: std::cell::Cell<u64>,
    }

    impl JobRoom for TestRoom<'_> {
        fn fs(&self) -> &dyn FileSystem {
            self.fs
        }
        fn dir(&self) -> &Path {
            self.dir
        }
        fn name(&self) -> Result<String> {
            self.n.set(self.n.get() + 1);
            Ok(format!("run-c{}", self.n.get()))
        }
        fn wrote(&self, _bytes: u64) -> bool {
            true
        }
    }

    /// Random puts, deletes and range deletes against a model, with
    /// checkpoints (set apart, saved, taken in — and sometimes a save
    /// skipped, as a failed checkpoint leaves it), reloads from the
    /// manifest, and forks: the engine reads as the model throughout, and
    /// compaction leaves several levels.
    #[test]
    fn reads_as_a_model_through_flushes_compactions_and_reloads() {
        let mut deep = 0;
        let mut spills = 0;
        for seed in 1..=8u64 {
            let p = SimPlatform::new(seed);
            let fs = p.fs();
            let dir = Path::new("/lsm");
            fs.create_dir_all(dir).unwrap();
            let rng = SplitMix64::new(seed);
            let mut e = LsmEngine::new();
            let mut model = Model::new();
            let mut lsn = 0;
            let room = TestRoom {
                fs,
                dir,
                n: Default::default(),
            };
            for step in 0..400 {
                match rng.below(10) {
                    0..=5 => {
                        let n = 1 + rng.below(20);
                        let mut ops: Vec<(Vec<u8>, Val)> = (0..n)
                            .map(|_| {
                                let k = key(rng.below(KEYS));
                                let v = (rng.below(4) != 0)
                                    .then(|| vec![rng.below(256) as u8; rng.below(200) as usize]);
                                (k, v)
                            })
                            .collect();
                        ops.sort_by(|a, b| a.0.cmp(&b.0));
                        ops.dedup_by(|a, b| a.0 == b.0);
                        for (k, v) in &ops {
                            match v {
                                Some(v) => model.insert(k.clone(), v.clone()),
                                None => model.remove(k),
                            };
                        }
                        e.apply(&ops).unwrap();
                        // A tail grown past SPILL written out, as a bulk
                        // load's writer does.
                        while let Some(j) = e.spill() {
                            let d = j(&room).unwrap().expect("a spill");
                            e.install(d).unwrap();
                            spills += 1;
                        }
                    }
                    6 => {
                        // Narrow, so the data grows and the levels deepen.
                        let a = rng.below(KEYS);
                        let (s, t) = (key(a), key(a + rng.below(40)));
                        let gone: Vec<Vec<u8>> = model
                            .range::<[u8], _>((Bound::Included(&s[..]), Bound::Excluded(&t[..])))
                            .map(|(k, _)| k.clone())
                            .collect();
                        for k in gone {
                            model.remove(&k);
                        }
                        e.remove_range(&s, &t).unwrap();
                    }
                    7 | 8 => {
                        lsn += 1;
                        e.prepare().unwrap();
                        check(
                            &e,
                            &model,
                            &rng,
                            &format!("seed {seed} step {step} set apart"),
                        );
                        // Sometimes a compaction planned before the
                        // save and taken in after it.
                        let early = (rng.below(3) == 0).then(|| e.job()).flatten();
                        if rng.below(8) != 0 {
                            let path = dir.join(format!("m-{lsn}"));
                            e.saver()(fs, dir, &path, &format!("0-1-{lsn}")).unwrap();
                            check(&e, &model, &rng, &format!("seed {seed} step {step} saved"));
                            if let Some(j) = early
                                && let Some(d) = j(&room).unwrap()
                            {
                                e.install(d).unwrap();
                            }
                            while rng.below(4) != 0 {
                                let Some(j) = e.job() else { break };
                                if let Some(d) = j(&room).unwrap() {
                                    e.install(d).unwrap();
                                }
                                check(
                                    &e,
                                    &model,
                                    &rng,
                                    &format!("seed {seed} step {step} compacted"),
                                );
                            }
                            let (tiers, bottom) = e.shape();
                            if bottom && tiers.len() >= 2 {
                                deep += 1;
                            }
                            if rng.below(3) == 0 {
                                e = LsmEngine::load(fs, dir, &path).unwrap();
                            }
                        }
                        // A bulk load: keys ascending from a random one,
                        // batch after batch (into the memtable's tail when
                        // above what it holds), spilled as it grows.
                        if rng.below(3) == 0 {
                            let mut i = rng.below(KEYS);
                            for _ in 0..4 {
                                let n = 1 + rng.below(30);
                                let ops: Vec<(Vec<u8>, Val)> = (i..(i + n).min(KEYS))
                                    .map(|j| (key(j), Some(vec![j as u8; 8 + (j % 32) as usize])))
                                    .collect();
                                i += n;
                                for (k, v) in &ops {
                                    model.insert(k.clone(), v.clone().unwrap());
                                }
                                e.apply(&ops).unwrap();
                                while let Some(j) = e.spill() {
                                    let d = j(&room).unwrap().expect("a spill");
                                    e.install(d).unwrap();
                                    spills += 1;
                                }
                            }
                            check(&e, &model, &rng, &format!("seed {seed} step {step} load"));
                            while let Some(j) = e.job() {
                                if let Some(d) = j(&room).unwrap() {
                                    e.install(d).unwrap();
                                }
                            }
                        }
                    }
                    _ => {
                        let f = e.fork();
                        drop(f);
                    }
                }
                check(&e, &model, &rng, &format!("seed {seed} step {step}"));
            }
            assert!(!e.files().is_empty(), "seed {seed}: no runs");
        }
        // Compactions reached the last level with two tiers above it.
        assert!(deep > 0, "never a last level with two tiers above");
        assert!(spills > 0, "never a spill");
        let batches = BATCHES.load(std::sync::atomic::Ordering::Relaxed);
        assert!(batches > 0, "never a scan a batch at a time");
    }

    /// The block cache keeps within its bytes, finds what it holds, and
    /// gives a block used since the hand passed a second chance.
    #[test]
    fn the_block_cache_keeps_within_its_bytes() {
        let c = BlockCache::new();
        c.capacity
            .store(SHARDS * 10_000, std::sync::atomic::Ordering::Relaxed);
        let block = |n: usize| {
            let mut data = Vec::new();
            put_bytes(&mut data, &vec![b'k'; n]);
            data.push(1);
            Arc::new(Decoded::new(data).unwrap())
        };
        // One run's blocks: the shards they fall in each keep within
        // 10 000 bytes.
        for i in 0..2000u32 {
            c.put((1, i), block(900));
            if i % 3 == 0 {
                // Used: kept over the others as the hand passes.
                assert!(c.get((1, i)).is_some());
            }
        }
        for s in &c.shards {
            let s = s.lock().unwrap();
            assert!(s.bytes <= 10_000, "a shard holds {}", s.bytes);
            assert_eq!(s.map.as_ref().map_or(0, |m| m.len()), s.slots.len());
            for (i, slot) in s.slots.iter().enumerate() {
                assert_eq!(s.map.as_ref().unwrap()[&slot.key], i);
            }
        }
        let held = (0..2000u32).filter(|&i| c.get((1, i)).is_some()).count();
        assert!(held > 100 && held < 200, "{held} held");
        let d = c.get((1, 1999)).expect("the last put is held");
        assert_eq!(d.key(0), &[b'k'; 900][..]);
        assert_eq!(d.value(0), None);
    }

    /// The filter: no false negatives, about 1 % false positives.
    #[test]
    fn the_filter_has_no_false_negatives_and_few_false_positives() {
        let keys: Vec<Vec<u8>> = (0..10_000u64)
            .map(|i| format!("key {i}").into_bytes())
            .collect();
        let hashes: Vec<u64> = keys.iter().map(|k| hash64(k)).collect();
        let b = Bloom::with_hashes(&hashes);
        assert!(hashes.iter().all(|&h| b.may(h)));
        let fp = (0..10_000u64)
            .filter(|i| b.may(hash64(format!("other {i}").as_bytes())))
            .count();
        assert!(fp < 300, "{fp} false positives in 10,000");
    }

    /// A run's damaged block or index is refused, not read wrong.
    #[test]
    fn a_damaged_run_is_refused() {
        let p = SimPlatform::new(5);
        let fs = p.fs();
        let dir = Path::new("/lsm");
        fs.create_dir_all(dir).unwrap();
        let mut w = RunWriter::new(fs, dir, "r").unwrap();
        for i in 0..2000 {
            w.add(&key(i), &Some(format!("value {i}").into_bytes()))
                .unwrap();
        }
        let r = w.finish(Ranges::default()).unwrap().unwrap();
        assert!(r.index.len() > 2);
        assert_eq!(
            r.lookup(&key(1500), hash64(&key(1500))).unwrap(),
            Some(Some(b"value 1500".to_vec()))
        );
        let f = fs.open(&dir.join("r"), OpenMode::ReadWrite).unwrap();
        f.write_at(b"\xff", r.index[1].offset + 7).unwrap();
        let at = r.index[1].first.clone();
        assert!(r.lookup(&at, hash64(&at)).is_err());
        let size = f.size().unwrap();
        f.write_at(b"\xff", size - FOOTER as u64 - 6).unwrap();
        assert!(Run::open(fs, dir, "r").is_err());
    }

    /// A key's prefix ends at its first `00 00`.
    #[test]
    fn prefixes_end_at_the_terminator() {
        assert_eq!(prefix(b"ab\0\0\x07\0\0"), b"ab\0\0");
        assert_eq!(prefix(b"a\0\xffb\0\0x"), b"a\0\xffb\0\0");
        assert_eq!(prefix(b"abc"), b"abc");
    }

    #[test]
    fn ranges_join_and_cover() {
        let mut r = Ranges::default();
        r.add(b"c", b"e");
        r.add(b"a", b"b");
        r.add(b"d", b"g");
        r.add(b"b", b"c");
        assert_eq!(r.0, vec![(b"a".to_vec(), b"g".to_vec())]);
        r.add(b"x", b"z");
        assert!(
            r.covers(b"a")
                && r.covers(b"f")
                && !r.covers(b"g")
                && r.covers(b"y")
                && !r.covers(b"z")
        );
    }
}
