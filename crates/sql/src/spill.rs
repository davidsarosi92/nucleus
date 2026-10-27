//! Work too large for memory (PLAN.md §M14.11 L6): what an operator holds
//! past its share of memory (`work_mem`) goes to scratch space — the KV
//! contract's ([`nucleus_kv::Kv::scratch`]) — and is read back from there.
//!
//! - [`Run`]: rows written in order, sealed, then read any number of times.
//! - [`Stash`]: rows kept in memory up to the limit, the rest in a run (a
//!   materialised result: a CTE, the rows a write collects first).
//! - [`sort`]: an external merge sort — sorted runs of a memory's worth,
//!   merged (a pass at a time when there are many).
//!
//! Rows are encoded with the row encoding's values (`encoding`), except
//! enum values, which keep their label: a run is read back exactly as it
//! was written.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::rc::Rc;

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

fn value_bytes(v: &Value) -> usize {
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

    /// Done writing: the run to read.
    pub fn seal(mut self) -> Result<Rc<Sealed>> {
        self.flush()?;
        Ok(Rc::new(Sealed {
            scratch: self.scratch,
            len: self.len,
            rows: self.rows,
            enums: self.enums.values,
        }))
    }
}

/// A run, written: read it with [`Sealed::iter`], as often as wanted.
pub struct Sealed {
    scratch: Box<dyn Scratch>,
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
    pub fn iter(self: &Rc<Self>) -> RunIter {
        RunIter {
            run: Rc::clone(self),
            at: 0,
            block: Vec::new(),
            pos: 0,
        }
    }
}

/// A run's rows, read a block at a time.
pub struct RunIter {
    run: Rc<Sealed>,
    /// Where `block` starts in scratch space.
    at: u64,
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
        let left = self.run.len - start;
        if left < n as u64 {
            return Ok(false);
        }
        let take = (left as usize).min(BLOCK.max(n));
        let mut block = vec![0u8; take];
        self.run
            .scratch
            .read_at(start, &mut block)
            .map_err(storage)?;
        self.at = start;
        self.block = block;
        self.pos = 0;
        Ok(true)
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
    Run(Rc<Sealed>),
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
    pub fn seal(self) -> Result<Vec<Rc<Sealed>>> {
        self.runs.into_iter().flatten().map(Run::seal).collect()
    }

    /// Every partition, by number (`None`: no rows) — to pair with another
    /// side's partitions of the same level.
    pub fn seal_all(self) -> Result<Vec<Option<Rc<Sealed>>>> {
        self.runs
            .into_iter()
            .map(|r| r.map(Run::seal).transpose())
            .collect()
    }
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
    fn new(runs: &[Rc<Sealed>], flags: Rc<[(bool, bool)]>, width: usize) -> Result<Self> {
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

/// Sorts `(keys, row)` pairs by `flags`: in memory while they fit in
/// `work_mem`, else by sorted runs merged.  The rows, in order.
pub fn sort(
    rows: impl Iterator<Item = Result<(Vec<Value>, Row)>>,
    flags: Vec<(bool, bool)>,
) -> Result<Box<dyn Iterator<Item = Result<Row>>>> {
    let limit = crate::budget::work_mem();
    let flags: Rc<[(bool, bool)]> = flags.into();
    let width = flags.len();
    let mut buffer: Vec<(Vec<Value>, Row)> = Vec::new();
    let mut bytes = 0;
    let mut runs: Vec<Rc<Sealed>> = Vec::new();
    let write = |buffer: &mut Vec<(Vec<Value>, Row)>| -> Result<Rc<Sealed>> {
        buffer.sort_by(|(a, _), (b, _)| compare(&flags, a, b));
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
        buffer.sort_by(|(a, _), (b, _)| compare(&flags, a, b));
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

#[cfg(test)]
mod tests {
    use super::*;

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
                sort(rows, vec![(true, false), (false, false)])
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
