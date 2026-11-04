//! Work too large for memory (PLAN.md §M14.11 L6): what an operator holds
//! past its share of memory (`work_mem`) goes to scratch space — the KV
//! contract's ([`nucleus_kv::Kv::scratch`]) — and is read back from there.
//!
//! - [`Run`]: rows written in order, sealed, then read any number of times,
//!   whole or by byte ranges, from any thread.
//! - [`Stash`]: rows kept in memory up to the limit, the rest in a run (a
//!   materialised result: a CTE, the rows a write collects first).
//! - [`sort`]: an external merge sort — sorted runs of a memory's worth,
//!   merged (a pass at a time when there are many); bounded by a `LIMIT`,
//!   a heap of the rows it takes ([`Top`]).
//!
//! Rows are encoded with the row encoding's values (`encoding`), except
//! enum values, which keep their label: a run is read back exactly as it
//! was written.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use nucleus_kv::Scratch;

use crate::encoding::{Reader, decode_value, encode_value};
use crate::error::{Error, Result};
use crate::types::{Row, Value};

/// A run's buffer: bytes written (and read) at a time.
const BLOCK: usize = 64 << 10;
/// Runs merged at once; more are merged in passes.
const FAN_IN: usize = 64;
/// A value tag of this codec's own: an enum value, by its index in the
/// run's table of them.
const ENUM: u8 = 0xF0;
/// Its array tag (the row encoding's), so arrays of enums keep labels.
const ARRAY: u8 = 10;

fn storage(e: nucleus_kv::Error) -> Error {
    Error::internal(format!("scratch space: {e}"))
}

/// Scratch space from the database the statement runs on (in memory when
/// none is known).
fn scratch() -> Result<Box<dyn Scratch>> {
    match crate::datetime::current_kv() {
        Some(kv) => kv.scratch().map_err(storage),
        None => Ok(Box::new(nucleus_kv::MemoryScratch::default())),
    }
}

/// About how much memory a row takes.
pub fn row_bytes(row: &[Value]) -> usize {
    24 + row.iter().map(value_bytes).sum::<usize>()
}

/// [`row_bytes`] of a row of one integer.
pub(crate) const INT_KEY_BYTES: usize = 24 + 32;

pub(crate) fn value_bytes(v: &Value) -> usize {
    32 + match v {
        Value::Text(s) | Value::Char(s) => s.len(),
        Value::Bytea(b) | Value::Ext(_, b) => b.len(),
        Value::Array(items) => items.iter().map(value_bytes).sum(),
        Value::Enum(e) => e.key.len() + e.label.len(),
        _ => 0,
    }
}

/// Enum values seen by a run, by index (their labels are not in the row
/// encoding).
#[derive(Default)]
struct Enums {
    values: Vec<Value>,
    index: HashMap<(std::sync::Arc<[u8]>, std::sync::Arc<str>), u32>,
}

fn encode(out: &mut Vec<u8>, v: &Value, enums: &mut Enums) {
    match v {
        Value::Enum(e) => {
            let key = (e.key.clone(), e.label.clone());
            let next = enums.values.len() as u32;
            let i = *enums.index.entry(key).or_insert_with(|| {
                enums.values.push(v.clone());
                next
            });
            out.push(ENUM);
            out.extend_from_slice(&i.to_le_bytes());
        }
        Value::Array(items) => {
            out.push(ARRAY);
            out.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for item in items {
                encode(out, item, enums);
            }
        }
        v => encode_value(out, v),
    }
}

fn decode(r: &mut Reader, enums: &[Value]) -> Result<Value> {
    let bad = |_| Error::internal("malformed scratch row");
    match r.peek() {
        Some(ENUM) => {
            r.u8().map_err(bad)?;
            let i = r.u32().map_err(bad)? as usize;
            enums
                .get(i)
                .cloned()
                .ok_or_else(|| Error::internal("malformed scratch row"))
        }
        Some(ARRAY) => {
            r.u8().map_err(bad)?;
            let n = r.u32().map_err(bad)? as usize;
            let mut items = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                items.push(decode(r, enums)?);
            }
            Ok(Value::Array(items))
        }
        _ => decode_value(r),
    }
}

/// Rows being written to scratch space, in order.
pub struct Run {
    scratch: Box<dyn Scratch>,
    /// Bytes written to scratch space.
    len: u64,
    rows: u64,
    buf: Vec<u8>,
    enums: Enums,
}

impl Run {
    /// An empty run.
    pub fn new() -> Result<Self> {
        Ok(Run {
            scratch: scratch()?,
            len: 0,
            rows: 0,
            buf: Vec::with_capacity(BLOCK),
            enums: Enums::default(),
        })
    }

    /// Appends a row.
    pub fn push(&mut self, row: &[Value]) -> Result<()> {
        let at = self.buf.len();
        self.buf.extend_from_slice(&[0; 4]);
        self.buf
            .extend_from_slice(&(row.len() as u32).to_le_bytes());
        for v in row {
            encode(&mut self.buf, v, &mut self.enums);
        }
        let n = (self.buf.len() - at - 4) as u32;
        self.buf[at..at + 4].copy_from_slice(&n.to_le_bytes());
        self.rows += 1;
        if self.buf.len() >= BLOCK {
            self.flush()?;
        }
        Ok(())
    }

    /// Appends a record of bytes of the caller's own layout: a run holds
    /// rows ([`push`](Self::push)) or records, read back by
    /// [`RunIter::next_record`] — for sorts of bytes (an index build's
    /// entries) that need no values.
    pub fn push_record(&mut self, parts: &[&[u8]]) -> Result<()> {
        let n: usize = parts.iter().map(|p| p.len()).sum();
        self.buf.extend_from_slice(&(n as u32).to_le_bytes());
        for p in parts {
            self.buf.extend_from_slice(p);
        }
        self.rows += 1;
        if self.buf.len() >= BLOCK {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if !self.buf.is_empty() {
            self.scratch
                .write_at(self.len, &self.buf)
                .map_err(storage)?;
            self.len += self.buf.len() as u64;
            self.buf.clear();
        }
        Ok(())
    }

    /// The rows written.
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Where the next row starts: with an earlier one, the byte range of
    /// the rows between ([`Sealed::range`]).
    pub fn at(&self) -> u64 {
        self.len + self.buf.len() as u64
    }

    /// Done writing: the run to read.
    pub fn seal(mut self) -> Result<Arc<Sealed>> {
        self.flush()?;
        Ok(Arc::new(Sealed {
            scratch: Mutex::new(self.scratch),
            len: self.len,
            rows: self.rows,
            enums: self.enums.values,
        }))
    }
}

/// A run, written: read it with [`Sealed::iter`] or by ranges, as often as
/// wanted and from any thread (a block's read holds its lock).
pub struct Sealed {
    scratch: Mutex<Box<dyn Scratch>>,
    len: u64,
    rows: u64,
    enums: Vec<Value>,
}

impl Sealed {
    /// Its rows.
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Its rows, from the first.
    pub fn iter(self: &Arc<Self>) -> RunIter {
        self.range(0, self.len)
    }

    /// Its rows from byte `from` to `to` — where they started and ended
    /// as it was written ([`Run::at`]).
    pub fn range(self: &Arc<Self>, from: u64, to: u64) -> RunIter {
        RunIter {
            run: Arc::clone(self),
            at: from,
            end: to.min(self.len),
            block: Vec::new(),
            pos: 0,
        }
    }
}

/// A run's rows, read a block at a time.
pub struct RunIter {
    run: Arc<Sealed>,
    /// Where `block` starts in scratch space.
    at: u64,
    /// Where its rows end.
    end: u64,
    block: Vec<u8>,
    pos: usize,
}

impl RunIter {
    /// At least `n` bytes from `pos` in `block`, or none left.
    fn fill(&mut self, n: usize) -> Result<bool> {
        if self.block.len() - self.pos >= n {
            return Ok(true);
        }
        let start = self.at + self.pos as u64;
        let left = self.end.saturating_sub(start);
        if left < n as u64 {
            return Ok(false);
        }
        let take = (left as usize).min(BLOCK.max(n));
        let mut block = vec![0u8; take];
        self.run
            .scratch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .read_at(start, &mut block)
            .map_err(storage)?;
        self.at = start;
        self.block = block;
        self.pos = 0;
        Ok(true)
    }

    /// The next record of a run of records ([`Run::push_record`]), valid
    /// until the next call.
    pub fn next_record(&mut self) -> Result<Option<&[u8]>> {
        if !self.fill(4)? {
            return Ok(None);
        }
        let n = u32::from_le_bytes(self.block[self.pos..self.pos + 4].try_into().unwrap()) as usize;
        if !self.fill(4 + n)? {
            return Err(Error::internal("a scratch run ends inside a record"));
        }
        let at = self.pos + 4;
        self.pos = at + n;
        Ok(Some(&self.block[at..at + n]))
    }

    fn next_row(&mut self) -> Result<Option<Row>> {
        if !self.fill(4)? {
            return Ok(None);
        }
        let n = u32::from_le_bytes(self.block[self.pos..self.pos + 4].try_into().unwrap()) as usize;
        if !self.fill(4 + n)? {
            return Err(Error::internal("a scratch run ends inside a row"));
        }
        let bytes = &self.block[self.pos + 4..self.pos + 4 + n];
        let mut r = Reader::new(bytes);
        let bad = |_| Error::internal("malformed scratch row");
        let width = r.u32().map_err(bad)? as usize;
        let mut row = Vec::with_capacity(width);
        for _ in 0..width {
            row.push(decode(&mut r, &self.run.enums)?);
        }
        self.pos += 4 + n;
        Ok(Some(row))
    }
}

impl Iterator for RunIter {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_row().transpose()
    }
}

/// Rows kept in memory up to the statement's `work_mem`, the rest in a
/// run, in order.
pub struct Stash {
    mem: Vec<Row>,
    bytes: usize,
    limit: usize,
    run: Option<Run>,
}

impl Default for Stash {
    fn default() -> Self {
        Self::new()
    }
}

impl Stash {
    /// An empty stash, with the statement's memory limit.
    pub fn new() -> Self {
        Stash {
            mem: Vec::new(),
            bytes: 0,
            limit: crate::budget::work_mem(),
            run: None,
        }
    }

    /// Adds a row.
    pub fn push(&mut self, row: Row) -> Result<()> {
        if let Some(run) = &mut self.run {
            return run.push(&row);
        }
        self.bytes += row_bytes(&row);
        self.mem.push(row);
        if self.bytes > self.limit {
            let mut run = Run::new()?;
            for r in self.mem.drain(..) {
                run.push(&r)?;
            }
            self.bytes = 0;
            self.run = Some(run);
        }
        Ok(())
    }

    /// The rows, to read.
    pub fn seal(self) -> Result<Stored> {
        Ok(match self.run {
            None => Stored::Memory(self.mem),
            Some(run) => Stored::Run(run.seal()?),
        })
    }
}

/// Rows stashed ([`Stash::seal`]): in memory, or in a run.
pub enum Stored {
    /// In memory.
    Memory(Vec<Row>),
    /// In scratch space.
    Run(Arc<Sealed>),
}

impl Stored {
    /// How many.
    pub fn len(&self) -> usize {
        match self {
            Stored::Memory(rows) => rows.len(),
            Stored::Run(run) => run.rows() as usize,
        }
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The rows, in order (cloned from memory).
    pub fn iter(self: &Rc<Self>) -> Box<dyn Iterator<Item = Result<Row>>> {
        match &**self {
            Stored::Memory(_) => {
                let rows = Rc::clone(self);
                Box::new((0..rows.len()).map(move |i| match &*rows {
                    Stored::Memory(r) => Ok(r[i].clone()),
                    Stored::Run(_) => unreachable!("a memory stash"),
                }))
            }
            Stored::Run(run) => Box::new(run.iter()),
        }
    }
}

impl IntoIterator for Stored {
    type Item = Result<Row>;
    type IntoIter = Box<dyn Iterator<Item = Result<Row>>>;

    /// The rows, in order, consuming them.
    fn into_iter(self) -> Self::IntoIter {
        match self {
            Stored::Memory(rows) => Box::new(rows.into_iter().map(Ok)),
            Stored::Run(run) => Box::new(run.iter()),
        }
    }
}

/// Partitions an operator spreads the input past its memory over.
pub const FAN_OUT: usize = 32;
/// Partitioning levels at most: past them, a partition is taken in
/// memory whatever its size (only a huge number of equal keys gets there).
pub const MAX_LEVEL: u32 = 4;

/// A hash of `key` for partitioning at `level` (each level hashes
/// differently, so a partition splits again).
pub fn hash_at(level: u32, key: &[Value]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    level.hash(&mut h);
    key.hash(&mut h);
    h.finish()
}

/// Rows spread over [`FAN_OUT`] runs by a hash of their keys: the part of
/// an operator's input past its memory, each partition then taken on its
/// own (rows with equal keys share one).
pub struct Partitions {
    runs: Vec<Option<Run>>,
    level: u32,
}

impl Partitions {
    /// Empty partitions at `level`.
    pub fn new(level: u32) -> Self {
        Partitions {
            runs: (0..FAN_OUT).map(|_| None).collect(),
            level,
        }
    }

    /// Adds `row`, by `key`.
    pub fn push(&mut self, key: &[Value], row: &[Value]) -> Result<()> {
        let i = (hash_at(self.level, key) % FAN_OUT as u64) as usize;
        if self.runs[i].is_none() {
            self.runs[i] = Some(Run::new()?);
        }
        self.runs[i].as_mut().expect("just made").push(row)
    }

    /// The partitions with rows.
    pub fn seal(self) -> Result<Vec<Arc<Sealed>>> {
        self.runs.into_iter().flatten().map(Run::seal).collect()
    }

    /// Every partition, by number (`None`: no rows) — to pair with another
    /// side's partitions of the same level.
    pub fn seal_all(self) -> Result<Vec<Option<Arc<Sealed>>>> {
        self.runs
            .into_iter()
            .map(|r| r.map(Run::seal).transpose())
            .collect()
    }
}

/// `pairs` sorted by their keys as [`compare`] orders them, ties in the
/// order they came (a stable sort) — by normalized keys when it can: each
/// row's keys as one byte string whose bytewise order is theirs (DuckDB's
/// and the System R line's normalized keys; here the index keys'
/// encoding, `encoding::encode_key_value`, each value after a byte that
/// places a `NULL` first or last, a descending key's bytes inverted), so
/// a comparison is one `memcmp`.  Only when every key column holds values
/// of one kind whose encoding orders as `compare` does (integers, text,
/// `char(n)`, dates, times, timestamps, booleans; `NULL`s anywhere) —
/// not doubles (`NaN`, `-0`), decimals or mixed kinds.
pub(crate) fn sort_keyed<R>(flags: &[(bool, bool)], pairs: &mut Vec<(Vec<Value>, R)>) {
    const NORMALIZED_FROM: usize = 64;
    if pairs.len() >= NORMALIZED_FROM
        && let Some((bytes, ends)) = normalized(flags, pairs)
    {
        let key = |i: u32| {
            let i = i as usize;
            let start = if i == 0 { 0 } else { ends[i - 1] as usize };
            &bytes[start..ends[i] as usize]
        };
        let mut order: Vec<u32> = (0..pairs.len() as u32).collect();
        order.sort_by(|&a, &b| key(a).cmp(key(b)));
        let mut taken: Vec<Option<(Vec<Value>, R)>> = pairs.drain(..).map(Some).collect();
        pairs.extend(
            order
                .iter()
                .map(|&i| taken[i as usize].take().expect("each once")),
        );
        return;
    }
    pairs.sort_by(|(a, _), (b, _)| compare(flags, a, b));
}

/// The normalized keys of `pairs` ([`sort_keyed`]): their bytes one after
/// another and where each ends — `None` when a column's kinds do not
/// allow it.
fn normalized<R>(flags: &[(bool, bool)], pairs: &[(Vec<Value>, R)]) -> Option<(Vec<u8>, Vec<u32>)> {
    fn kind(v: &Value) -> Option<u8> {
        Some(match v {
            Value::Int(_) => 1,
            Value::Text(_) => 2,
            Value::Char(_) => 3,
            Value::Date(_) => 4,
            Value::Timestamp(_) => 5,
            Value::TimestampTz(_) => 6,
            Value::Time(_) => 7,
            Value::Bool(_) => 8,
            _ => return None,
        })
    }
    for j in 0..flags.len() {
        let mut seen = None;
        for (keys, _) in pairs {
            let v = &keys[j];
            if v.is_null() {
                continue;
            }
            let k = kind(v)?;
            match seen {
                None => seen = Some(k),
                Some(s) if s == k => {}
                Some(_) => return None,
            }
        }
    }
    let mut bytes = Vec::with_capacity(pairs.len() * (flags.len() * 10));
    let mut ends = Vec::with_capacity(pairs.len());
    for (keys, _) in pairs {
        for (v, &(desc, nulls_first)) in keys.iter().zip(flags) {
            if v.is_null() {
                bytes.push(if nulls_first { 0x00 } else { 0x02 });
                continue;
            }
            bytes.push(0x01);
            let at = bytes.len();
            crate::encoding::encode_key_value(&mut bytes, v);
            if desc {
                bytes[at..].iter_mut().for_each(|b| *b = !*b);
            }
        }
        ends.push(u32::try_from(bytes.len()).ok()?);
    }
    Some((bytes, ends))
}

/// The order of sort key values under `flags` (descending, NULLs first),
/// one per key.
pub fn compare(flags: &[(bool, bool)], a: &[Value], b: &[Value]) -> Ordering {
    for (i, &(desc, nulls_first)) in flags.iter().enumerate() {
        let (x, y) = (&a[i], &b[i]);
        let o = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            _ => {
                let o = x.sort_cmp(y);
                if desc { o.reverse() } else { o }
            }
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// A row read from one of the runs a merge reads: its sort keys first.
struct Head {
    /// Keys, then the row.
    row: Row,
    /// The run it came from: ties keep the runs' order.
    from: usize,
    flags: Rc<[(bool, bool)]>,
}

impl PartialEq for Head {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Head {}

impl PartialOrd for Head {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Head {
    // Reversed: the heap's greatest is the merge's next.
    fn cmp(&self, other: &Self) -> Ordering {
        compare(&self.flags, &self.row, &other.row)
            .then(self.from.cmp(&other.from))
            .reverse()
    }
}

/// Runs of (keys ++ row), each sorted, merged into one order.
struct Merge {
    runs: Vec<RunIter>,
    heap: BinaryHeap<Head>,
    flags: Rc<[(bool, bool)]>,
    width: usize,
}

impl Merge {
    fn new(runs: &[Arc<Sealed>], flags: Rc<[(bool, bool)]>, width: usize) -> Result<Self> {
        let mut m = Merge {
            runs: runs.iter().map(Sealed::iter).collect(),
            heap: BinaryHeap::new(),
            flags,
            width,
        };
        for i in 0..m.runs.len() {
            m.advance(i)?;
        }
        Ok(m)
    }

    fn advance(&mut self, i: usize) -> Result<()> {
        if let Some(row) = self.runs[i].next_row()? {
            self.heap.push(Head {
                row,
                from: i,
                flags: Rc::clone(&self.flags),
            });
        }
        Ok(())
    }

    /// The next (keys ++ row).
    fn next_keyed(&mut self) -> Result<Option<Row>> {
        let Some(head) = self.heap.pop() else {
            return Ok(None);
        };
        self.advance(head.from)?;
        Ok(Some(head.row))
    }
}

impl Iterator for Merge {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        let keys = self.width;
        self.next_keyed()
            .map(|r| r.map(|mut row| row.split_off(keys)))
            .transpose()
    }
}

/// The least `k` (keys, row) pairs under `flags` of those given, ties in
/// the order given — the first `k` of [`sort`]'s stable order — kept in a
/// heap of `k` (PostgreSQL's bounded sort, `tuplesort_set_bound`): a pair
/// past the heap's greatest is turned away by one comparison
/// ([`admits`](Self::admits)), before its row need be built.  As
/// PostgreSQL's, the pairs are only gathered up to `2k` — fewer are
/// sorted as they are — and then the least `k` become the heap.
pub struct Top {
    k: usize,
    flags: Rc<[(bool, bool)]>,
    /// The pairs gathered, then (once `heaped`) a max-heap by (keys, the
    /// order given).
    heap: Vec<(Vec<Value>, u64, Row)>,
    heaped: bool,
    next: u64,
    bytes: usize,
}

impl Top {
    /// Room for `k` pairs.
    pub fn new(k: usize, flags: Rc<[(bool, bool)]>) -> Self {
        Top {
            k,
            flags,
            heap: Vec::with_capacity(k.min(1 << 16)),
            heaped: false,
            next: 0,
            bytes: 0,
        }
    }

    /// Whether a pair with keys `keys`, given now, is among the least `k`
    /// so far.
    pub fn admits(&self, keys: &[Value]) -> bool {
        !self.heaped
            || (self.k > 0 && compare(&self.flags, keys, &self.heap[0].0) == Ordering::Less)
    }

    /// Takes a pair it [`admits`](Self::admits).
    pub fn push(&mut self, keys: Vec<Value>, row: Row) {
        let at = self.next;
        self.next += 1;
        self.bytes += row_bytes(&keys) + row_bytes(&row);
        if !self.heaped {
            self.heap.push((keys, at, row));
            if self.heap.len() >= 2 * self.k.max(1) {
                // The least k, greatest first: a max-heap as it is.
                let flags = Rc::clone(&self.flags);
                self.heap
                    .sort_by(|a, b| compare(&flags, &b.0, &a.0).then(b.1.cmp(&a.1)));
                let cut = self.heap.len() - self.k;
                for (k, _, r) in self.heap.drain(..cut) {
                    self.bytes -= row_bytes(&k) + row_bytes(&r);
                }
                self.heaped = true;
            }
            return;
        }
        // In place of the greatest, sifted down.
        let old = std::mem::replace(&mut self.heap[0], (keys, at, row));
        self.bytes -= row_bytes(&old.0) + row_bytes(&old.2);
        let (n, mut i) = (self.heap.len(), 0);
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut top = i;
            if l < n && self.before(top, l) {
                top = l;
            }
            if r < n && self.before(top, r) {
                top = r;
            }
            if top == i {
                break;
            }
            self.heap.swap(i, top);
            i = top;
        }
    }

    /// Whether pair `a` comes before pair `b`.
    fn before(&self, a: usize, b: usize) -> bool {
        let (x, y) = (&self.heap[a], &self.heap[b]);
        compare(&self.flags, &x.0, &y.0).then(x.1.cmp(&y.1)) == Ordering::Less
    }

    /// About how much memory its pairs take.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Its pairs in order.
    pub fn into_sorted(self) -> Vec<(Vec<Value>, Row)> {
        let flags = self.flags;
        let mut heap = self.heap;
        heap.sort_by(|a, b| compare(&flags, &a.0, &b.0).then(a.1.cmp(&b.1)));
        heap.truncate(self.k);
        heap.into_iter().map(|(k, _, r)| (k, r)).collect()
    }

    /// Its pairs in the order given.
    fn into_given(self) -> Vec<(Vec<Value>, Row)> {
        let mut heap = self.heap;
        heap.sort_by_key(|p| p.1);
        heap.into_iter().map(|(k, _, r)| (k, r)).collect()
    }
}

/// Sorts `(keys, row)` pairs by `flags`: in memory while they fit in
/// `work_mem`, else by sorted runs merged.  The rows, in order — with
/// `bound`, only the first `bound` (a `LIMIT`'s rows and those it skips),
/// kept in a [`Top`] while it fits in `work_mem`.
pub fn sort(
    rows: impl Iterator<Item = Result<(Vec<Value>, Row)>>,
    flags: Vec<(bool, bool)>,
    bound: Option<usize>,
) -> Result<Box<dyn Iterator<Item = Result<Row>>>> {
    let Some(k) = bound else {
        return sort_all(rows, flags);
    };
    let limit = crate::budget::work_mem();
    let mut rows = rows;
    let mut top = Top::new(k, flags.clone().into());
    for r in rows.by_ref() {
        let (keys, row) = r?;
        if top.admits(&keys) {
            top.push(keys, row);
            if top.bytes() > limit {
                // Too large for a heap in memory: sorted with the rest
                // (those it turned away are not among the first `k`).
                let given = top.into_given();
                return sort_all(given.into_iter().map(Ok).chain(rows), flags);
            }
        }
    }
    Ok(Box::new(top.into_sorted().into_iter().map(|(_, r)| Ok(r))))
}

/// [`sort`] without a bound.
fn sort_all(
    rows: impl Iterator<Item = Result<(Vec<Value>, Row)>>,
    flags: Vec<(bool, bool)>,
) -> Result<Box<dyn Iterator<Item = Result<Row>>>> {
    let limit = crate::budget::work_mem();
    let flags: Rc<[(bool, bool)]> = flags.into();
    let width = flags.len();
    let mut buffer: Vec<(Vec<Value>, Row)> = Vec::new();
    let mut bytes = 0;
    let mut runs: Vec<Arc<Sealed>> = Vec::new();
    let write = |buffer: &mut Vec<(Vec<Value>, Row)>| -> Result<Arc<Sealed>> {
        sort_keyed(&flags, buffer);
        let mut run = Run::new()?;
        for (keys, row) in buffer.drain(..) {
            let mut keyed = keys;
            keyed.extend(row);
            run.push(&keyed)?;
        }
        run.seal()
    };
    for r in rows {
        let (keys, row) = r?;
        bytes += row_bytes(&keys) + row_bytes(&row);
        buffer.push((keys, row));
        if bytes > limit {
            runs.push(write(&mut buffer)?);
            bytes = 0;
        }
    }
    if runs.is_empty() {
        sort_keyed(&flags, &mut buffer);
        return Ok(Box::new(buffer.into_iter().map(|(_, r)| Ok(r))));
    }
    if !buffer.is_empty() {
        runs.push(write(&mut buffer)?);
    }
    // Many runs: merged in passes, FAN_IN at a time, into longer runs.
    while runs.len() > FAN_IN {
        let mut merged = Vec::new();
        for group in runs.chunks(FAN_IN) {
            let mut m = Merge::new(group, Rc::clone(&flags), width)?;
            let mut run = Run::new()?;
            while let Some(keyed) = m.next_keyed()? {
                run.push(&keyed)?;
            }
            merged.push(run.seal()?);
        }
        runs = merged;
    }
    Ok(Box::new(Merge::new(&runs, flags, width)?))
}

/// Rows already sorted in parts (a parallel sort's morsels, in scan order)
/// merged into one order: in memory when they fit in work_mem, else each
/// part a run, merged as [`sort`] merges its runs.  Ties go to the earlier
/// part, so the order is a stable sort's of the parts one after another.
pub fn merge_sorted(
    parts: Vec<Vec<(Vec<Value>, Row)>>,
    flags: Vec<(bool, bool)>,
) -> Result<Box<dyn Iterator<Item = Result<Row>>>> {
    let bytes: usize = parts
        .iter()
        .flatten()
        .map(|(k, r)| row_bytes(k) + row_bytes(r))
        .sum();
    let flags: Rc<[(bool, bool)]> = flags.into();
    if bytes <= crate::budget::work_mem() {
        let mut parts: Vec<std::vec::IntoIter<(Vec<Value>, Row)>> =
            parts.into_iter().map(Vec::into_iter).collect();
        let mut heap = std::collections::BinaryHeap::new();
        let mut rows: Vec<Option<Row>> = vec![None; parts.len()];
        for (i, p) in parts.iter_mut().enumerate() {
            if let Some((keys, row)) = p.next() {
                rows[i] = Some(row);
                heap.push(PartHead {
                    keys,
                    part: i,
                    flags: Rc::clone(&flags),
                });
            }
        }
        return Ok(Box::new(std::iter::from_fn(move || {
            let head = heap.pop()?;
            let row = rows[head.part].take().expect("a head's row");
            if let Some((keys, next)) = parts[head.part].next() {
                rows[head.part] = Some(next);
                heap.push(PartHead {
                    keys,
                    part: head.part,
                    flags: Rc::clone(&head.flags),
                });
            }
            Some(Ok(row))
        })));
    }
    let width = flags.len();
    let mut runs = Vec::new();
    for part in parts {
        let mut run = Run::new()?;
        for (keys, row) in part {
            let mut keyed = keys;
            keyed.extend(row);
            run.push(&keyed)?;
        }
        runs.push(run.seal()?);
    }
    while runs.len() > FAN_IN {
        let mut merged = Vec::new();
        for group in runs.chunks(FAN_IN) {
            let mut m = Merge::new(group, Rc::clone(&flags), width)?;
            let mut run = Run::new()?;
            while let Some(keyed) = m.next_keyed()? {
                run.push(&keyed)?;
            }
            merged.push(run.seal()?);
        }
        runs = merged;
    }
    Ok(Box::new(Merge::new(&runs, flags, width)?))
}

/// A part's first row's keys, ordered for [`merge_sorted`]'s heap (the
/// smallest, then the earliest part, on top).
struct PartHead {
    keys: Vec<Value>,
    part: usize,
    flags: Rc<[(bool, bool)]>,
}

impl PartialEq for PartHead {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for PartHead {}

impl PartialOrd for PartHead {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PartHead {
    fn cmp(&self, other: &Self) -> Ordering {
        // A max-heap: reversed.
        compare(&self.flags, &other.keys, &self.keys).then(other.part.cmp(&self.part))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_keys_sort_as_compare_does() {
        // Random keys of every kind normalized keys take — texts that are
        // prefixes of each other, with zero bytes, padded char(n), NULLs —
        // under every direction and NULL placement, and kinds they do not
        // take (falling back): the order of a stable sort by `compare`.
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let texts = ["", "a", "ab", "abc", "a\0", "a\0b", "b", "\0", "ab\0", "zz"];
        for round in 0..40 {
            let kinds = [round % 9, (round / 9) % 9];
            let value = |k: u64, r: u64| -> Value {
                if r.is_multiple_of(7) {
                    return Value::Null;
                }
                match k {
                    0 => Value::Int(
                        (r % 11) as i64 - 5
                            + if r.is_multiple_of(13) {
                                i64::MIN / 2
                            } else {
                                0
                            },
                    ),
                    1 => Value::Text(texts[(r % 10) as usize].to_string()),
                    2 => Value::Char(format!("{:<4}", texts[(r % 6) as usize])),
                    3 => Value::Date((r % 9) as i32 - 4),
                    4 => Value::Timestamp((r % 9) as i64 * 1000 - 3000),
                    5 => Value::Bool(r.is_multiple_of(2)),
                    6 => Value::Time((r % 5) as i64),
                    7 => Value::Float((r % 5) as f64 - 2.0),
                    _ => {
                        if r.is_multiple_of(2) {
                            Value::Int((r % 5) as i64)
                        } else {
                            Value::Float((r % 9) as f64 / 2.0 - 1.0)
                        }
                    }
                }
            };
            let pairs: Vec<(Vec<Value>, usize)> = (0..300)
                .map(|i| {
                    let keys = kinds.iter().map(|&k| value(k, next())).collect();
                    (keys, i)
                })
                .collect();
            for flags in [
                [(false, false), (false, false)],
                [(true, false), (false, true)],
                [(false, true), (true, true)],
                [(true, true), (true, false)],
            ] {
                let mut want = pairs.clone();
                want.sort_by(|(a, _), (b, _)| compare(&flags, a, b));
                let mut got = pairs.clone();
                sort_keyed(&flags, &mut got);
                let order = |v: &[(Vec<Value>, usize)]| v.iter().map(|p| p.1).collect::<Vec<_>>();
                assert_eq!(
                    order(&got),
                    order(&want),
                    "kinds {kinds:?}, flags {flags:?}"
                );
            }
        }
    }

    #[test]
    fn a_run_reads_back_what_was_written() {
        let rows: Vec<Row> = (0..20_000)
            .map(|i| {
                vec![
                    Value::Int(i),
                    Value::Text("x".repeat((i % 50) as usize)),
                    if i % 3 == 0 {
                        Value::Null
                    } else {
                        Value::Float(i as f64 / 3.0)
                    },
                    Value::Array(vec![Value::Int(i), Value::Null]),
                ]
            })
            .collect();
        let mut run = Run::new().unwrap();
        for r in &rows {
            run.push(r).unwrap();
        }
        let sealed = run.seal().unwrap();
        assert_eq!(sealed.rows(), 20_000);
        for _ in 0..2 {
            let back: Vec<Row> = sealed.iter().map(Result::unwrap).collect();
            assert_eq!(back, rows);
        }
    }

    #[test]
    fn an_external_sort_is_a_sort() {
        let n = 30_000i64;
        let rows = (0..n).map(|i| {
            let k = (i * 7919) % n;
            Ok((
                vec![Value::Int(k % 100), Value::Int(k)],
                vec![Value::Int(k), Value::Text(format!("r{k}"))],
            ))
        });
        let got: Vec<i64> = crate::budget::with_budget(
            crate::budget::Budget {
                work_mem: Some(64 << 10),
                ..Default::default()
            },
            || {
                sort(rows, vec![(true, false), (false, false)], None)
                    .unwrap()
                    .map(|r| match r.unwrap()[0] {
                        Value::Int(k) => k,
                        _ => unreachable!(),
                    })
                    .collect()
            },
        );
        let mut want: Vec<i64> = (0..n).collect();
        want.sort_by(|a, b| (b % 100).cmp(&(a % 100)).then(a.cmp(b)));
        assert_eq!(got, want);
    }
}
