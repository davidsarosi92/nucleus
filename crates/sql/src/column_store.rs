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
use std::sync::{Arc, Mutex, OnceLock};

use nucleus_kv::{KvTxn, prefix_end};

use crate::catalog::Table;
use crate::encoding::{Reader, Writer, decode_value};
use crate::enums::Labels;
use crate::error::{Error, Result};
use crate::storage::{BatchSource, ColumnBound, RowIter, RowStorage, Scan, TableStorage};
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
/// The column number of a segment's row keys.
const KEYS: u16 = u16::MAX;

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

fn marker_prefix(t: &Table, seg: u64) -> Vec<u8> {
    let mut k = t.index_prefix(DELETED);
    k.extend_from_slice(&seg.to_be_bytes());
    k
}

fn marker_key(t: &Table, seg: u64, row: u32) -> Vec<u8> {
    let mut k = marker_prefix(t, seg);
    k.extend_from_slice(&row.to_be_bytes());
    k
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
    /// Per column: least, greatest, `NULL`s.
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
        Ok(Segment {
            id,
            first,
            last: last.to_vec(),
            rows,
            stats,
        })
    }

    /// Whether `bounds` rule out every row of it.
    fn excluded(&self, bounds: &[ColumnBound]) -> bool {
        bounds.iter().any(|b| {
            let Some((lo, hi, nulls)) = self.stats.get(b.column) else {
                return false;
            };
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

/// The segment whose key range holds `key`, if any.
fn segment_of(txn: &dyn KvTxn, t: &Table, key: &[u8]) -> Result<Option<Segment>> {
    let p = t.index_prefix(DIRECTORY);
    let from = dir_key(t, key);
    let to = prefix_end(&p).expect("directory prefix");
    let Some(e) = txn
        .scan(Bound::Included(&from), Bound::Excluded(&to))?
        .next()
    else {
        return Ok(None);
    };
    let (k, v) = e?;
    let seg = Segment::decode(&k[p.len()..], &v)?;
    Ok((seg.first.as_slice() <= key).then_some(seg))
}

// ------------------------------------------------------------------ cache

/// A segment's row keys (suffixes after the table's primary index prefix).
#[derive(Debug)]
struct Keys {
    offsets: Vec<u32>,
    bytes: Vec<u8>,
}

impl Keys {
    fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    fn get(&self, i: usize) -> &[u8] {
        &self.bytes[self.offsets[i] as usize..self.offsets[i + 1] as usize]
    }

    /// The first row whose key is `>= suffix`.
    fn search(&self, suffix: &[u8]) -> usize {
        let (mut lo, mut hi) = (0, self.len());
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
        Ok(Keys { offsets, bytes })
    }
}

/// A decoded column, cached.
#[derive(Clone)]
enum Decoded {
    Keys(Arc<Keys>),
    Column(Arc<Vector>),
}

/// Decoded columns by (segment id, column): a segment never changes, so
/// they stay right.  Bounded; emptied when full.
fn cache() -> &'static Mutex<(HashMap<(u64, u16), Decoded>, usize)> {
    static CACHE: OnceLock<Mutex<(HashMap<(u64, u16), Decoded>, usize)>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new((HashMap::new(), 0)))
}

const CACHE_ROWS: usize = 64 * SEGMENT_ROWS;

fn cached(txn: &dyn KvTxn, t: &Table, seg: &Segment, col: u16) -> Result<Decoded> {
    if let Some(d) = cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .0
        .get(&(seg.id, col))
    {
        return Ok(d.clone());
    }
    let b = txn
        .get(&chunk_key(t, seg.id, col))?
        .ok_or_else(|| bad("a column is missing"))?;
    let d = if col == KEYS {
        Decoded::Keys(Arc::new(Keys::decode(&b)?))
    } else {
        Decoded::Column(Arc::new(crate::segment::decode(&b)?))
    };
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    if c.1 + seg.rows > CACHE_ROWS {
        c.0.clear();
        c.1 = 0;
    }
    c.1 += seg.rows;
    c.0.insert((seg.id, col), d.clone());
    Ok(d)
}

fn keys_of(txn: &dyn KvTxn, t: &Table, seg: &Segment) -> Result<Arc<Keys>> {
    match cached(txn, t, seg, KEYS)? {
        Decoded::Keys(k) if k.len() == seg.rows => Ok(k),
        _ => Err(bad("row keys")),
    }
}

fn column_of(txn: &dyn KvTxn, t: &Table, seg: &Segment, col: usize) -> Result<Arc<Vector>> {
    if col >= seg.stats.len() {
        // A column added after the segment was written: NULL.
        return Ok(Arc::new(Vector::Const(Value::Null)));
    }
    match cached(txn, t, seg, col as u16)? {
        Decoded::Column(v) => Ok(v),
        Decoded::Keys(_) => Err(bad("a column")),
    }
}

/// The rows of `seg` marked deleted.
fn deleted(txn: &dyn KvTxn, t: &Table, seg: &Segment, validated: bool) -> Result<BTreeSet<u32>> {
    let p = marker_prefix(t, seg.id);
    let end = prefix_end(&p).expect("marker prefix");
    let scan = if validated {
        txn.scan_validated(Bound::Included(&p), Bound::Excluded(&end))?
    } else {
        txn.scan(Bound::Included(&p), Bound::Excluded(&end))?
    };
    let mut out = BTreeSet::new();
    for e in scan {
        let (k, _) = e?;
        let row = k[p.len()..]
            .try_into()
            .map(u32::from_be_bytes)
            .map_err(|_| bad("a marker"))?;
        out.insert(row);
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
) -> Result<Option<(Segment, usize)>> {
    let Some(seg) = segment_of(txn, t, key)? else {
        return Ok(None);
    };
    let prefix = t.index_prefix(0).len();
    let keys = keys_of(txn, t, &seg)?;
    let i = keys.search(&key[prefix..]);
    if i == keys.len() || keys.get(i) != &key[prefix..] {
        return Ok(None);
    }
    let marker = marker_key(t, seg.id, i as u32);
    let gone = if validated {
        // Also: the segment is still there at commit.
        txn.get_validated(&dir_key(t, &seg.last))?;
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
            row.push(column_of(txn, t, seg, c)?.value(i));
        }
    }
    Ok(row)
}

/// Marks the live segment row under `key` deleted (if there is one);
/// whether there was.
fn mark(txn: &mut dyn KvTxn, t: &Table, key: &[u8]) -> Result<bool> {
    let Some((seg, i)) = locate(&*txn, t, key, true)? else {
        return Ok(false);
    };
    txn.put(&marker_key(t, seg.id, i as u32), &[])?;
    Ok(true)
}

// ------------------------------------------------------------------ writes

/// Rows a statement inserts are gathered (by table) until it ends — or
/// until this many, when they go to segments at once — and go to segments
/// when they are many ([`MERGE_ROWS`]), else to the delta store: a load
/// never passes through the delta store (whose deleted rows stay as
/// versions until a vacuum).
const BUFFER_ROWS: usize = 4 * SEGMENT_ROWS;

thread_local! {
    /// Rows each table's statement wrote so far.
    static WRITTEN: std::cell::RefCell<HashMap<u32, usize>> = std::cell::RefCell::new(HashMap::new());
    /// The rows each table's statement inserted, not yet stored.
    static BUFFER: std::cell::RefCell<HashMap<u32, std::collections::BTreeMap<Vec<u8>, Row>>> =
        std::cell::RefCell::new(HashMap::new());
}

fn note_written(t: &Table) {
    WRITTEN.with(|w| *w.borrow_mut().entry(t.id).or_default() += 1);
}

fn buffered<T>(t: &Table, f: impl FnOnce(&mut std::collections::BTreeMap<Vec<u8>, Row>) -> T) -> T {
    BUFFER.with(|b| f(b.borrow_mut().entry(t.id).or_default()))
}

fn take_buffer(t: &Table) -> std::collections::BTreeMap<Vec<u8>, Row> {
    BUFFER.with(|b| b.borrow_mut().remove(&t.id).unwrap_or_default())
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

/// Writes `rows` (keyed, ascending) as one segment.
fn write_segment(txn: &mut dyn KvTxn, t: &Table, rows: &[(Vec<u8>, Row)]) -> Result<()> {
    let prefix = t.index_prefix(0).len();
    let n = t.columns.len();
    let suffixes: Vec<&[u8]> = rows.iter().map(|(k, _)| &k[prefix..]).collect();
    let mut chunks = vec![Keys::encode(&suffixes)];
    let mut stats = Vec::with_capacity(n);
    for c in 0..n {
        let col: Vec<Value> = rows
            .iter()
            .map(|(_, r)| r.get(c).cloned().unwrap_or(Value::Null))
            .collect();
        stats.push(crate::segment::stats(&col));
        chunks.push(crate::segment::encode(&col));
    }
    let id = segment_id(t, &chunks);
    txn.put(&chunk_key(t, id, KEYS), &chunks[0])?;
    for (c, chunk) in chunks[1..].iter().enumerate() {
        txn.put(&chunk_key(t, id, c as u16), chunk)?;
    }
    let seg = Segment {
        id,
        first: rows[0].0.clone(),
        last: rows[rows.len() - 1].0.clone(),
        rows: rows.len(),
        stats,
    };
    txn.put(&dir_key(t, &seg.last), &seg.encode())?;
    Ok(())
}

/// Removes `seg`: its columns, its markers (read validated: a delete
/// meanwhile conflicts), its directory entry.
fn drop_segment(txn: &mut dyn KvTxn, t: &Table, seg: &Segment) -> Result<()> {
    for row in deleted(&*txn, t, seg, true)? {
        txn.delete(&marker_key(t, seg.id, row))?;
    }
    txn.delete(&chunk_key(t, seg.id, KEYS))?;
    for c in 0..seg.stats.len() {
        txn.delete(&chunk_key(t, seg.id, c as u16))?;
    }
    txn.delete(&dir_key(t, &seg.last))?;
    Ok(())
}

/// Segments being written, in key order: rows gathered until a segment is
/// full.
struct SegmentWriter<'t> {
    t: &'t Table,
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
            write_segment(txn, self.t, &self.rows)?;
            self.rows.clear();
        }
        self.bytes = 0;
        Ok(())
    }
}

/// The rows a merge takes in, in key order: the delta store's (copied
/// aside) and a statement's buffered ones.
struct Incoming {
    delta: std::iter::Peekable<Box<dyn Iterator<Item = Result<Row>>>>,
    extra: std::iter::Peekable<std::collections::btree_map::IntoIter<Vec<u8>, Row>>,
}

impl Incoming {
    fn delta_key(&mut self) -> Result<Option<Vec<u8>>> {
        match self.delta.peek() {
            None => Ok(None),
            Some(Ok(row)) => match row.first() {
                Some(Value::Bytea(k)) => Ok(Some(k.to_vec())),
                _ => Err(Error::internal("a stashed row without its key")),
            },
            Some(Err(_)) => Err(self.delta.next().expect("peeked").expect_err("an error")),
        }
    }

    /// The next row's key.
    fn peek(&mut self) -> Result<Option<Vec<u8>>> {
        let d = self.delta_key()?;
        let e = self.extra.peek().map(|(k, _)| k.clone());
        Ok(match (d, e) {
            (Some(d), Some(e)) => Some(d.min(e)),
            (d, e) => d.or(e),
        })
    }

    /// The next row, and whether it is the delta store's.
    fn next(&mut self) -> Result<Option<(Vec<u8>, Row, bool)>> {
        let d = self.delta_key()?;
        let e = self.extra.peek().map(|(k, _)| k.clone());
        let from_delta = match (&d, &e) {
            (None, None) => return Ok(None),
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some(d), Some(e)) => d <= e,
        };
        if from_delta {
            let (k, row) = crate::exec::unkeyed(self.delta.next().expect("peeked")?)?;
            Ok(Some((k, row, true)))
        } else {
            let (k, row) = self.extra.next().expect("peeked");
            Ok(Some((k, row, false)))
        }
    }
}

/// Merges the delta store — and `extra`, rows a statement inserted — into
/// segments: the segments their keys fall among rewritten with them,
/// deleted rows dropped, the delta store emptied.
pub fn merge(
    txn: &mut dyn KvTxn,
    t: &Table,
    extra: std::collections::BTreeMap<Vec<u8>, Row>,
) -> Result<()> {
    let start = t.index_prefix(0);
    let end = prefix_end(&start);
    // The delta store, copied aside (past work_mem to scratch space), and
    // the first and last keys coming in.
    let mut stash = crate::spill::Stash::new();
    let mut range: Option<(Vec<u8>, Vec<u8>)> = None;
    let mut widen = |k: &[u8]| {
        range = Some(match range.take() {
            None => (k.to_vec(), k.to_vec()),
            Some((f, l)) => (f.min(k.to_vec()), l.max(k.to_vec())),
        });
    };
    for r in RowStorage.rows(Scan {
        txn: &*txn,
        table: t,
        start: start.clone(),
        end,
        wanted: None,
        validated: false,
        bounds: Vec::new(),
    })? {
        let (k, row) = r?;
        widen(&k);
        stash.push(crate::exec::keyed(k, row))?;
    }
    if let (Some((f, _)), Some((l, _))) = (extra.first_key_value(), extra.last_key_value()) {
        widen(f);
        widen(l);
    }
    let Some((lo, hi)) = range else {
        return Ok(());
    };
    let old: Vec<Segment> = segments(&*txn, t, &lo, None)?
        .into_iter()
        .take_while(|s| s.first <= hi)
        .collect();
    let mut incoming = Incoming {
        delta: stash.seal()?.into_iter().peekable(),
        extra: extra.into_iter().peekable(),
    };
    let mut out = SegmentWriter {
        t,
        rows: Vec::new(),
        bytes: 0,
    };
    // A delta row moves out of the delta store as it goes in.
    let take = |txn: &mut dyn KvTxn,
                out: &mut SegmentWriter,
                (k, row, from_delta): (Vec<u8>, Row, bool)|
     -> Result<()> {
        if from_delta {
            RowStorage.remove(txn, t, &k)?;
        }
        out.push(txn, k, row)
    };
    for seg in &old {
        // The old segment, read whole, goes before anything is written
        // over it (a new segment may end at its last key).
        let keys = keys_of(&*txn, t, seg)?;
        let cols = (0..t.columns.len())
            .map(|c| column_of(&*txn, t, seg, c))
            .collect::<Result<Vec<_>>>()?;
        let gone = deleted(&*txn, t, seg, true)?;
        drop_segment(txn, t, seg)?;
        for i in 0..seg.rows {
            let mut key = start.clone();
            key.extend_from_slice(keys.get(i));
            while incoming.peek()?.is_some_and(|k| k < key) {
                let r = incoming.next()?.expect("peeked");
                take(txn, &mut out, r)?;
            }
            if incoming.peek()?.is_some_and(|k| k == key) {
                let r = incoming.next()?.expect("peeked");
                take(txn, &mut out, r)?; // the new row replaces it
                continue;
            }
            if gone.contains(&(i as u32)) {
                continue;
            }
            let row = cols.iter().map(|v| v.value(i)).collect();
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
    wanted: Option<&'a [bool]>,
    validated: bool,
    end: Option<Vec<u8>>,
    segs: VecDeque<Segment>,
    delta: std::iter::Peekable<RowIter<'a>>,
    /// The segment being read: it, its keys, its deleted rows, where.
    cur: Option<(Rc<Segment>, Arc<Keys>, BTreeSet<u32>, usize, usize)>,
    prefix: Vec<u8>,
}

impl<'a> Merged<'a> {
    fn new(scan: Scan<'a>) -> Result<Self> {
        let Scan {
            txn,
            table: t,
            start,
            end,
            wanted,
            validated,
            bounds,
        } = scan;
        let segs = segments(txn, t, &start, end.as_deref())?
            .into_iter()
            .filter(|s| !s.excluded(&bounds))
            .collect();
        let delta = RowStorage
            .rows(Scan {
                txn,
                table: t,
                start: start.clone(),
                end: end.clone(),
                wanted,
                validated,
                bounds: Vec::new(),
            })?
            .peekable();
        let mut m = Merged {
            txn,
            t,
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
                        Some(r) => rows.push(r?),
                        None => break,
                    }
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
                        rows.push(self.delta.next().expect("peeked")?);
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
                // The delta row comes now; the segment row of its key (if
                // any) is skipped.
                let kr = self.delta.next().expect("peeked")?;
                if replaced && let Some(c) = &mut self.cur {
                    c.3 = pos + 1;
                }
                return Ok(Some(Run::Delta(vec![kr])));
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

/// The columnar storage.
pub struct ColumnStorage;

impl TableStorage for ColumnStorage {
    fn name(&self) -> &str {
        COLUMN
    }

    fn index_ids(&self) -> &[u32] {
        &[0, crate::large::LARGE_INDEX, SEGMENTS, DELETED, DIRECTORY]
    }

    fn write(
        &self,
        txn: &mut dyn KvTxn,
        t: &Table,
        key: &[u8],
        row: &Row,
        old: Option<&Row>,
    ) -> Result<()> {
        note_written(t);
        if old.is_none() || buffered(t, |b| b.contains_key(key)) {
            // An insert, or a change of one: gathered for the statement's
            // end, and stored in segments when they are many.
            let full = buffered(t, |b| {
                b.insert(key.to_vec(), row.clone());
                b.len() >= BUFFER_ROWS
            });
            if full {
                merge(txn, t, take_buffer(t))?;
            }
            return Ok(());
        }
        if txn.get(key)?.is_some() {
            return RowStorage.write(txn, t, key, row, old); // a delta row
        }
        // A segment's row: marked deleted, its new version gathered as an
        // insert is.
        mark(txn, t, key)?;
        let full = buffered(t, |b| {
            b.insert(key.to_vec(), row.clone());
            b.len() >= BUFFER_ROWS
        });
        if full {
            merge(txn, t, take_buffer(t))?;
        }
        Ok(())
    }

    fn remove(&self, txn: &mut dyn KvTxn, t: &Table, key: &[u8]) -> Result<()> {
        if buffered(t, |b| b.remove(key).is_some()) {
            return Ok(());
        }
        if txn.get(key)?.is_some() {
            RowStorage.remove(txn, t, key)?;
        }
        mark(txn, t, key)?;
        Ok(())
    }

    fn exists(&self, txn: &dyn KvTxn, t: &Table, key: &[u8], validated: bool) -> Result<bool> {
        Ok(buffered(t, |b| b.contains_key(key))
            || RowStorage.exists(txn, t, key, validated)?
            || locate(txn, t, key, validated)?.is_some())
    }

    fn get(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        key: &[u8],
        wanted: Option<&[bool]>,
        labels: &mut Labels,
    ) -> Result<Option<Row>> {
        if let Some(row) = buffered(t, |b| b.get(key).cloned()) {
            return Ok(Some(row));
        }
        if let Some(row) = RowStorage.get(txn, t, key, wanted, labels)? {
            return Ok(Some(row));
        }
        match locate(txn, t, key, false)? {
            None => Ok(None),
            Some((seg, i)) => Ok(Some(segment_row(txn, t, &seg, i, wanted)?)),
        }
    }

    fn rows<'a>(&self, scan: Scan<'a>) -> Result<RowIter<'a>> {
        let mut m = Merged::new(scan)?;
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
                        let read = || -> Result<_> {
                            let keys = keys_of(m.txn, m.t, &seg)?;
                            let cols = (0..m.t.columns.len())
                                .map(|c| {
                                    if m.wanted
                                        .is_some_and(|w| !w.get(c).copied().unwrap_or(false))
                                    {
                                        Ok(None)
                                    } else {
                                        column_of(m.txn, m.t, &seg, c).map(Some)
                                    }
                                })
                                .collect::<Result<Vec<_>>>()?;
                            Ok((keys, cols))
                        };
                        let (keys, cols) = match read() {
                            Ok(x) => x,
                            Err(e) => return Some(Err(e)),
                        };
                        for i in live {
                            let i = i as usize;
                            let row = cols
                                .iter()
                                .map(|v| v.as_ref().map_or(Value::Null, |v| v.value(i)))
                                .collect();
                            buf.push_back((m.key_at(&keys, i), row));
                        }
                    }
                }
            }
        })))
    }

    fn batches<'a>(&self, scan: Scan<'a>) -> Result<Box<dyn BatchSource + 'a>> {
        Ok(Box::new(Merged::new(scan)?))
    }

    fn split_points(
        &self,
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

    fn after_writes(&self, txn: &mut dyn KvTxn, t: &Table) -> Result<()> {
        let written = WRITTEN.with(|w| w.borrow_mut().remove(&t.id).unwrap_or(0));
        let rows = take_buffer(t);
        if written == 0 && rows.is_empty() {
            return Ok(());
        }
        let start = t.index_prefix(0);
        let end = prefix_end(&start);
        // Many rows this statement, or a delta store grown large in the
        // store (the KV splits it): to segments.  Else the rows inserted
        // wait in the delta store.
        if rows.len() >= MERGE_ROWS
            || written >= MERGE_ROWS
            || !txn.split_points(&start, end.as_deref(), 2)?.is_empty()
        {
            return merge(txn, t, rows);
        }
        for (k, row) in rows {
            RowStorage.write(txn, t, &k, &row, None)?;
        }
        Ok(())
    }

    fn after_failure(&self, t: &Table) {
        WRITTEN.with(|w| w.borrow_mut().remove(&t.id));
        take_buffer(t);
    }

    fn check(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        problems: &mut Vec<String>,
    ) -> Result<Vec<(Vec<u8>, Row)>> {
        let delta = crate::check::row_storage(txn, t, problems)?;
        let start = t.index_prefix(0);
        let end = prefix_end(&start);
        let mut live: Vec<(Vec<u8>, Row)> = Vec::new();
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
            for c in 0..seg.stats.len() {
                match column_of(txn, t, &seg, c) {
                    Ok(v) if matches!(*v, Vector::Const(_)) || vector_len(&v) == seg.rows => {}
                    Ok(_) => problems.push(format!(
                        "{}: segment {:x} column {c} has the wrong length",
                        t.name, seg.id
                    )),
                    Err(e) => {
                        problems.push(format!("{}: segment {:x} column {c}: {e}", t.name, seg.id))
                    }
                }
            }
            let gone = deleted(txn, t, &seg, false)?;
            if gone.iter().any(|&r| r as usize >= seg.rows) {
                problems.push(format!(
                    "{}: segment {:x} marks rows it lacks",
                    t.name, seg.id
                ));
            }
            for i in 0..seg.rows {
                if !gone.contains(&(i as u32)) {
                    live.push((full(i), segment_row(txn, t, &seg, i, None)?));
                }
            }
        }
        let in_segments: BTreeSet<&Vec<u8>> = live.iter().map(|(k, _)| k).collect();
        for (k, _) in &delta {
            if in_segments.contains(k) {
                problems.push(format!(
                    "{}: row under {k:?} both in a segment and in the delta store",
                    t.name
                ));
            }
        }
        live.extend(delta);
        live.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(live)
    }
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
        Vector::Bool(d, _) => d.len(),
        Vector::Text(o, _, _) => o.len() - 1,
        Vector::Values(v) => v.len(),
    }
}
