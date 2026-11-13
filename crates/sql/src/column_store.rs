//! Columnar storage (PLAN.md §M14.20 V7): a table's rows as column
//! segments, for scans that read few columns of many rows.
//!
//! **Segments.**  A segment holds up to [`SEGMENT_ROWS`] rows with
//! consecutive row keys, one KV value per column ([`crate::segment`]'s
//! encodings) and one for the row keys.  Segments cover disjoint key ranges
//! in key order, listed in a directory keyed by each segment's last key
//! (with its first key, row count and each column's least and greatest
//! values, which let a scan skip a segment its filter rules out).  A
//! segment's id is a hash of its contents, so it never changes: decoded
//! columns are cached by it ([`cached`]).
//!
//! **The delta store.**  Writes go to the row storage's format under the
//! same row keys; an update or delete of a row in a segment marks it deleted
//! (a key per row: concurrent deletes do not conflict) and an update writes
//! the new row to the delta store.  After a statement wrote enough rows,
//! the delta store is merged into segments in the same transaction —
//! segments it overlaps rewritten, deleted rows dropped.
//!
//! **MVCC and crashes** come from the KV contract: segments, markers and
//! delta rows are versioned keys, written in their transactions.  A marker
//! read validates its segment's directory entry, and a merge validates the
//! markers it read, so a delete and a merge of the same segment conflict
//! instead of losing the delete.
//!
//! Key spaces under the table's data prefix: the row storage's (the delta
//! store), [`SEGMENTS`] (chunks: segment id, column), [`DELETED`]
//! (markers: segment id, row), [`DIRECTORY`] (entries: last row key).

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::ops::Bound;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, OnceLock, RwLock};

use nucleus_kv::{KvTxn, prefix_end};

use crate::catalog::{INDEX_PREFIX_LEN, Table};
use crate::encoding::{Reader, Writer, decode_value};
use crate::enums::Labels;
use crate::error::{Error, Result};
use crate::storage::{BatchSource, ColumnBound, NewRow, RowIter, RowStorage, Scan, TableStorage};
use crate::types::{Row, Value};
use crate::vector::{Batch, Sel, Vector};

/// The storage's name.
pub const COLUMN: &str = "column";
/// Rows a segment holds at most.
pub const SEGMENT_ROWS: usize = 32 * 1024;
/// A segment's columns' encoded bytes, about, at most (wide rows make
/// shorter segments).
const SEGMENT_BYTES: usize = 32 << 20;
/// Rows written to the delta store by one statement that start a merge.
pub const MERGE_ROWS: usize = 4096;
/// The index id of segments' columns.
pub const SEGMENTS: u32 = u32::MAX - 1;
/// The index id of rows of segments deleted.
pub const DELETED: u32 = u32::MAX - 2;
/// The index id of the directory of segments.
pub const DIRECTORY: u32 = u32::MAX - 3;
/// The index id of a `column` table's delta store (rows in the row
/// storage's format, keyed by their primary key suffixes).
pub const DELTA: u32 = u32::MAX - 4;
/// The index id of a `row, column` table's changed keys: rows written since
/// the column copy last took them in (their versions in the row copy).
pub const CHANGED: u32 = u32::MAX - 5;
/// The column number of a segment's row keys.
const KEYS: u16 = u16::MAX;

/// Which copy of a `row, column` table is being built (by a storage
/// conversion, `ALTER TABLE … SET STORAGE`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CopyState {
    /// Both are whole.
    #[default]
    Ready,
    /// The column copy: the rows are read from the row copy.
    BuildingColumns,
    /// The row copy: the rows are read from the column copy.
    BuildingRows,
    /// A `row` table's column copy, left over, being removed.
    DroppingColumns,
    /// A `column` table's row copy, left over, being removed.
    DroppingRows,
    /// A `row, column` table whose rows were in a `column` table's delta
    /// store: writes go to the row copy (and the changed keys), reads see
    /// the delta store under the changed keys, until it is moved.
    MovingFromDelta,
    /// A `column` table whose rows were among a `row, column` table's
    /// changed keys: writes go to the delta store, reads see the changed
    /// keys (their rows in the row copy) under it, until they are moved.
    MovingFromChanged,
}

/// The index ids of a column copy.
pub const COPY_INDEXES: [u32; 4] = [SEGMENTS, DELETED, DIRECTORY, CHANGED];

impl CopyState {
    /// Its tag in a table's record.
    pub fn tag(self) -> u8 {
        match self {
            CopyState::Ready => 0,
            CopyState::BuildingColumns => 1,
            CopyState::BuildingRows => 2,
            CopyState::DroppingColumns => 3,
            CopyState::DroppingRows => 4,
            CopyState::MovingFromDelta => 5,
            CopyState::MovingFromChanged => 6,
        }
    }

    /// The state of a tag.
    pub fn of_tag(t: u8) -> Result<Self> {
        Ok(match t {
            0 => CopyState::Ready,
            1 => CopyState::BuildingColumns,
            2 => CopyState::BuildingRows,
            3 => CopyState::DroppingColumns,
            4 => CopyState::DroppingRows,
            5 => CopyState::MovingFromDelta,
            6 => CopyState::MovingFromChanged,
            _ => return Err(Error::internal("an unknown copy state")),
        })
    }
}

/// Where the rows not in segments are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Delta {
    /// A `column` table's delta store ([`DELTA`]).
    Own,
    /// A `row, column` table's changed keys ([`CHANGED`]), the rows in its
    /// row copy (a key without one: a row deleted).
    Changed,
    /// Both while one moves into the other: the second wins a key both
    /// have (`Both(Own, Changed)`: the changed keys are newer).
    Both(Single, Single),
}

/// One delta, of a [`Delta::Both`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Single {
    Own,
    Changed,
}

impl Single {
    fn delta(self) -> Delta {
        match self {
            Single::Own => Delta::Own,
            Single::Changed => Delta::Changed,
        }
    }
}

/// The delta of a table of storage `storage` in state `copy`.
fn delta_of(t: &Table) -> Delta {
    match (t.storage.as_str(), t.copy) {
        (COLUMN, CopyState::MovingFromChanged) => Delta::Both(Single::Changed, Single::Own),
        (COLUMN, _) => Delta::Own,
        (_, CopyState::MovingFromDelta) => Delta::Both(Single::Own, Single::Changed),
        _ => Delta::Changed,
    }
}

/// `key` (a row key, or a bound of a range of them) moved to index `index`.
fn moved(t: &Table, key: &[u8], index: u32) -> Vec<u8> {
    let from = t.index_prefix(0);
    let mut k = t.index_prefix(index);
    if key.starts_with(&from) {
        k.extend_from_slice(&key[from.len()..]);
        k
    } else if key > from.as_slice() {
        prefix_end(&k).expect("an index prefix")
    } else {
        k
    }
}

/// The row key of a key of another index of `t` (the same suffix).
fn back(t: &Table, key: &[u8]) -> Vec<u8> {
    let mut k = t.index_prefix(0);
    k.extend_from_slice(&key[k.len()..]);
    k
}

fn delta_index(d: Delta) -> u32 {
    match d {
        Delta::Own => DELTA,
        Delta::Changed => CHANGED,
        Delta::Both(_, newer) => delta_index(newer.delta()),
    }
}

/// The rows not in segments in `[start, end)` (row keys), ascending: a row,
/// or `None` for a key whose row was deleted.
fn delta_items<'a>(
    txn: &'a dyn KvTxn,
    t: &'a Table,
    start: &[u8],
    end: Option<&[u8]>,
    wanted: Option<&'a [bool]>,
    validated: bool,
    delta: Delta,
) -> Result<Box<dyn Iterator<Item = Result<(Vec<u8>, Option<Row>)>> + 'a>> {
    if let Delta::Both(older, newer) = delta {
        // Both, by key; the newer's item where both have one.
        let mut a = delta_items(txn, t, start, end, wanted, validated, older.delta())?.peekable();
        let mut b = delta_items(txn, t, start, end, wanted, validated, newer.delta())?.peekable();
        return Ok(Box::new(std::iter::from_fn(move || {
            let ka = match a.peek() {
                Some(Ok((k, _))) => Some(k.clone()),
                Some(Err(_)) => return a.next(),
                None => None,
            };
            let kb = match b.peek() {
                Some(Ok((k, _))) => Some(k.clone()),
                Some(Err(_)) => return b.next(),
                None => None,
            };
            match (ka, kb) {
                (None, None) => None,
                (Some(_), None) => a.next(),
                (None, Some(_)) => b.next(),
                (Some(x), Some(y)) if x < y => a.next(),
                (Some(x), Some(y)) if x > y => b.next(),
                _ => {
                    a.next();
                    b.next()
                }
            }
        })));
    }
    let index = delta_index(delta);
    let from = moved(t, start, index);
    let to = match end {
        Some(e) => moved(t, e, index),
        None => prefix_end(&t.index_prefix(index)).expect("an index prefix"),
    };
    Ok(match delta {
        Delta::Own => Box::new(
            RowStorage
                .rows(Scan {
                    txn,
                    table: t,
                    start: from,
                    end: Some(to),
                    wanted,
                    validated,
                    bounds: Vec::new(),
                    dicts: Vec::new(),
                })?
                .map(move |r| r.map(|(k, row)| (back(t, &k), Some(row)))),
        ),
        Delta::Both(..) => unreachable!("taken above"),
        Delta::Changed => {
            let keys = if validated {
                txn.scan_validated(Bound::Included(&from), Bound::Excluded(&to))?
            } else {
                txn.scan(Bound::Included(&from), Bound::Excluded(&to))?
            };
            let mut labels = Labels::default();
            Box::new(keys.map(move |e| {
                let (k, _) = e?;
                let key = back(t, &k);
                let row = RowStorage.get(txn, t, &key, wanted, &mut labels)?;
                Ok((key, row))
            }))
        }
    })
}

fn bad(what: &str) -> Error {
    Error::internal(format!("a malformed column segment: {what}"))
}

// ------------------------------------------------------------------- keys

fn chunk_key(t: &Table, seg: u64, col: u16) -> Vec<u8> {
    let mut k = t.index_prefix(SEGMENTS);
    k.extend_from_slice(&seg.to_be_bytes());
    k.extend_from_slice(&col.to_be_bytes());
    k
}

/// The marker of the segment row under row key `key`: the row deleted.
/// Keyed by the row, not by its place in a segment, a marker stays right
/// when a merge rewrites the segment — so a delete and a merge do not
/// conflict.
fn marker_key(t: &Table, key: &[u8]) -> Vec<u8> {
    moved(t, key, DELETED)
}

fn dir_key(t: &Table, last: &[u8]) -> Vec<u8> {
    let mut k = t.index_prefix(DIRECTORY);
    k.extend_from_slice(last);
    k
}

// -------------------------------------------------------------- directory

/// A segment, as the directory lists it.
#[derive(Clone, Debug, PartialEq)]
struct Segment {
    id: u64,
    /// Its first and last row keys.
    first: Vec<u8>,
    last: Vec<u8>,
    rows: usize,
    /// The table's columns it holds.
    cols: Vec<u16>,
    /// Per column held: least, greatest, `NULL`s.
    stats: Vec<(Value, Value, u64)>,
}

impl Segment {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u64(self.id).u32(self.first.len() as u32);
        w.0.extend_from_slice(&self.first);
        w.u32(self.rows as u32).u32(self.stats.len() as u32);
        for (lo, hi, nulls) in &self.stats {
            w.value(lo).value(hi).u64(*nulls);
        }
        for &c in &self.cols {
            w.u16(c);
        }
        w.0
    }

    fn decode(last: &[u8], b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let e = |_| bad("directory entry");
        let id = r.u64().map_err(e)?;
        let n = r.u32().map_err(e)? as usize;
        let first = r.take(n).map_err(e)?.to_vec();
        let rows = r.u32().map_err(e)? as usize;
        let cols = r.u32().map_err(e)? as usize;
        let mut stats = Vec::with_capacity(cols);
        for _ in 0..cols {
            let lo = decode_value(&mut r)?;
            let hi = decode_value(&mut r)?;
            stats.push((lo, hi, r.u64().map_err(e)?));
        }
        let cols = if r.is_empty() {
            (0..cols as u16).collect()
        } else {
            (0..cols)
                .map(|_| r.u16().map_err(e))
                .collect::<Result<_>>()?
        };
        Ok(Segment {
            id,
            first,
            last: last.to_vec(),
            rows,
            cols,
            stats,
        })
    }

    /// Whether `bounds` rule out every row of it.
    fn excluded(&self, bounds: &[ColumnBound]) -> bool {
        bounds.iter().any(|b| {
            let Some(at) = self.cols.iter().position(|&c| usize::from(c) == b.column) else {
                return false;
            };
            let (lo, hi, nulls) = &self.stats[at];
            if lo.is_null() {
                return *nulls > 0; // only NULLs: no bound holds
            }
            // Compared only with a value of the column's own kind.
            let same = |v: &Value| std::mem::discriminant(v) == std::mem::discriminant(lo);
            let kinds = match (&b.lower, &b.upper) {
                (Bound::Included(v) | Bound::Excluded(v), _)
                | (_, Bound::Included(v) | Bound::Excluded(v)) => same(v),
                _ => false,
            };
            if !kinds {
                return false;
            }
            let below = match &b.lower {
                Bound::Included(v) => hi.sort_cmp(v).is_lt(),
                Bound::Excluded(v) => hi.sort_cmp(v).is_le(),
                Bound::Unbounded => false,
            };
            let above = match &b.upper {
                Bound::Included(v) => lo.sort_cmp(v).is_gt(),
                Bound::Excluded(v) => lo.sort_cmp(v).is_ge(),
                Bound::Unbounded => false,
            };
            below || above
        })
    }
}

/// The segments with rows in `[start, end)`, in key order.
fn segments(txn: &dyn KvTxn, t: &Table, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Segment>> {
    let p = t.index_prefix(DIRECTORY);
    let from = dir_key(t, start);
    let to = prefix_end(&p).expect("directory prefix");
    let mut out = Vec::new();
    for e in txn.scan(Bound::Included(&from), Bound::Excluded(&to))? {
        let (k, v) = e?;
        let seg = Segment::decode(&k[p.len()..], &v)?;
        if end.is_some_and(|e| seg.first.as_slice() >= e) {
            break;
        }
        out.push(seg);
    }
    Ok(out)
}

/// The segments whose key ranges meet `[lo, hi]`, in key order.  Read
/// `validated`, the directory is claimed for the keys a write stores
/// there: through the first entry past `hi` (to the directory's end
/// without one), so a segment another transaction commits over any of
/// these keys meanwhile — its entry inside the range read, or one it
/// rewrote — fails this transaction's commit.  Entries are keyed by a
/// segment's last key, so two segments over the same keys need not share
/// one; this read is what makes their writes conflict.
fn segments_through(
    txn: &dyn KvTxn,
    t: &Table,
    lo: &[u8],
    hi: &[u8],
    validated: bool,
) -> Result<Vec<Segment>> {
    let p = t.index_prefix(DIRECTORY);
    let from = dir_key(t, lo);
    let to = prefix_end(&p).expect("directory prefix");
    let scan = if validated {
        txn.scan_validated(Bound::Included(&from), Bound::Excluded(&to))?
    } else {
        txn.scan(Bound::Included(&from), Bound::Excluded(&to))?
    };
    let mut out = Vec::new();
    for e in scan {
        let (k, v) = e?;
        let seg = Segment::decode(&k[p.len()..], &v)?;
        if seg.first.as_slice() > hi {
            break; // read: the claim ends at this entry
        }
        out.push(seg);
    }
    Ok(out)
}

/// Claims the delta store's keys in `[lo, hi]` for a merge that moves
/// rows there into segments: a row another transaction commits into the
/// delta store there meanwhile fails this one's commit (it would be a
/// second row under a key the merge stores, or one the merge missed).
fn claim_delta(txn: &dyn KvTxn, t: &Table, lo: &[u8], hi: &[u8]) -> Result<()> {
    let (from, to) = (moved(t, lo, DELTA), moved(t, hi, DELTA));
    for e in txn.scan_validated(Bound::Included(&from), Bound::Included(&to))? {
        e?;
    }
    Ok(())
}

/// Claims the directory for `rows` (keyed, ascending) a statement stores
/// in the delta store ([`segments_through`]): a merge committed over
/// their keys meanwhile conflicts with them.
fn claim_for_delta(
    txn: &dyn KvTxn,
    t: &Table,
    rows: &std::collections::BTreeMap<Vec<u8>, Row>,
) -> Result<()> {
    if let (Some((lo, _)), Some((hi, _))) = (rows.first_key_value(), rows.last_key_value()) {
        segments_through(txn, t, lo, hi, true)?;
    }
    Ok(())
}

/// The segment whose key range holds `key`, if any.
fn segment_of(txn: &dyn KvTxn, t: &Table, key: &[u8]) -> Result<Option<Arc<Segment>>> {
    let p = t.index_prefix(DIRECTORY);
    let from = dir_key(t, key);
    let to = prefix_end(&p).expect("directory prefix");
    // Just the first entry at or after the key: no read-ahead.
    let Some((k, v)) = txn.first(Bound::Included(&from), Bound::Excluded(&to))? else {
        return Ok(None);
    };
    let seg = entry(&k[p.len()..], &v)?;
    Ok((seg.first.as_slice() <= key).then_some(seg))
}

// ------------------------------------------------------------------ cache

/// A segment's row keys (suffixes after the table's primary index prefix).
#[derive(Debug)]
struct Keys {
    offsets: Vec<u32>,
    bytes: Vec<u8>,
    /// Every [`MARK`]th key's first 8 bytes, a big-endian number: a search
    /// narrows to a run of keys in this small array first, where each
    /// step of a search over the keys themselves is a miss or two in
    /// memory (`offsets`, then `bytes`) — ClickHouse's marks of its
    /// primary key, a B+-tree's inner level.
    marks: Vec<u64>,
}

/// Keys between two marks ([`Keys::marks`]).
const MARK: usize = 32;

/// The first 8 bytes of `key` as a big-endian number (zeros past its
/// end): `a < b` gives `prefix(a) <= prefix(b)`.
fn prefix(key: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    let n = key.len().min(8);
    b[..n].copy_from_slice(&key[..n]);
    u64::from_be_bytes(b)
}

impl Keys {
    fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    fn get(&self, i: usize) -> &[u8] {
        &self.bytes[self.offsets[i] as usize..self.offsets[i + 1] as usize]
    }

    /// The first row whose key is `>= suffix`: the marks first — a mark
    /// whose prefix is below `suffix`'s is a key below it, one whose
    /// prefix is above a key above it —, then the keys between.
    fn search(&self, suffix: &[u8]) -> usize {
        let p = prefix(suffix);
        let below = self.marks.partition_point(|&m| m < p);
        let not_above = below + self.marks[below..].partition_point(|&m| m == p);
        let mut lo = if below == 0 {
            0
        } else {
            (below - 1) * MARK + 1
        };
        let mut hi = (not_above * MARK).min(self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.get(mid) < suffix {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    fn encode(suffixes: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(suffixes.len() as u32).to_le_bytes());
        let mut at = 0u32;
        out.extend_from_slice(&at.to_le_bytes());
        for s in suffixes {
            at += s.len() as u32;
            out.extend_from_slice(&at.to_le_bytes());
        }
        for s in suffixes {
            out.extend_from_slice(s);
        }
        out
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let e = |_| bad("row keys");
        let n = r.u32().map_err(e)? as usize;
        let mut offsets = Vec::with_capacity(n + 1);
        for _ in 0..=n {
            offsets.push(r.u32().map_err(e)?);
        }
        let bytes = r.take(r.remaining()).map_err(e)?.to_vec();
        if offsets.windows(2).any(|w| w[0] > w[1])
            || offsets.last().is_some_and(|&l| l as usize != bytes.len())
        {
            return Err(bad("row key offsets"));
        }
        let mut keys = Keys {
            offsets,
            bytes,
            marks: Vec::new(),
        };
        keys.marks = (0..keys.len())
            .step_by(MARK)
            .map(|i| prefix(keys.get(i)))
            .collect();
        Ok(keys)
    }
}

/// A decoded column, cached.
#[derive(Clone)]
enum Decoded {
    Keys(Arc<Keys>),
    Column(Arc<Vector>),
    /// A column's encoding as stored (decompressed), for point reads of
    /// one value ([`crate::segment::value_at`]): smaller than the column
    /// decoded.
    Raw(Arc<Vec<u8>>),
    /// A segment's directory entry, decoded: a lookup's, the same for the
    /// segment's id as long as it lives (a merge writes new segments).
    Entry(Arc<Segment>),
}

/// The cache key of a segment's directory entry (its column number: no
/// table has this many columns, nor this many less [`RAW`]).
const ENTRY: u16 = u16::MAX - 1;

/// The directory entry `v` (its key's suffix `last`), decoded once for
/// its segment and kept in the cache.
fn entry(last: &[u8], v: &[u8]) -> Result<Arc<Segment>> {
    let id = Reader::new(v).u64().map_err(|_| bad("directory entry"))?;
    if let Some(Decoded::Entry(seg)) = cache_get(&(id, ENTRY)) {
        return Ok(seg);
    }
    let seg = Arc::new(Segment::decode(last, v)?);
    cache_insert((id, ENTRY), Decoded::Entry(Arc::clone(&seg)));
    Ok(seg)
}

/// The cache key of a column's encoding as stored (beside its decoded
/// form's, the column itself).
const RAW: u16 = 0x8000;

/// Decoded columns by (segment id, column): a segment never changes, so
/// they stay right.  Bounded by their size; past it, those used longest
/// ago go — an eighth of the bound at a time, so a table larger than the
/// cache keeps what its reads come back to (point reads by an index), and
/// a scan through it evicts in steps rather than all at once.
#[derive(Default)]
struct Cache {
    entries: HashMap<(u64, u16), (Decoded, usize, AtomicU64)>,
}

/// The cache's shards: a column's by its key, so threads reading different
/// columns (a parallel join's lookups) take different locks.
const CACHE_SHARDS: usize = 16;

fn shard_at(i: usize) -> &'static RwLock<Cache> {
    static CACHE: OnceLock<Vec<RwLock<Cache>>> = OnceLock::new();
    let shards = CACHE.get_or_init(|| {
        (0..CACHE_SHARDS)
            .map(|_| RwLock::new(Cache::default()))
            .collect()
    });
    &shards[i]
}

fn cache(key: &(u64, u16)) -> &'static RwLock<Cache> {
    let h = (key.0 ^ (u64::from(key.1) << 48)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    shard_at((h >> 32) as usize % CACHE_SHARDS)
}

/// A cached column (a hit only reads its shard).
fn cache_get(key: &(u64, u16)) -> Option<Decoded> {
    cache(key)
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
}

/// Caches `d` under `key`: past the bound (all shards' together), the
/// columns used longest ago in any shard go first, until an eighth of the
/// bound is free — one shard's lock held at a time.
fn cache_insert(key: (u64, u16), d: Decoded) {
    let size = decoded_bytes(&d);
    if CACHE_TOTAL.load(Relaxed) + size > CACHE_BYTES {
        evict(size);
    }
    cache(&key)
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, d, size);
}

/// Drops the columns used longest ago, across the shards, until `room`
/// more bytes fit an eighth below the bound.
fn evict(room: usize) {
    let mut by_use: Vec<((u64, u16), u64)> = Vec::new();
    for shard in 0..CACHE_SHARDS {
        let c = shard_at(shard).read().unwrap_or_else(|e| e.into_inner());
        by_use.extend(
            c.entries
                .iter()
                .map(|(k, (_, _, used))| (*k, used.load(Relaxed))),
        );
    }
    by_use.sort_unstable_by_key(|e| e.1);
    for (k, _) in by_use {
        if CACHE_TOTAL.load(Relaxed) + room <= CACHE_BYTES - CACHE_BYTES / 8 {
            break;
        }
        cache(&k)
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&k);
    }
}

/// The decoded columns' bound, in bytes (all shards' together).
const CACHE_BYTES: usize = 256 << 20;

/// The bytes the shards hold.
static CACHE_TOTAL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// When a cached column was last used: a clock that moves on with each
/// column cached (an approximate order of use is enough to evict by).
static CACHE_CLOCK: AtomicU64 = AtomicU64::new(0);

/// About the memory a decoded column holds.
fn decoded_bytes(d: &Decoded) -> usize {
    let nulls = |n: &Option<Vec<bool>>| n.as_ref().map_or(0, Vec::len);
    64 + match d {
        Decoded::Keys(k) => k.offsets.len() * 4 + k.bytes.len() + k.marks.len() * 8,
        Decoded::Entry(e) => e.first.len() + e.last.len() + e.cols.len() * 2 + e.stats.len() * 80,
        Decoded::Raw(b) => b.len(),
        Decoded::Column(v) => match &**v {
            Vector::Const(_) => 32,
            Vector::Int(d, n) | Vector::Time(_, d, n) => d.len() * 8 + nulls(n),
            Vector::Float(d, n) => d.len() * 8 + nulls(n),
            Vector::Dec(_, crate::vector::Mantissas::Narrow(d), n) => d.len() * 8 + nulls(n),
            Vector::Dec(_, crate::vector::Mantissas::Wide(d), n) => d.len() * 16 + nulls(n),
            Vector::Bool(d, n) => d.len() + nulls(n),
            Vector::Text(o, b, n) | Vector::Char(o, b, n) => o.len() * 4 + b.len() + nulls(n),
            Vector::Values(v) => v.len() * std::mem::size_of::<Value>(),
            Vector::Dict(d, c, n) => match &**d {
                Vector::Text(o, b, _) | Vector::Char(o, b, _) => {
                    c.len() * 4 + nulls(n) + o.len() * 4 + b.len()
                }
                _ => c.len() * 4 + nulls(n),
            },
        },
    }
}

impl Cache {
    fn get(&self, key: &(u64, u16)) -> Option<Decoded> {
        self.entries.get(key).map(|(d, _, used)| {
            used.store(CACHE_CLOCK.load(Relaxed), Relaxed);
            d.clone()
        })
    }

    fn insert(&mut self, key: (u64, u16), d: Decoded, size: usize) {
        let tick = CACHE_CLOCK.fetch_add(1, Relaxed) + 1;
        CACHE_TOTAL.fetch_add(size, Relaxed);
        if let Some((_, old, _)) = self.entries.insert(key, (d, size, AtomicU64::new(tick))) {
            CACHE_TOTAL.fetch_sub(old, Relaxed);
        }
    }

    fn remove(&mut self, key: &(u64, u16)) {
        if let Some((_, size, _)) = self.entries.remove(key) {
            CACHE_TOTAL.fetch_sub(size, Relaxed);
        }
    }
}

fn cached(txn: &dyn KvTxn, t: &Table, seg: &Segment, col: u16) -> Result<Decoded> {
    if let Some(d) = cache_get(&(seg.id, col)) {
        return Ok(d);
    }
    let b = txn
        .get(&chunk_key(t, seg.id, col))?
        .ok_or_else(|| bad("a column is missing"))?;
    let d = if col == KEYS {
        Decoded::Keys(Arc::new(Keys::decode(&b)?))
    } else {
        Decoded::Column(Arc::new(crate::segment::decode(&b)?))
    };
    cache_insert((seg.id, col), d.clone());
    Ok(d)
}

fn keys_of(txn: &dyn KvTxn, t: &Table, seg: &Segment) -> Result<Arc<Keys>> {
    match cached(txn, t, seg, KEYS)? {
        Decoded::Keys(k) if k.len() == seg.rows => Ok(k),
        _ => Err(bad("row keys")),
    }
}

/// Row `i` of column `col` of `seg`: from the column decoded if it is
/// cached, else read from its stored encoding where that allows (the
/// encoding cached, not the column decoded — a point read's way), else
/// from the column decoded.
/// An enum's value given its label.
fn value_of(txn: &dyn KvTxn, t: &Table, seg: &Segment, col: usize, i: usize) -> Result<Value> {
    let mut v = stored_value(txn, t, seg, col, i)?;
    Labels::default().fill(txn, t.columns[col].ty, &mut v)?;
    Ok(v)
}

/// [`value_of`] as stored (an enum's value without its label).
fn stored_value(txn: &dyn KvTxn, t: &Table, seg: &Segment, col: usize, i: usize) -> Result<Value> {
    if !seg.cols.contains(&(col as u16)) {
        return Ok(Value::Null);
    }
    let decoded = cache_get(&(seg.id, col as u16));
    if let Some(Decoded::Column(v)) = decoded {
        return Ok(v.value(i));
    }
    let key = (seg.id, col as u16 | RAW);
    let raw = cache_get(&key);
    let raw = match raw {
        Some(Decoded::Raw(b)) => b,
        _ => {
            let b = Arc::new(
                txn.get(&chunk_key(t, seg.id, col as u16))?
                    .ok_or_else(|| bad("a column is missing"))?,
            );
            cache_insert(key, Decoded::Raw(Arc::clone(&b)));
            b
        }
    };
    match crate::segment::value_at(&raw, i)? {
        Some(v) => Ok(v),
        None => Ok(stored_column(txn, t, seg, col)?.value(i)),
    }
}

/// Column `col` of `seg`, decoded — an enum's values given their labels:
/// the cache holds them without, as stored, for the labels are the
/// catalog's as the reading transaction sees it (`ALTER TYPE … RENAME
/// VALUE` changes them).
fn column_of(txn: &dyn KvTxn, t: &Table, seg: &Segment, col: usize) -> Result<Arc<Vector>> {
    let v = stored_column(txn, t, seg, col)?;
    let ty = t.columns[col].ty;
    if !matches!(ty, crate::types::DataType::Enum(_)) {
        return Ok(v);
    }
    let mut labels = Labels::default();
    let mut v = Arc::unwrap_or_clone(v);
    match &mut v {
        Vector::Values(vals) => {
            for x in vals {
                labels.fill(txn, ty, x)?;
            }
        }
        Vector::Const(x) => labels.fill(txn, ty, x)?,
        _ => {}
    }
    Ok(Arc::new(v))
}

/// [`column_of`] as stored (an enum's values without their labels).
fn stored_column(txn: &dyn KvTxn, t: &Table, seg: &Segment, col: usize) -> Result<Arc<Vector>> {
    if !seg.cols.contains(&(col as u16)) {
        // A column added after the segment was written: NULL.
        return Ok(Arc::new(Vector::Const(Value::Null)));
    }
    match cached(txn, t, seg, col as u16)? {
        Decoded::Column(v) => Ok(v),
        Decoded::Keys(_) | Decoded::Raw(_) | Decoded::Entry(_) => Err(bad("a column")),
    }
}

/// The rows of `seg` marked deleted, by place.
fn deleted(txn: &dyn KvTxn, t: &Table, seg: &Segment, validated: bool) -> Result<BTreeSet<u32>> {
    let from = marker_key(t, &seg.first);
    let mut to = marker_key(t, &seg.last);
    to.push(0);
    let scan = if validated {
        txn.scan_validated(Bound::Included(&from), Bound::Excluded(&to))?
    } else {
        txn.scan(Bound::Included(&from), Bound::Excluded(&to))?
    };
    let keys = keys_of(txn, t, seg)?;
    let prefix = crate::catalog::INDEX_PREFIX_LEN;
    let mut out = BTreeSet::new();
    for e in scan {
        let (k, _) = e?;
        let i = keys.search(&k[prefix..]);
        if i < keys.len() && keys.get(i) == &k[prefix..] {
            out.insert(i as u32);
        }
    }
    Ok(out)
}

// ----------------------------------------------------------------- lookups

/// Where the live row under `key` is in a segment: the segment and row.
fn locate(
    txn: &dyn KvTxn,
    t: &Table,
    key: &[u8],
    validated: bool,
) -> Result<Option<(Arc<Segment>, usize)>> {
    match segment_of(txn, t, key)? {
        None => Ok(None),
        Some(seg) => locate_in(txn, t, key, validated, seg),
    }
}

/// [`locate`] in `seg`, the segment whose range holds `key`.
fn locate_in(
    txn: &dyn KvTxn,
    t: &Table,
    key: &[u8],
    validated: bool,
    seg: Arc<Segment>,
) -> Result<Option<(Arc<Segment>, usize)>> {
    let prefix = crate::catalog::INDEX_PREFIX_LEN;
    let keys = keys_of(txn, t, &seg)?;
    let i = keys.search(&key[prefix..]);
    if i == keys.len() || keys.get(i) != &key[prefix..] {
        return Ok(None);
    }
    let marker = marker_key(t, key);
    let gone = if validated {
        txn.get_validated(&marker)?
    } else {
        txn.get(&marker)?
    };
    Ok(gone.is_none().then_some((seg, i)))
}

/// Row `i` of `seg`, the `wanted` columns decoded.
fn segment_row(
    txn: &dyn KvTxn,
    t: &Table,
    seg: &Segment,
    i: usize,
    wanted: Option<&[bool]>,
) -> Result<Row> {
    let mut row = Vec::with_capacity(t.columns.len());
    for c in 0..t.columns.len() {
        if wanted.is_some_and(|w| !w.get(c).copied().unwrap_or(false)) {
            row.push(Value::Null);
        } else {
            row.push(value_of(txn, t, seg, c, i)?);
        }
    }
    Ok(row)
}

/// Marks the live segment row under `key` deleted (if there is one);
/// whether there was.
fn mark(txn: &mut dyn KvTxn, t: &Table, key: &[u8]) -> Result<bool> {
    if locate(&*txn, t, key, false)?.is_none() {
        return Ok(false);
    }
    txn.put(&marker_key(t, key), &[])?;
    Ok(true)
}

// ------------------------------------------------------------------ writes

/// Rows a statement inserts are gathered (by table) until it ends — or
/// until this many, when they go to segments at once — and go to segments
/// when they are many ([`MERGE_ROWS`]), else to the delta store: a load
/// never passes through the delta store (whose deleted rows stay as
/// versions until a vacuum).
const BUFFER_ROWS: usize = 4 * SEGMENT_ROWS;

/// Counts a row the statement writes to `t` — and so notes `t` among the
/// statement's tables at once ([`crate::storage::with_writes`]): the rows
/// gathered for the statement's end live in its context and never outlive
/// it, even when it fails before its caller notes the table.
fn note_written(t: &Table) {
    crate::storage::with_writes(t, |w| w.rows += 1);
}

fn buffered<T>(t: &Table, f: impl FnOnce(&mut std::collections::BTreeMap<Vec<u8>, Row>) -> T) -> T {
    crate::storage::with_writes(t, |w| f(&mut w.gathered))
}

/// The row gathered under `key`, if the statement gathered it.
fn gathered(t: &Table, key: &[u8]) -> Option<Row> {
    crate::storage::peek_writes(t, |w| w.and_then(|w| w.gathered.get(key).cloned()))
}

fn is_gathered(t: &Table, key: &[u8]) -> bool {
    crate::storage::peek_writes(t, |w| {
        w.is_some_and(|w| {
            // Past the last key — a load's every question — is one
            // comparison, not a search.
            w.gathered
                .last_key_value()
                .is_some_and(|(last, _)| key <= last.as_slice() && w.gathered.contains_key(key))
        })
    })
}

/// Rows the statement inserts or changes (keyed), gathered for its end —
/// appended at once when they come after every row gathered — and stored
/// in segments when they are many (counted, and gathered, in one look at
/// the statement's context).
fn gather(txn: &mut dyn KvTxn, t: &Table, rows: Vec<(Vec<u8>, Row)>) -> Result<()> {
    let full = crate::storage::with_writes(t, |w| {
        w.rows += rows.len();
        let after = match (w.gathered.last_key_value(), rows.first()) {
            (Some((last, _)), Some((first, _))) => first > last,
            _ => true,
        };
        if after && rows.len() > 1 {
            let mut more: std::collections::BTreeMap<Vec<u8>, Row> = rows.into_iter().collect();
            w.gathered.append(&mut more);
        } else {
            for (k, row) in rows {
                w.gathered.insert(k, row);
            }
        }
        w.gathered.len() >= BUFFER_ROWS
    });
    if full {
        flush_buffer(txn, t)?;
    }
    Ok(())
}

/// Whether `t` holds no row keyed in `[first, last]` — none gathered, in
/// the delta store or in a segment — as one look at each tells (a segment
/// the range meets counts as holding one): what lets a load's batch skip
/// a look a row ([`ColumnStorage::insert_batch`]).  Read unvalidated, as
/// [`exists_plain`] reads; `false` for a table whose copy is not ready.
fn none_between(txn: &dyn KvTxn, t: &Table, first: &[u8], last: &[u8]) -> Result<bool> {
    if t.copy != CopyState::Ready {
        return Ok(false);
    }
    let range = (Bound::Included(first), Bound::Included(last));
    if crate::storage::peek_writes(t, |w| {
        w.is_some_and(|w| w.gathered.range::<[u8], _>(range).next().is_some())
    }) {
        return Ok(false);
    }
    // The first segment ending at or after `first` (the directory is keyed
    // by a segment's last key), if it starts by `last`.
    let mut k = Vec::with_capacity(INDEX_PREFIX_LEN + first.len());
    t.push_index_prefix(&mut k, DIRECTORY);
    k.extend_from_slice(first);
    if let Some((d, v)) = txn.first(
        Bound::Included(&k),
        Bound::Excluded(&t.index_end(DIRECTORY)),
    )? && Segment::decode(&d[INDEX_PREFIX_LEN..], &v)?
        .first
        .as_slice()
        <= last
    {
        return Ok(false);
    }
    let (lo, hi) = (moved(t, first, DELTA), moved(t, last, DELTA));
    Ok(txn
        .first(Bound::Included(&lo), Bound::Included(&hi))?
        .is_none())
}

/// The rows gathered, taken to be stored.
fn take_buffer(t: &Table) -> std::collections::BTreeMap<Vec<u8>, Row> {
    crate::storage::with_writes(t, |w| std::mem::take(&mut w.gathered))
}

/// A full buffer of a `column` table: into segments — or, while changed
/// keys move in (no merges then), into the delta store.
fn flush_buffer(txn: &mut dyn KvTxn, t: &Table) -> Result<()> {
    let rows = take_buffer(t);
    if t.copy == CopyState::MovingFromChanged {
        claim_for_delta(&*txn, t, &rows)?;
        for (k, row) in rows {
            RowStorage.write(txn, t, &moved(t, &k, DELTA), &row, None)?;
        }
        return Ok(());
    }
    merge(txn, t, rows, Delta::Own)
}

/// A hash of `chunks` (and the table): a segment's id.
fn segment_id(t: &Table, chunks: &[Vec<u8>]) -> u64 {
    let mut h = crate::kernels::mix(u64::from(t.id) ^ 0x6e75_636c_6575_7321);
    for c in chunks {
        h = crate::kernels::mix(h ^ c.len() as u64);
        for w in c.chunks(8) {
            let mut b = [0u8; 8];
            b[..w.len()].copy_from_slice(w);
            h = crate::kernels::mix(h ^ u64::from_le_bytes(b));
        }
    }
    h
}

/// The table's columns a segment of `t` holds: those of its column copy.
fn copy_columns(t: &Table) -> Vec<usize> {
    if t.columnar.is_empty() {
        (0..t.columns.len()).collect()
    } else {
        t.columnar.clone()
    }
}

/// Writes `rows` (keyed, ascending) as one segment of the columns `cols`,
/// their values moved into the columns (no value copied).
fn write_segment(
    txn: &mut dyn KvTxn,
    t: &Table,
    rows: Vec<(Vec<u8>, Row)>,
    cols: &[usize],
) -> Result<()> {
    let prefix = crate::catalog::INDEX_PREFIX_LEN;
    let n = rows.len();
    let threads = crate::budget::workers().min(cols.len());
    let (keys, encoded): (Vec<Vec<u8>>, Vec<_>) = if threads > 1 && n >= SEGMENT_ROWS / 4 {
        // On threads: the rows cut into parts, each taken apart into its
        // columns; then each column's parts joined, encoded and measured,
        // a share of the columns each (they are independent).
        let mut rows = rows;
        let mut parts = Vec::with_capacity(threads);
        let size = n.div_ceil(threads);
        while rows.len() > size {
            parts.push(rows.split_off(rows.len() - size));
        }
        parts.push(rows);
        parts.reverse();
        std::thread::scope(|sc| {
            let split: Vec<_> = parts
                .into_iter()
                .map(|part| sc.spawn(|| columns_of(part, cols)))
                .collect();
            let mut keys = Vec::with_capacity(n);
            // [column][part]
            let mut columns: Vec<Vec<Vec<Value>>> = cols.iter().map(|_| Vec::new()).collect();
            for h in split {
                let (k, cs) = h.join().expect("taking rows apart does not panic");
                keys.extend(k);
                for (column, part) in columns.iter_mut().zip(cs) {
                    column.push(part);
                }
            }
            let share = cols.len().div_ceil(threads);
            let mut shares = Vec::with_capacity(threads);
            let mut columns = columns.into_iter();
            loop {
                let share: Vec<Vec<Vec<Value>>> = columns.by_ref().take(share).collect();
                if share.is_empty() {
                    break;
                }
                shares.push(share);
            }
            let encoded: Vec<_> = shares
                .into_iter()
                .map(|share| {
                    sc.spawn(move || {
                        share
                            .into_iter()
                            .map(|parts| {
                                let mut column = Vec::with_capacity(n);
                                for p in parts {
                                    column.extend(p);
                                }
                                crate::segment::encode(column)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            let encoded = encoded
                .into_iter()
                .flat_map(|h| h.join().expect("a column's encoding does not panic"))
                .collect();
            (keys, encoded)
        })
    } else {
        let (keys, columns) = columns_of(rows, cols);
        (
            keys,
            columns.into_iter().map(crate::segment::encode).collect(),
        )
    };
    let suffixes: Vec<&[u8]> = keys.iter().map(|k| &k[prefix..]).collect();
    let mut chunks = vec![Keys::encode(&suffixes)];
    let mut stats = Vec::with_capacity(cols.len());
    for (chunk, s) in encoded {
        stats.push(s);
        chunks.push(chunk);
    }
    let id = segment_id(t, &chunks);
    txn.put(&chunk_key(t, id, KEYS), &chunks[0])?;
    for (&c, chunk) in cols.iter().zip(&chunks[1..]) {
        txn.put(&chunk_key(t, id, c as u16), chunk)?;
    }
    let seg = Segment {
        id,
        first: keys[0].clone(),
        last: keys[n - 1].clone(),
        rows: n,
        cols: cols.iter().map(|&c| c as u16).collect(),
        stats,
    };
    txn.put(&dir_key(t, &seg.last), &seg.encode())?;
    Ok(())
}

/// Keyed rows taken apart: their keys, and the columns `cols` of them,
/// each value moved (no value copied).
fn columns_of(rows: Vec<(Vec<u8>, Row)>, cols: &[usize]) -> (Vec<Vec<u8>>, Vec<Vec<Value>>) {
    let mut keys = Vec::with_capacity(rows.len());
    let mut columns: Vec<Vec<Value>> = cols
        .iter()
        .map(|_| Vec::with_capacity(rows.len()))
        .collect();
    for (k, mut row) in rows {
        for (column, &c) in columns.iter_mut().zip(cols) {
            column.push(
                row.get_mut(c)
                    .map_or(Value::Null, |v| std::mem::replace(v, Value::Null)),
            );
        }
        keys.push(k);
    }
    (keys, columns)
}

/// Removes `seg`: its columns, the markers of its rows, its directory
/// entry.  (A marker written meanwhile stays: it marks the row where the
/// merge writes it.)
fn drop_segment(txn: &mut dyn KvTxn, t: &Table, seg: &Segment) -> Result<()> {
    let keys = keys_of(&*txn, t, seg)?;
    let prefix = t.index_prefix(0);
    for i in deleted(&*txn, t, seg, false)? {
        let mut k = prefix.clone();
        k.extend_from_slice(keys.get(i as usize));
        txn.delete(&marker_key(t, &k))?;
    }
    txn.delete(&chunk_key(t, seg.id, KEYS))?;
    for &c in &seg.cols {
        txn.delete(&chunk_key(t, seg.id, c))?;
    }
    txn.delete(&dir_key(t, &seg.last))?;
    // Its values leave memory with it (a reader still on it decodes again).
    uncache(seg);
    Ok(())
}

/// Segments being written, in key order: rows gathered until a segment is
/// full.
struct SegmentWriter<'t> {
    t: &'t Table,
    cols: Vec<usize>,
    rows: Vec<(Vec<u8>, Row)>,
    bytes: usize,
}

impl SegmentWriter<'_> {
    fn push(&mut self, txn: &mut dyn KvTxn, key: Vec<u8>, row: Row) -> Result<()> {
        self.bytes += crate::spill::row_bytes(&row);
        self.rows.push((key, row));
        if self.rows.len() >= SEGMENT_ROWS || self.bytes >= SEGMENT_BYTES {
            self.flush(txn)?;
        }
        Ok(())
    }

    fn flush(&mut self, txn: &mut dyn KvTxn) -> Result<()> {
        if !self.rows.is_empty() {
            write_segment(txn, self.t, std::mem::take(&mut self.rows), &self.cols)?;
        }
        self.bytes = 0;
        Ok(())
    }
}

/// The rows a merge takes in, in key order: those not in segments (copied
/// aside) and a statement's buffered ones.
struct Incoming {
    delta: std::iter::Peekable<Box<dyn Iterator<Item = Result<Row>>>>,
    extra: std::iter::Peekable<std::collections::btree_map::IntoIter<Vec<u8>, Row>>,
}

/// A row not in segments as a merge copies it aside: its key, whether it
/// exists, its values.
fn aside(key: Vec<u8>, row: Option<Row>) -> Row {
    let mut out = vec![Value::Bytea(key.into()), Value::Bool(row.is_some())];
    out.extend(row.unwrap_or_default());
    out
}

/// What [`Incoming::next`] gives: a key, its row (`None`: deleted), whether
/// it came from the delta.
type Item = (Vec<u8>, Option<Row>, bool);

impl Incoming {
    /// The next row's key, and whether it comes from the delta (borrowed:
    /// a merge asks once or twice a row).
    fn peek(&mut self) -> Result<Option<(&[u8], bool)>> {
        if let Some(Err(_)) = self.delta.peek() {
            return Err(self.delta.next().expect("peeked").expect_err("an error"));
        }
        let d = match self.delta.peek() {
            Some(Ok(row)) => match row.first() {
                Some(Value::Bytea(k)) => Some(&k[..]),
                _ => return Err(Error::internal("a row copied aside without its key")),
            },
            _ => None,
        };
        let e = self.extra.peek().map(|(k, _)| k.as_slice());
        Ok(match (d, e) {
            (None, None) => None,
            (Some(d), None) => Some((d, true)),
            (None, Some(e)) => Some((e, false)),
            (Some(d), Some(e)) => Some(if d <= e { (d, true) } else { (e, false) }),
        })
    }

    fn next(&mut self) -> Result<Option<Item>> {
        let Some((_, from_delta)) = self.peek()? else {
            return Ok(None);
        };
        if !from_delta {
            let (k, row) = self.extra.next().expect("peeked");
            return Ok(Some((k, Some(row), false)));
        }
        let mut row = self.delta.next().expect("peeked")?;
        let values = row.split_off(2);
        let present = matches!(row[1], Value::Bool(true));
        let Value::Bytea(k) = &row[0] else {
            return Err(Error::internal("a row copied aside without its key"));
        };
        Ok(Some((k.to_vec(), present.then_some(values), true)))
    }
}

/// Merges what is not in segments — and `extra`, rows a statement inserted
/// — into segments of the column copy's columns: the segments the incoming
/// keys fall among rewritten with them, deleted rows dropped, the delta
/// emptied.
fn merge(
    txn: &mut dyn KvTxn,
    t: &Table,
    extra: std::collections::BTreeMap<Vec<u8>, Row>,
    delta: Delta,
) -> Result<()> {
    merge_with(txn, t, extra, delta, false)
}

/// [`merge`], the delta read validated (`validated`): its commit fails if
/// a write to the delta committed meanwhile — what a switch of storage
/// that takes the delta in whole needs.
fn merge_with(
    txn: &mut dyn KvTxn,
    t: &Table,
    extra: std::collections::BTreeMap<Vec<u8>, Row>,
    delta: Delta,
    validated: bool,
) -> Result<()> {
    if matches!(delta, Delta::Both(..)) {
        return Err(Error::internal("a merge while a delta moves"));
    }
    let start = t.index_prefix(0);
    // The delta, copied aside (past work_mem to scratch space), and the
    // first and last keys coming in.
    let mut stash = crate::spill::Stash::new();
    let mut range: Option<(Vec<u8>, Vec<u8>)> = None;
    let mut widen = |k: &[u8]| {
        range = Some(match range.take() {
            None => (k.to_vec(), k.to_vec()),
            Some((f, l)) => (f.min(k.to_vec()), l.max(k.to_vec())),
        });
    };
    for r in delta_items(&*txn, t, &start, None, None, validated, delta)? {
        let (k, row) = r?;
        widen(&k);
        stash.push(aside(k, row))?;
    }
    if let (Some((f, _)), Some((l, _))) = (extra.first_key_value(), extra.last_key_value()) {
        widen(f);
        widen(l);
    }
    let Some((lo, hi)) = range else {
        return Ok(());
    };
    // The segments the incoming keys fall among, and the delta store's
    // keys among them, claimed: a write committed over them meanwhile
    // conflicts with this merge.
    let old = segments_through(&*txn, t, &lo, &hi, true)?;
    claim_delta(&*txn, t, &lo, &hi)?;
    let mut incoming = Incoming {
        delta: stash.seal()?.into_iter().peekable(),
        extra: extra.into_iter().peekable(),
    };
    let cols = copy_columns(t);
    let mut out = SegmentWriter {
        t,
        cols: cols.clone(),
        rows: Vec::new(),
        bytes: 0,
    };
    // A delta row (or changed key) moves out as it goes in.
    let take =
        |txn: &mut dyn KvTxn, out: &mut SegmentWriter, (k, row, from_delta): Item| -> Result<()> {
            if from_delta {
                match delta {
                    Delta::Own => RowStorage.remove(txn, t, &moved(t, &k, DELTA))?,
                    Delta::Changed => txn.delete(&moved(t, &k, CHANGED))?,
                    Delta::Both(..) => unreachable!("refused above"),
                }
            }
            match row {
                Some(row) => out.push(txn, k, row),
                None => Ok(()),
            }
        };
    for seg in &old {
        // The old segment, read whole, goes before anything is written
        // over it (a new segment may end at its last key).
        let keys = keys_of(&*txn, t, seg)?;
        let vectors = (0..t.columns.len())
            .map(|c| stored_column(&*txn, t, seg, c))
            .collect::<Result<Vec<_>>>()?;
        let gone = deleted(&*txn, t, seg, false)?;
        drop_segment(txn, t, seg)?;
        for i in 0..seg.rows {
            let mut key = start.clone();
            key.extend_from_slice(keys.get(i));
            while incoming.peek()?.is_some_and(|(k, _)| k < key.as_slice()) {
                let r = incoming.next()?.expect("peeked");
                take(txn, &mut out, r)?;
            }
            if incoming.peek()?.is_some_and(|(k, _)| k == key.as_slice()) {
                let r = incoming.next()?.expect("peeked");
                take(txn, &mut out, r)?; // the new row (or none) replaces it
                continue;
            }
            if gone.contains(&(i as u32)) {
                continue;
            }
            let row = vectors.iter().map(|v| v.value(i)).collect();
            out.push(txn, key, row)?;
        }
    }
    while let Some(r) = incoming.next()? {
        take(txn, &mut out, r)?;
    }
    out.flush(txn)
}

// ------------------------------------------------------------------ scans

/// A run of a scan's rows: delta rows, or rows of a segment.
enum Run {
    Delta(Vec<(Vec<u8>, Row)>),
    /// Rows `range` of the segment, those of `live` not deleted.
    Segment {
        seg: Rc<Segment>,
        range: std::ops::Range<usize>,
        live: Vec<u32>,
    },
}

/// A scan's rows in key order: the segments' and the delta store's
/// merged.
struct Merged<'a> {
    txn: &'a dyn KvTxn,
    t: &'a Table,
    /// The columns a dictionary segment gives as codes ([`Scan::dicts`]).
    dicts: Vec<usize>,
    wanted: Option<&'a [bool]>,
    validated: bool,
    end: Option<Vec<u8>>,
    segs: VecDeque<Segment>,
    delta: std::iter::Peekable<Box<dyn Iterator<Item = Result<(Vec<u8>, Option<Row>)>> + 'a>>,
    /// The segment being read: it, its keys, its deleted rows, where.
    cur: Option<(Rc<Segment>, Arc<Keys>, BTreeSet<u32>, usize, usize)>,
    prefix: Vec<u8>,
}

impl<'a> Merged<'a> {
    fn new(scan: Scan<'a>, delta: Delta) -> Result<Self> {
        let Scan {
            txn,
            table: t,
            start,
            end,
            wanted,
            validated,
            bounds,
            dicts,
        } = scan;
        let segs = segments(txn, t, &start, end.as_deref())?
            .into_iter()
            .filter(|s| !s.excluded(&bounds))
            .collect();
        let delta =
            delta_items(txn, t, &start, end.as_deref(), wanted, validated, delta)?.peekable();
        let mut m = Merged {
            txn,
            t,
            dicts,
            wanted,
            validated,
            end,
            segs,
            delta,
            cur: None,
            prefix: t.index_prefix(0),
        };
        m.start_at(&start)?;
        Ok(m)
    }

    /// Opens the next segment, from the first row `>= start`.
    fn start_at(&mut self, start: &[u8]) -> Result<()> {
        self.cur = None;
        while let Some(seg) = self.segs.pop_front() {
            let keys = keys_of(self.txn, self.t, &seg)?;
            let from = if start > self.prefix.as_slice() {
                keys.search(&start[self.prefix.len().min(start.len())..])
            } else {
                0
            };
            let to = match &self.end {
                Some(e) if e.len() >= self.prefix.len() && e.starts_with(&self.prefix) => {
                    keys.search(&e[self.prefix.len()..])
                }
                Some(e) if e.as_slice() <= self.prefix.as_slice() => 0,
                _ => keys.len(),
            };
            if from < to {
                let gone = deleted(self.txn, self.t, &seg, self.validated)?;
                self.cur = Some((Rc::new(seg), keys, gone, from, to));
                return Ok(());
            }
        }
        Ok(())
    }

    fn key_at(&self, keys: &Keys, i: usize) -> Vec<u8> {
        let mut k = self.prefix.clone();
        k.extend_from_slice(keys.get(i));
        k
    }

    /// The next run, of at most `max` rows.
    fn next_run(&mut self, max: usize) -> Result<Option<Run>> {
        loop {
            let Some((seg, keys, gone, pos, to)) = &self.cur else {
                // No segment left: the delta's rows.
                let mut rows = Vec::new();
                while rows.len() < max {
                    match self.delta.next() {
                        Some(r) => {
                            if let (k, Some(row)) = r? {
                                rows.push((k, row));
                            }
                        }
                        None => break,
                    }
                }
                if rows.is_empty() && self.delta.peek().is_some() {
                    continue;
                }
                return Ok((!rows.is_empty()).then_some(Run::Delta(rows)));
            };
            let (pos, to) = (*pos, *to);
            if pos >= to {
                let next_start = self.key_at(keys, to.saturating_sub(1));
                let mut after = next_start;
                after.push(0);
                self.start_at(&after)?;
                continue;
            }
            let here = self.key_at(keys, pos);
            // Delta rows before the segment's next row first.
            let mut rows = Vec::new();
            while rows.len() < max {
                match self.delta.peek() {
                    Some(Ok((k, _))) if *k < here => {
                        if let (k, Some(row)) = self.delta.next().expect("peeked")? {
                            rows.push((k, row));
                        }
                    }
                    Some(Err(_)) => {
                        return Err(self.delta.next().expect("peeked").expect_err("an error"));
                    }
                    _ => break,
                }
            }
            if !rows.is_empty() {
                return Ok(Some(Run::Delta(rows)));
            }
            // The segment's rows up to the next delta key (a delta row of
            // the same key replaces the segment's).
            let mut stop = (pos + max).min(to);
            let mut replaced = false;
            if let Some(Ok((k, _))) = self.delta.peek()
                && k.starts_with(&self.prefix)
            {
                let at = keys.search(&k[self.prefix.len()..]);
                if at < stop {
                    stop = at.max(pos);
                    replaced = at < to && keys.get(at) == &k[self.prefix.len()..];
                }
            }
            if stop == pos {
                // The delta row comes now (or a deletion); the segment row
                // of its key (if any) is skipped.
                let (k, row) = self.delta.next().expect("peeked")?;
                if replaced && let Some(c) = &mut self.cur {
                    c.3 = pos + 1;
                }
                match row {
                    Some(row) => return Ok(Some(Run::Delta(vec![(k, row)]))),
                    None => continue,
                }
            }
            let live: Vec<u32> = (pos as u32..stop as u32)
                .filter(|i| !gone.contains(i))
                .collect();
            let seg = Rc::clone(seg);
            if let Some(c) = &mut self.cur {
                c.3 = stop;
            }
            if live.is_empty() {
                continue;
            }
            return Ok(Some(Run::Segment {
                seg,
                range: pos..stop,
                live,
            }));
        }
    }
}

impl BatchSource for Merged<'_> {
    fn next_batch(&mut self, max: usize) -> Result<Option<Batch>> {
        match self.next_run(max)? {
            None => Ok(None),
            Some(Run::Delta(rows)) => Ok(crate::vector::batch_of_rows(
                rows.into_iter().map(|(_, r)| r).collect(),
            )),
            Some(Run::Segment { seg, range, live }) => {
                let len = range.len();
                let mut cols = Vec::with_capacity(self.t.columns.len());
                for c in 0..self.t.columns.len() {
                    if self
                        .wanted
                        .is_some_and(|w| !w.get(c).copied().unwrap_or(false))
                    {
                        cols.push(Rc::new(Vector::Const(Value::Null)));
                    } else {
                        let v = column_of(self.txn, self.t, &seg, c)?;
                        cols.push(Rc::new(match &*v {
                            Vector::Const(x) => Vector::Const(x.clone()),
                            // A dictionary's codes for a filter that asked
                            // for them; its text for anything else.
                            Vector::Dict(..) if !self.dicts.contains(&c) => v
                                .slice(range.clone())
                                .undict(&Sel::All(len))
                                .expect("a dictionary"),
                            v => v.slice(range.clone()),
                        }));
                    }
                }
                let sel = if live.len() == len {
                    Sel::All(len)
                } else {
                    Sel::Rows(live.iter().map(|&i| i - range.start as u32).collect())
                };
                Ok(Some(Batch { len, cols, sel }))
            }
        }
    }
}

// ----------------------------------------------------------------- storage

/// Rows of a run of a segment, as rows (keys and the wanted columns).
fn run_rows(
    txn: &dyn KvTxn,
    t: &Table,
    seg: &Segment,
    live: &[u32],
    wanted: Option<&[bool]>,
    out: &mut VecDeque<(Vec<u8>, Row)>,
) -> Result<()> {
    let keys = keys_of(txn, t, seg)?;
    let cols = (0..t.columns.len())
        .map(|c| {
            if wanted.is_some_and(|w| !w.get(c).copied().unwrap_or(false)) {
                Ok(None)
            } else {
                column_of(txn, t, seg, c).map(Some)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let prefix = t.index_prefix(0);
    for &i in live {
        let i = i as usize;
        let mut key = prefix.clone();
        key.extend_from_slice(keys.get(i));
        let row = cols
            .iter()
            .map(|v| v.as_ref().map_or(Value::Null, |v| v.value(i)))
            .collect();
        out.push_back((key, row));
    }
    Ok(())
}

/// A scan's rows (with keys) through the segments and the delta.
fn merged_rows<'a>(scan: Scan<'a>, delta: Delta) -> Result<RowIter<'a>> {
    let mut m = Merged::new(scan, delta)?;
    let mut buf: VecDeque<(Vec<u8>, Row)> = VecDeque::new();
    Ok(Box::new(std::iter::from_fn(move || {
        loop {
            if let Some(kr) = buf.pop_front() {
                return Some(Ok(kr));
            }
            match m.next_run(1024) {
                Err(e) => return Some(Err(e)),
                Ok(None) => return None,
                Ok(Some(Run::Delta(rows))) => buf.extend(rows),
                Ok(Some(Run::Segment { seg, live, .. })) => {
                    if let Err(e) = run_rows(m.txn, m.t, &seg, &live, m.wanted, &mut buf) {
                        return Some(Err(e));
                    }
                }
            }
        }
    })))
}

/// [`TableStorage::sample`] through the segments and the delta: a
/// segment's columns decoded when the sample first takes one of its rows
/// (not at all when it takes none), a row made of them only when taken.
fn merged_sample(scan: Scan<'_>, delta: Delta, sample: &mut crate::stats::Sample) -> Result<()> {
    let mut m = Merged::new(scan, delta)?;
    while let Some(run) = m.next_run(SEGMENT_ROWS)? {
        match run {
            Run::Delta(rows) => {
                for (_, row) in rows {
                    sample.offer(|| Ok(row))?;
                }
            }
            Run::Segment { seg, live, .. } => {
                let mut cols: Option<Vec<Arc<Vector>>> = None;
                for &i in &live {
                    sample.offer(|| {
                        let cols = match &mut cols {
                            Some(cols) => cols,
                            None => cols.insert(
                                (0..m.t.columns.len())
                                    .map(|c| column_of(m.txn, m.t, &seg, c))
                                    .collect::<Result<Vec<_>>>()?,
                            ),
                        };
                        Ok(cols.iter().map(|v| v.value(i as usize)).collect())
                    })?;
                }
            }
        }
    }
    Ok(())
}

/// Segment boundaries in `[start, end)`: where a parallel scan splits.
fn segment_splits(
    txn: &dyn KvTxn,
    t: &Table,
    start: &[u8],
    end: Option<&[u8]>,
    parts: usize,
) -> Result<Vec<Vec<u8>>> {
    let segs = segments(txn, t, start, end)?;
    let firsts: Vec<&Vec<u8>> = segs
        .iter()
        .map(|s| &s.first)
        .filter(|f| f.as_slice() > start && end.is_none_or(|e| f.as_slice() < e))
        .collect();
    let parts = parts.min(firsts.len() + 1);
    Ok((1..parts)
        .map(|j| firsts[j * firsts.len() / parts].clone())
        .collect())
}

/// Whether the delta in the store is large (the KV splits it).
fn delta_large(txn: &dyn KvTxn, t: &Table, delta: Delta) -> Result<bool> {
    let p = t.index_prefix(delta_index(delta));
    let end = prefix_end(&p);
    Ok(!txn.split_points(&p, end.as_deref(), 2)?.is_empty())
}

/// The segments' rows (not deleted, not replaced by the delta) and the
/// delta's, checked; keyed, in key order.
fn check_segments(
    txn: &dyn KvTxn,
    t: &Table,
    delta: Delta,
    problems: &mut Vec<String>,
) -> Result<Vec<(Vec<u8>, Row)>> {
    let start = t.index_prefix(0);
    let end = prefix_end(&start);
    let mut last: Option<Vec<u8>> = None;
    for seg in segments(txn, t, &start, end.as_deref())? {
        let keys = match keys_of(txn, t, &seg) {
            Ok(k) => k,
            Err(e) => {
                problems.push(format!("{}: segment {:x}: {e}", t.name, seg.id));
                continue;
            }
        };
        let full = |i| {
            let mut k = start.clone();
            k.extend_from_slice(keys.get(i));
            k
        };
        if seg.rows == 0
            || full(0) != seg.first
            || full(seg.rows - 1) != seg.last
            || (1..seg.rows).any(|i| keys.get(i - 1) >= keys.get(i))
        {
            problems.push(format!(
                "{}: segment {:x} keys out of order",
                t.name, seg.id
            ));
        }
        if last.as_ref().is_some_and(|l| *l >= seg.first) {
            problems.push(format!(
                "{}: segment {:x} overlaps the one before",
                t.name, seg.id
            ));
        }
        last = Some(seg.last.clone());
        for &c in &seg.cols {
            match column_of(txn, t, &seg, usize::from(c)) {
                Ok(v) if vector_len(&v) == seg.rows => {}
                Ok(_) => problems.push(format!(
                    "{}: segment {:x} column {c} has the wrong length",
                    t.name, seg.id
                )),
                Err(e) => {
                    problems.push(format!("{}: segment {:x} column {c}: {e}", t.name, seg.id))
                }
            }
        }
        if deleted(txn, t, &seg, false)?
            .iter()
            .any(|&r| r as usize >= seg.rows)
        {
            problems.push(format!(
                "{}: segment {:x} marks rows it lacks",
                t.name, seg.id
            ));
        }
    }
    if delta == Delta::Own {
        // Every delta row's key is no live segment row's.
        for r in delta_items(txn, t, &start, None, None, false, delta)? {
            let (k, _) = r?;
            if locate(txn, t, &k, false)?.is_some() {
                problems.push(format!(
                    "{}: row under {k:?} both in a segment and in the delta store",
                    t.name
                ));
            }
        }
    }
    merged_rows(
        Scan {
            txn,
            table: t,
            start,
            end,
            wanted: None,
            validated: false,
            bounds: Vec::new(),
            dicts: Vec::new(),
        },
        delta,
    )?
    .collect()
}

/// The columnar storage.
pub struct ColumnStorage;

impl TableStorage for ColumnStorage {
    fn name(&self) -> &str {
        COLUMN
    }

    fn index_ids(&self, t: &Table) -> Vec<u32> {
        let mut ids = vec![
            crate::large::LARGE_INDEX,
            SEGMENTS,
            DELETED,
            DIRECTORY,
            DELTA,
        ];
        if matches!(
            t.copy,
            CopyState::DroppingRows | CopyState::MovingFromChanged
        ) {
            ids.extend([0, CHANGED]); // the row copy, being moved or removed
        }
        ids
    }

    fn write(
        &self,
        txn: &mut dyn KvTxn,
        t: &Table,
        key: &[u8],
        row: &Row,
        old: Option<&Row>,
    ) -> Result<()> {
        if old.is_some() && !is_gathered(t, key) {
            let in_delta = moved(t, key, DELTA);
            if txn.get(&in_delta)?.is_some() {
                note_written(t);
                return RowStorage.write(txn, t, &in_delta, row, old); // a delta row
            }
            // A segment's row: marked deleted, its new version gathered as
            // an insert is.
            mark(txn, t, key)?;
        }
        // An insert, or a change of one: gathered for the statement's end,
        // and stored in segments when they are many.
        gather(txn, t, vec![(key.to_vec(), row.clone())])
    }

    fn insert_batch(
        &self,
        txn: &mut dyn KvTxn,
        t: &Table,
        rows: Vec<NewRow>,
        new_keys: bool,
    ) -> Result<Option<usize>> {
        // A batch in ascending key order where the table holds no row (a
        // load's, past every row): two looks for the batch, not two a row,
        // and the rows gathered as they are handed over.
        let ascending = rows.windows(2).all(|w| w[0].key < w[1].key);
        if let (true, Some(first), Some(last)) = (ascending, rows.first(), rows.last())
            && (new_keys || none_between(&*txn, t, &first.key, &last.key)?)
        {
            gather(txn, t, rows.into_iter().map(|r| (r.key, r.row)).collect())?;
            return Ok(None);
        }
        for (i, r) in rows.into_iter().enumerate() {
            if !new_keys && self.exists(&*txn, t, &r.key, false)? {
                return Ok(Some(i));
            }
            gather(txn, t, vec![(r.key, r.row)])?;
        }
        Ok(None)
    }

    fn remove(&self, txn: &mut dyn KvTxn, t: &Table, key: &[u8]) -> Result<()> {
        if t.copy == CopyState::MovingFromChanged {
            // A version among the changed keys goes too.
            txn.delete(&moved(t, key, CHANGED))?;
        }
        if buffered(t, |b| b.remove(key).is_some()) {
            return Ok(());
        }
        let in_delta = moved(t, key, DELTA);
        if txn.get(&in_delta)?.is_some() {
            RowStorage.remove(txn, t, &in_delta)?;
        }
        mark(txn, t, key)?;
        Ok(())
    }

    fn exists(&self, txn: &dyn KvTxn, t: &Table, key: &[u8], validated: bool) -> Result<bool> {
        if is_gathered(t, key) {
            return Ok(true);
        }
        if !validated && t.copy == CopyState::Ready {
            return exists_plain(txn, t, key);
        }
        if RowStorage.exists(txn, t, &moved(t, key, DELTA), validated)? {
            return Ok(true);
        }
        if t.copy == CopyState::MovingFromChanged && txn.get(&moved(t, key, CHANGED))?.is_some() {
            return RowStorage.exists(txn, t, key, validated);
        }
        Ok(locate(txn, t, key, validated)?.is_some())
    }

    fn get(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        key: &[u8],
        wanted: Option<&[bool]>,
        labels: &mut Labels,
    ) -> Result<Option<Row>> {
        if let Some(row) = gathered(t, key) {
            return Ok(Some(row));
        }
        if let Some(row) = RowStorage.get(txn, t, &moved(t, key, DELTA), wanted, labels)? {
            return Ok(Some(row));
        }
        if t.copy == CopyState::MovingFromChanged && txn.get(&moved(t, key, CHANGED))?.is_some() {
            return RowStorage.get(txn, t, key, wanted, labels);
        }
        match locate(txn, t, key, false)? {
            None => Ok(None),
            Some((seg, i)) => Ok(Some(segment_row(txn, t, &seg, i, wanted)?)),
        }
    }

    fn rows<'a>(&self, scan: Scan<'a>) -> Result<RowIter<'a>> {
        let delta = delta_of(scan.table);
        merged_rows(scan, delta)
    }

    fn sample(&self, scan: Scan<'_>, sample: &mut crate::stats::Sample) -> Result<()> {
        let delta = delta_of(scan.table);
        merged_sample(scan, delta, sample)
    }

    fn batches<'a>(&self, scan: Scan<'a>) -> Result<Box<dyn BatchSource + 'a>> {
        let delta = delta_of(scan.table);
        Ok(Box::new(Merged::new(scan, delta)?))
    }

    fn split_points(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        start: &[u8],
        end: Option<&[u8]>,
        parts: usize,
    ) -> Result<Vec<Vec<u8>>> {
        segment_splits(txn, t, start, end, parts)
    }

    fn after_writes(&self, txn: &mut dyn KvTxn, t: &Table) -> Result<()> {
        let w = crate::storage::take_writes(t);
        let (written, rows) = (w.rows, w.gathered);
        if written == 0 && rows.is_empty() {
            return Ok(());
        }
        // Many rows this statement, or a delta store grown large in the
        // store: to segments.  Else the rows inserted wait in the delta
        // store (and while changed keys move in, always).
        if t.copy != CopyState::MovingFromChanged
            && (rows.len() >= MERGE_ROWS
                || written >= MERGE_ROWS
                || delta_large(&*txn, t, Delta::Own)?)
        {
            return merge(txn, t, rows, Delta::Own);
        }
        claim_for_delta(&*txn, t, &rows)?;
        for (k, row) in rows {
            RowStorage.write(txn, t, &moved(t, &k, DELTA), &row, None)?;
        }
        Ok(())
    }

    fn check(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        problems: &mut Vec<String>,
    ) -> Result<Vec<(Vec<u8>, Row)>> {
        // The delta store's rows as the row storage checks its own.
        crate::check::rows_at(txn, t, DELTA, problems)?;
        check_segments(txn, t, delta_of(t), problems)
    }
}

/// [`ColumnStorage::exists`] of a key not gathered, unvalidated, in a
/// table with no copy being moved: the directory's first segment ending at
/// or after the key, and — when every segment ends before it — the delta
/// store's first row at or after it.  Both are `first` reads, which the
/// transaction answers from memory inside a range it found empty before
/// (`KvTxn::first`): a load's ascending keys past every stored row read
/// the store once each time it stores rows, not once a row (PLAN.md
/// §M14.20 W3).
fn exists_plain(txn: &dyn KvTxn, t: &Table, key: &[u8]) -> Result<bool> {
    // Once a row, so the keys are built in one buffer.
    let end_of = |index: u32| t.index_end(index);
    let suffix = &key[INDEX_PREFIX_LEN..];
    // The first segment ending at or after the key (the directory is keyed
    // by a segment's last key).
    let mut k = Vec::with_capacity(INDEX_PREFIX_LEN + key.len());
    t.push_index_prefix(&mut k, DIRECTORY);
    k.extend_from_slice(key);
    let seg = match txn.first(Bound::Included(&k), Bound::Excluded(&end_of(DIRECTORY)))? {
        Some((d, v)) => Some(entry(&d[INDEX_PREFIX_LEN..], &v)?),
        None => None,
    };
    // The key in the delta store.
    k.clear();
    t.push_index_prefix(&mut k, DELTA);
    k.extend_from_slice(suffix);
    let in_delta = k;
    let Some(seg) = seg else {
        // Every segment ends before the key: the delta store's first row
        // at or after it tells the rest.
        return Ok(txn
            .first(Bound::Included(&in_delta), Bound::Excluded(&end_of(DELTA)))?
            .is_some_and(|(d, _)| d == in_delta));
    };
    if RowStorage.exists(txn, t, &in_delta, false)? {
        return Ok(true);
    }
    if seg.first.as_slice() > key {
        return Ok(false);
    }
    Ok(locate_in(txn, t, key, false, seg)?.is_some())
}

/// The name of the storage keeping both copies.
pub const ROW_COLUMN: &str = "row,column";

/// Both copies (`row, column`): the rows whole in the row storage, which
/// key lookups and row scans read, and a column copy — of every column or
/// some — which scans of the columns it holds read: segments, and the keys
/// written since the copy took them in ([`CHANGED`]), whose rows are in
/// the row copy.  Writes go to the row copy and note the key; merges take
/// changed keys into segments.
pub struct HtapStorage;

impl HtapStorage {
    /// Whether the column copy can serve a scan of `wanted`.
    fn columnar(t: &Table, wanted: Option<&[bool]>) -> bool {
        match t.copy {
            CopyState::BuildingColumns
            | CopyState::DroppingColumns
            | CopyState::DroppingRows
            | CopyState::MovingFromChanged => false,
            CopyState::BuildingRows | CopyState::MovingFromDelta => true,
            CopyState::Ready => {
                t.columnar.is_empty()
                    || wanted.is_some_and(|w| {
                        w.iter()
                            .enumerate()
                            .all(|(c, &need)| !need || t.columnar.contains(&c))
                    })
            }
        }
    }

    /// Notes that the row under `key` changed: its segment row (if any)
    /// marked, its key among the changed.
    fn changed(txn: &mut dyn KvTxn, t: &Table, key: &[u8]) -> Result<()> {
        note_written(t);
        mark(txn, t, key)?;
        txn.put(&moved(t, key, CHANGED), &[])?;
        Ok(())
    }
}

impl TableStorage for HtapStorage {
    fn name(&self) -> &str {
        ROW_COLUMN
    }

    fn index_ids(&self, t: &Table) -> Vec<u32> {
        let mut ids = vec![
            0,
            crate::large::LARGE_INDEX,
            SEGMENTS,
            DELETED,
            DIRECTORY,
            CHANGED,
        ];
        if t.copy == CopyState::MovingFromDelta {
            ids.push(DELTA); // a column table's delta store, being moved
        }
        ids
    }

    fn write(
        &self,
        txn: &mut dyn KvTxn,
        t: &Table,
        key: &[u8],
        row: &Row,
        old: Option<&Row>,
    ) -> Result<()> {
        RowStorage.write(txn, t, key, row, old)?;
        Self::changed(txn, t, key)
    }

    fn remove(&self, txn: &mut dyn KvTxn, t: &Table, key: &[u8]) -> Result<()> {
        RowStorage.remove(txn, t, key)?;
        Self::changed(txn, t, key)
    }

    fn exists(&self, txn: &dyn KvTxn, t: &Table, key: &[u8], validated: bool) -> Result<bool> {
        let partial = matches!(t.copy, CopyState::BuildingRows | CopyState::MovingFromDelta);
        if partial && txn.get(&moved(t, key, CHANGED))?.is_none() {
            // The row copy is partial: the column copy (and the delta store
            // being moved) has the row.
            return Ok(RowStorage.exists(txn, t, key, validated)?
                || (t.copy == CopyState::MovingFromDelta
                    && RowStorage.exists(txn, t, &moved(t, key, DELTA), validated)?)
                || locate(txn, t, key, validated)?.is_some());
        }
        RowStorage.exists(txn, t, key, validated)
    }

    fn get(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        key: &[u8],
        wanted: Option<&[bool]>,
        labels: &mut Labels,
    ) -> Result<Option<Row>> {
        if let Some(row) = RowStorage.get(txn, t, key, wanted, labels)? {
            return Ok(Some(row));
        }
        let partial = matches!(t.copy, CopyState::BuildingRows | CopyState::MovingFromDelta);
        if partial && txn.get(&moved(t, key, CHANGED))?.is_none() {
            if t.copy == CopyState::MovingFromDelta
                && let Some(row) = RowStorage.get(txn, t, &moved(t, key, DELTA), wanted, labels)?
            {
                return Ok(Some(row));
            }
            return match locate(txn, t, key, false)? {
                None => Ok(None),
                Some((seg, i)) => Ok(Some(segment_row(txn, t, &seg, i, wanted)?)),
            };
        }
        Ok(None)
    }

    fn rows<'a>(&self, scan: Scan<'a>) -> Result<RowIter<'a>> {
        if matches!(
            scan.table.copy,
            CopyState::BuildingRows | CopyState::MovingFromDelta
        ) {
            let delta = delta_of(scan.table);
            return merged_rows(scan, delta);
        }
        RowStorage.rows(scan)
    }

    fn sample(&self, scan: Scan<'_>, sample: &mut crate::stats::Sample) -> Result<()> {
        if matches!(
            scan.table.copy,
            CopyState::BuildingRows | CopyState::MovingFromDelta
        ) {
            for r in self.rows(scan)? {
                let (_, row) = r?;
                sample.offer(|| Ok(row))?;
            }
            return Ok(());
        }
        RowStorage.sample(scan, sample)
    }

    fn batches<'a>(&self, scan: Scan<'a>) -> Result<Box<dyn BatchSource + 'a>> {
        if Self::columnar(scan.table, scan.wanted) {
            let delta = delta_of(scan.table);
            return Ok(Box::new(Merged::new(scan, delta)?));
        }
        RowStorage.batches(scan)
    }

    fn split_points(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        start: &[u8],
        end: Option<&[u8]>,
        parts: usize,
    ) -> Result<Vec<Vec<u8>>> {
        if t.copy == CopyState::BuildingColumns {
            return Ok(txn.split_points(start, end, parts)?);
        }
        segment_splits(txn, t, start, end, parts)
    }

    fn after_writes(&self, txn: &mut dyn KvTxn, t: &Table) -> Result<()> {
        let written = crate::storage::take_writes(t).rows;
        if written == 0 || t.copy != CopyState::Ready {
            return Ok(()); // (a copy being built takes the changes in at its end)
        }
        if written >= MERGE_ROWS || delta_large(&*txn, t, Delta::Changed)? {
            merge(txn, t, Default::default(), Delta::Changed)?;
        }
        Ok(())
    }

    fn check(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        problems: &mut Vec<String>,
    ) -> Result<Vec<(Vec<u8>, Row)>> {
        let rows = crate::check::rows_at(txn, t, 0, problems)?;
        if t.copy == CopyState::BuildingColumns {
            return Ok(rows);
        }
        if t.copy == CopyState::MovingFromDelta {
            crate::check::rows_at(txn, t, DELTA, problems)?;
        }
        let columns = check_segments(txn, t, delta_of(t), problems)?;
        if matches!(t.copy, CopyState::BuildingRows | CopyState::MovingFromDelta) {
            return Ok(columns);
        }
        // The column copy holds the row copy's rows (in its columns).
        let cols = copy_columns(t);
        let project = |(k, r): &(Vec<u8>, Row)| -> (Vec<u8>, Vec<Value>) {
            (
                k.clone(),
                cols.iter()
                    .map(|&c| r.get(c).cloned().unwrap_or(Value::Null))
                    .collect(),
            )
        };
        let a: Vec<_> = rows.iter().map(project).collect();
        let b: Vec<_> = columns.iter().map(project).collect();
        if a != b {
            problems.push(format!(
                "{}: the column copy ({} rows) is not the row copy ({} rows)",
                t.name,
                b.len(),
                a.len()
            ));
        }
        Ok(rows)
    }
}

// --------------------------------------------------------------- converting

/// The last key in `t`'s segments.
pub fn last_key(txn: &dyn KvTxn, t: &Table) -> Result<Option<Vec<u8>>> {
    let start = t.index_prefix(0);
    let end = prefix_end(&start);
    Ok(segments(txn, t, &start, end.as_deref())?
        .last()
        .map(|s| s.last.clone()))
}

/// Writes `rows` (keyed, ascending, after every segment) as a segment of
/// the column copy.
pub fn append_segment(txn: &mut dyn KvTxn, t: &Table, rows: Vec<(Vec<u8>, Row)>) -> Result<()> {
    write_segment(txn, t, rows, &copy_columns(t))
}

/// Takes a `row, column` table's changed keys into its segments
/// (`validated`: all of them, the commit failing if one was added
/// meanwhile).
pub fn take_changes(txn: &mut dyn KvTxn, t: &Table, validated: bool) -> Result<()> {
    merge_with(txn, t, Default::default(), Delta::Changed, validated)
}

/// Merges a `column` table's delta store into its segments (`validated`:
/// as [`take_changes`]).
pub fn flush_delta(txn: &mut dyn KvTxn, t: &Table, validated: bool) -> Result<()> {
    merge_with(txn, t, take_buffer(t), Delta::Own, validated)
}

/// Rewrites what keeps a deleted row's values (PLAN.md §M14.24 R2b): the
/// delta taken into segments, and each segment with rows marked deleted
/// written again without them (its old chunks deleted, its decoded columns
/// dropped from the cache — as [`drop_segment`] does for every segment).  The number of segments rewritten.  Nothing
/// while a storage conversion moves the table's rows.
pub fn scrub(txn: &mut dyn KvTxn, t: &Table) -> Result<usize> {
    if t.copy != CopyState::Ready || !matches!(t.storage.as_str(), COLUMN | ROW_COLUMN) {
        return Ok(0);
    }
    match delta_of(t) {
        Delta::Own => flush_delta(txn, t, false)?,
        _ => take_changes(txn, t, false)?,
    }
    let start = t.index_prefix(0);
    let end = prefix_end(&start);
    let cols = copy_columns(t);
    let mut rewritten = 0;
    for seg in segments(&*txn, t, &start, end.as_deref())? {
        let gone = deleted(&*txn, t, &seg, false)?;
        if gone.is_empty() {
            continue;
        }
        let keys = keys_of(&*txn, t, &seg)?;
        let vectors = (0..t.columns.len())
            .map(|c| stored_column(&*txn, t, &seg, c))
            .collect::<Result<Vec<_>>>()?;
        drop_segment(txn, t, &seg)?;
        let mut out = SegmentWriter {
            t,
            cols: cols.clone(),
            rows: Vec::new(),
            bytes: 0,
        };
        for i in (0..seg.rows).filter(|i| !gone.contains(&(*i as u32))) {
            let mut key = start.clone();
            key.extend_from_slice(keys.get(i));
            let row = vectors.iter().map(|v| v.value(i)).collect();
            out.push(txn, key, row)?;
        }
        out.flush(txn)?;
        rewritten += 1;
    }
    Ok(rewritten)
}

/// Drops a segment's decoded columns, keys and entry from the cache.
fn uncache(seg: &Segment) {
    for col in seg.cols.iter().copied().chain([KEYS, ENTRY]) {
        let key = (seg.id, col);
        cache(&key)
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
    }
}

/// The key of a `column` table's delta row for row key `key`.
pub fn delta_key(t: &Table, key: &[u8]) -> Vec<u8> {
    moved(t, key, DELTA)
}

/// Copies into the row copy up to `max` rows of the segments after `after`
/// (from the first for `None`) — those not written since the copy began
/// (their keys among the changed) nor copied already; the last key looked
/// at, or `None` when no row is left.
pub fn copy_segment_rows(
    txn: &mut dyn KvTxn,
    t: &Table,
    after: Option<&[u8]>,
    max: usize,
) -> Result<Option<Vec<u8>>> {
    let start = t.index_prefix(0);
    let from = match after {
        Some(a) => {
            let mut k = a.to_vec();
            k.push(0);
            k
        }
        None => start.clone(),
    };
    let end = prefix_end(&start);
    let mut last = None;
    let mut copied = 0;
    for seg in segments(&*txn, t, &from, end.as_deref())? {
        let keys = keys_of(&*txn, t, &seg)?;
        let gone = deleted(&*txn, t, &seg, false)?;
        let vectors = (0..t.columns.len())
            .map(|c| column_of(&*txn, t, &seg, c))
            .collect::<Result<Vec<_>>>()?;
        let first = if from.len() > start.len() {
            keys.search(&from[start.len()..])
        } else {
            0
        };
        for i in first..seg.rows {
            let mut key = start.clone();
            key.extend_from_slice(keys.get(i));
            last = Some(key.clone());
            if gone.contains(&(i as u32))
                || txn.get(&moved(t, &key, CHANGED))?.is_some()
                || txn.get(&key)?.is_some()
            {
                continue;
            }
            let row: Row = vectors.iter().map(|v| v.value(i)).collect();
            RowStorage.write(txn, t, &key, &row, None)?;
            copied += 1;
            if copied == max {
                return Ok(last);
            }
        }
    }
    Ok(last)
}

/// Moves up to `max` of a `column` table's changed keys (its state
/// [`CopyState::MovingFromChanged`]) into its delta store — a row copied
/// there, a deleted row's segment row marked — unless the delta store has a
/// newer version; how many it moved.
pub fn changed_to_delta(txn: &mut dyn KvTxn, t: &Table, max: usize) -> Result<usize> {
    let start = t.index_prefix(0);
    let items: Vec<(Vec<u8>, Option<Row>)> =
        delta_items(&*txn, t, &start, None, None, false, Delta::Changed)?
            .take(max)
            .collect::<Result<_>>()?;
    for (k, row) in &items {
        let in_delta = moved(t, k, DELTA);
        if txn.get(&in_delta)?.is_none() {
            mark(txn, t, k)?;
            if let Some(row) = row {
                RowStorage.write(txn, t, &in_delta, row, None)?;
            }
        }
        txn.delete(&moved(t, k, CHANGED))?;
    }
    Ok(items.len())
}

/// Moves up to `max` rows of a `row, column` table's delta store left from
/// a `column` table (its state [`CopyState::MovingFromDelta`]) into its row
/// copy and changed keys — unless a changed key has a newer version; how
/// many it moved.
pub fn delta_to_changed(txn: &mut dyn KvTxn, t: &Table, max: usize) -> Result<usize> {
    let start = t.index_prefix(0);
    let items: Vec<(Vec<u8>, Option<Row>)> =
        delta_items(&*txn, t, &start, None, None, false, Delta::Own)?
            .take(max)
            .collect::<Result<_>>()?;
    for (k, row) in &items {
        let changed = moved(t, k, CHANGED);
        if txn.get(&changed)?.is_none()
            && let Some(row) = row
        {
            RowStorage.write(txn, t, k, row, None)?;
            txn.put(&changed, &[])?;
        }
        RowStorage.remove(txn, t, &moved(t, k, DELTA))?;
    }
    Ok(items.len())
}

/// Each segment of `t`: its id, rows, rows deleted, and the encoded bytes
/// of its columns (`nc_catalog.segments`).
pub fn segment_info(txn: &dyn KvTxn, t: &Table) -> Result<Vec<(u64, usize, usize, usize)>> {
    let start = t.index_prefix(0);
    let end = prefix_end(&start);
    let mut out = Vec::new();
    for seg in segments(txn, t, &start, end.as_deref())? {
        let gone = deleted(txn, t, &seg, false)?.len();
        let mut bytes = 0;
        for c in (0..seg.stats.len() as u16).chain([KEYS]) {
            bytes += txn.get(&chunk_key(t, seg.id, c))?.map_or(0, |b| b.len());
        }
        out.push((seg.id, seg.rows, gone, bytes));
    }
    Ok(out)
}

fn vector_len(v: &Vector) -> usize {
    match v {
        Vector::Const(_) => 0,
        Vector::Int(d, _) | Vector::Time(_, d, _) => d.len(),
        Vector::Float(d, _) => d.len(),
        Vector::Dec(_, d, _) => d.len(),
        Vector::Bool(d, _) => d.len(),
        Vector::Text(o, _, _) | Vector::Char(o, _, _) => o.len() - 1,
        Vector::Values(v) => v.len(),
        Vector::Dict(_, c, _) => c.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The marks' search finds what a search over the keys alone does:
    /// keys sharing their first 8 bytes in runs past a mark, keys shorter
    /// than 8 bytes (a prefix's zeros), probes between, before and after.
    #[test]
    fn marked_search_is_the_plain_one() {
        let mut keys: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..700 {
            // Runs of 100 keys with one 8-byte prefix, then short ones.
            let mut k = vec![1, 2, 3, 4, (i / 100) as u8, 0, 0, 7];
            k.extend_from_slice(&i.to_be_bytes());
            keys.push(k);
        }
        for i in 0u8..90 {
            keys.push(vec![9, i]);
            keys.push(vec![9, i, 0]);
        }
        keys.sort();
        keys.dedup();
        let refs: Vec<&[u8]> = keys.iter().map(Vec::as_slice).collect();
        let k = Keys::decode(&Keys::encode(&refs)).unwrap();
        assert_eq!(k.marks.len(), keys.len().div_ceil(MARK));
        let mut probes: Vec<Vec<u8>> = keys.clone();
        for key in &keys {
            let mut more = key.clone();
            more.push(0);
            probes.push(more);
            let mut less = key.clone();
            less.pop();
            probes.push(less);
        }
        probes.extend([vec![], vec![0], vec![255; 12], vec![1, 2, 3, 4, 3]]);
        for p in probes {
            let want = keys.partition_point(|x| x.as_slice() < p.as_slice());
            assert_eq!(k.search(&p), want, "{p:?}");
        }
        let empty = Keys::decode(&Keys::encode(&[])).unwrap();
        assert_eq!(empty.search(b"x"), 0);
    }
}
