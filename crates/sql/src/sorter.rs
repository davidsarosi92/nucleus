//! Sorting rows (`ORDER BY`, the windows' sorts): rows taken as records
//! of bytes, sorted a chunk at a time, merged in parallel by key ranges.
//!
//! - A row and its sort keys become one record: the keys' *normalized*
//!   bytes — whose bytewise order is the keys' order under their
//!   directions and `NULL` placements (System R's normalized keys, as
//!   DuckDB's) —, the keys' values, the row's values (the scratch codec).
//!   A record is a few dozen bytes where a row of values is hundreds, so
//!   `work_mem` holds several times the rows, and comparing two is a
//!   `memcmp` (its first eight bytes kept beside it as an integer:
//!   PostgreSQL's abbreviated keys).  Where a key column's values are of
//!   kinds whose normalized bytes do not order as [`compare`] does (or of
//!   more than one kind), its records are compared by their key values.
//! - A [`Sorter`] fills a chunk of records, sorts it (a stable sort: ties
//!   keep their input order), and keeps it as a [`Part`] — in memory while
//!   the sort's parts fit in its `work_mem` ([`Budget`]), else written to
//!   scratch space with a sparse index of its keys.  A parallel sort runs
//!   a sorter a morsel.
//! - [`merge`] merges the parts: cut into key ranges by splitters drawn
//!   from the parts' keys (each part's share of a range found by binary
//!   search, or through its index), the ranges merged on worker threads a
//!   few ahead of the reader, so the merge — once one thread's — runs in
//!   parallel and the rows come out in order (Merge Path's idea, Odeh et
//!   al.; DuckDB's parallel merge sort).  Ties go to the earlier part.

use std::cmp::Ordering;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Condvar, Mutex};

use crate::encoding::Reader;
use crate::error::{Error, Result};
use crate::spill::{Enums, RunIter, Sealed, compare, decode, encode};
use crate::types::{Row, Value};

/// A record's first bytes: its normalized key's length, its key values'.
const HEAD: usize = 8;
/// A run on disk keeps every this many records' keys and places.
const INDEX_EVERY: usize = 128;
/// Ranges merged ahead of the reader, a worker.
const AHEAD: usize = 2;
/// Fewer records than this are merged on the reader's thread.
const PARALLEL_FROM: usize = if cfg!(test) { 64 } else { 1 << 15 };
/// The fewest records a range of a parallel merge is cut to, about.
const RANGE_ROWS: usize = if cfg!(test) { 8 } else { 1024 };
/// More parts than this are merged a group at a time first.
const FAN_IN: usize = 512;

/// A sort's memory: its `work_mem`, the bytes its parts in memory hold,
/// and the size of a sorter's chunk.
pub struct Budget {
    limit: usize,
    chunk: usize,
    resident: AtomicUsize,
}

impl Budget {
    /// The statement's `work_mem`, shared by `sorters` sorters.
    pub fn new(sorters: usize) -> Arc<Budget> {
        let limit = crate::budget::work_mem();
        Arc::new(Budget {
            limit,
            chunk: (limit / (2 * sorters.max(1))).max(4 << 10),
            resident: AtomicUsize::new(0),
        })
    }
}

/// A key column's kinds of values so far: whether normalized bytes order
/// them.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Unseen,
    One(u8),
    Mixed,
}

/// The kinds whose normalized bytes order as [`compare`] does.
fn kind_of(v: &Value) -> Option<u8> {
    Some(match v {
        Value::Bool(_) => 1,
        Value::Int(_) => 2,
        Value::Float(_) | Value::Real(_) => 3,
        Value::Numeric(_) => 4,
        Value::Text(_) => 5,
        Value::Char(_) => 6,
        Value::Date(_) => 7,
        Value::Time(_) => 8,
        Value::Timestamp(_) => 9,
        Value::TimestampTz(_) => 10,
        Value::Bytea(_) => 11,
        _ => return None,
    })
}

#[derive(Clone, Debug)]
struct Kinds(Vec<Kind>);

impl Kinds {
    fn new(n: usize) -> Kinds {
        Kinds(vec![Kind::Unseen; n])
    }

    fn see(&mut self, keys: &[Value]) {
        for (k, v) in self.0.iter_mut().zip(keys) {
            if v.is_null() || *k == Kind::Mixed {
                continue;
            }
            *k = match (kind_of(v), *k) {
                (None, _) => Kind::Mixed,
                (Some(x), Kind::Unseen) => Kind::One(x),
                (Some(x), Kind::One(y)) if x == y => continue,
                _ => Kind::Mixed,
            };
        }
    }

    fn join(&mut self, o: &Kinds) {
        for (k, &x) in self.0.iter_mut().zip(&o.0) {
            *k = match (*k, x) {
                (a, Kind::Unseen) => a,
                (Kind::Unseen, b) => b,
                (Kind::One(a), Kind::One(b)) if a == b => Kind::One(a),
                _ => Kind::Mixed,
            };
        }
    }

    fn normalized(&self) -> bool {
        self.0.iter().all(|k| *k != Kind::Mixed)
    }
}

/// Appends the record of `row` sorted by `keys`.
fn encode_record(
    out: &mut Vec<u8>,
    flags: &[(bool, bool)],
    keys: &[Value],
    row: &[Value],
    enums: &mut Enums,
) {
    let at = out.len();
    out.extend_from_slice(&[0; HEAD]);
    for (v, &(desc, nulls_first)) in keys.iter().zip(flags) {
        if v.is_null() {
            out.push(if nulls_first { 0x00 } else { 0x02 });
            continue;
        }
        out.push(0x01);
        let from = out.len();
        crate::encoding::encode_key_value(out, v);
        if desc {
            out[from..].iter_mut().for_each(|b| *b = !*b);
        }
    }
    let norm = out.len() - at - HEAD;
    for v in keys {
        encode(out, v, enums);
    }
    let kl = out.len() - at - HEAD - norm;
    out.extend_from_slice(&(row.len() as u32).to_le_bytes());
    for v in row {
        encode(out, v, enums);
    }
    out[at..at + 4].copy_from_slice(&(norm as u32).to_le_bytes());
    out[at + 4..at + 8].copy_from_slice(&(kl as u32).to_le_bytes());
}

/// A record's normalized key.
fn norm(rec: &[u8]) -> &[u8] {
    let n = u32::from_le_bytes(rec[0..4].try_into().unwrap()) as usize;
    &rec[HEAD..HEAD + n]
}

/// A record's normalized key and key values (what a disk part's index
/// keeps), with its head.
fn key_part(rec: &[u8]) -> &[u8] {
    let n = u32::from_le_bytes(rec[0..4].try_into().unwrap()) as usize;
    let k = u32::from_le_bytes(rec[4..8].try_into().unwrap()) as usize;
    &rec[..HEAD + n + k]
}

/// A normalized key's first eight bytes as an integer (zeros after its
/// end): comparing two compares them first.
fn prefix(norm: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    let n = norm.len().min(8);
    b[..n].copy_from_slice(&norm[..n]);
    u64::from_be_bytes(b)
}

fn decode_keys(rec: &[u8], width: usize, enums: &[Value]) -> Result<Vec<Value>> {
    let n = u32::from_le_bytes(rec[0..4].try_into().unwrap()) as usize;
    let mut r = Reader::new(&rec[HEAD + n..]);
    (0..width).map(|_| decode(&mut r, enums)).collect()
}

fn decode_row(rec: &[u8], enums: &[Value]) -> Result<Row> {
    let at = key_part(rec).len();
    let mut r = Reader::new(&rec[at..]);
    let bad = |_| Error::internal("malformed sort record");
    let width = r.u32().map_err(bad)? as usize;
    let mut row = Vec::with_capacity(width);
    for _ in 0..width {
        row.push(decode(&mut r, enums)?);
    }
    Ok(row)
}

/// A record in a chunk: where it starts, its length, its key's prefix.
#[derive(Clone, Copy)]
struct Ent {
    at: usize,
    len: u32,
    pre: u64,
}

/// A sorted part of a sort's records.
pub struct Part {
    data: Data,
    enums: Vec<Value>,
    kinds: Kinds,
    rows: usize,
}

enum Data {
    Memory {
        bytes: Vec<u8>,
        ents: Vec<Ent>,
    },
    /// In scratch space; every [`INDEX_EVERY`]th record's place and key
    /// part.
    Disk {
        run: Arc<Sealed>,
        index: Vec<(u64, Vec<u8>)>,
        end: u64,
    },
}

/// Rows and their keys, taken a chunk at a time into sorted parts.
pub struct Sorter {
    flags: Arc<[(bool, bool)]>,
    budget: Arc<Budget>,
    bytes: Vec<u8>,
    ents: Vec<Ent>,
    enums: Enums,
    kinds: Kinds,
    parts: Vec<Arc<Part>>,
}

impl Sorter {
    /// An empty sorter of keys under `flags` (descending, `NULL`s first),
    /// its parts in memory as `budget` allows.
    pub fn new(flags: Arc<[(bool, bool)]>, budget: Arc<Budget>) -> Sorter {
        let n = flags.len();
        Sorter {
            flags,
            budget,
            bytes: Vec::new(),
            ents: Vec::new(),
            enums: Enums::default(),
            kinds: Kinds::new(n),
            parts: Vec::new(),
        }
    }

    /// Takes `row`, sorted by `keys`.
    pub fn push(&mut self, keys: &[Value], row: &[Value]) -> Result<()> {
        self.kinds.see(keys);
        let at = self.bytes.len();
        encode_record(&mut self.bytes, &self.flags, keys, row, &mut self.enums);
        let len = (self.bytes.len() - at) as u32;
        let pre = prefix(norm(&self.bytes[at..]));
        self.ents.push(Ent { at, len, pre });
        if self.bytes.len() + 24 * self.ents.len() >= self.budget.chunk {
            self.seal()?;
        }
        Ok(())
    }

    /// The chunk sorted, kept as a part.
    fn seal(&mut self) -> Result<()> {
        if self.ents.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut self.bytes);
        let mut ents = std::mem::take(&mut self.ents);
        let enums = std::mem::take(&mut self.enums).values;
        let kinds = std::mem::replace(&mut self.kinds, Kinds::new(self.flags.len()));
        let rec = |e: &Ent| &bytes[e.at..e.at + e.len as usize];
        if kinds.normalized() {
            ents.sort_by(|a, b| {
                a.pre
                    .cmp(&b.pre)
                    .then_with(|| norm(rec(a)).cmp(norm(rec(b))))
            });
        } else {
            let width = self.flags.len();
            let keys = ents
                .iter()
                .map(|e| decode_keys(rec(e), width, &enums))
                .collect::<Result<Vec<_>>>()?;
            let mut order: Vec<usize> = (0..ents.len()).collect();
            order.sort_by(|&a, &b| compare(&self.flags, &keys[a], &keys[b]));
            ents = order.into_iter().map(|i| ents[i]).collect();
        }
        let rows = ents.len();
        let size = bytes.len() + 24 * rows;
        let fits = self
            .budget
            .resident
            .try_update(SeqCst, SeqCst, |r| {
                (r + size <= self.budget.limit).then_some(r + size)
            })
            .is_ok();
        let data = if fits {
            Data::Memory { bytes, ents }
        } else {
            let mut run = crate::spill::Run::new()?;
            let mut index = Vec::with_capacity(rows / INDEX_EVERY + 1);
            for (i, e) in ents.iter().enumerate() {
                let r = rec(e);
                if i % INDEX_EVERY == 0 {
                    index.push((run.at(), key_part(r).to_vec()));
                }
                run.push_record(&[r])?;
            }
            let end = run.at();
            Data::Disk {
                run: run.seal()?,
                index,
                end,
            }
        };
        self.parts.push(Arc::new(Part {
            data,
            enums,
            kinds,
            rows,
        }));
        Ok(())
    }

    /// Its parts, in the order they were taken.
    pub fn finish(mut self) -> Result<Vec<Arc<Part>>> {
        self.seal()?;
        Ok(self.parts)
    }
}

/// A key to compare records with: a splitter, or a record's.
#[derive(Clone)]
struct Key {
    norm: Vec<u8>,
    /// Its values, when the merge compares values.
    values: Vec<Value>,
}

/// How a merge compares records: by normalized bytes, or by key values.
struct Cmp {
    flags: Arc<[(bool, bool)]>,
    normalized: bool,
}

impl Cmp {
    fn keys(&self, a: (&[u8], &[Value]), b: (&[u8], &[Value])) -> Ordering {
        if self.normalized {
            a.0.cmp(b.0)
        } else {
            compare(&self.flags, a.1, b.1)
        }
    }

    /// The key of `rec` (a record or a key part) of `part`.
    fn key_of(&self, rec: &[u8], part: &Part) -> Result<Key> {
        Ok(Key {
            norm: norm(rec).to_vec(),
            values: if self.normalized {
                Vec::new()
            } else {
                decode_keys(rec, self.flags.len(), &part.enums)?
            },
        })
    }
}

/// A part's records from one place to another, read in order.
struct Cursor {
    part: Arc<Part>,
    src: Src,
    /// The current record's key values (compared by values).
    values: Vec<Value>,
    /// Records from this key on are the next range's.
    hi: Option<Arc<Key>>,
    done: bool,
}

enum Src {
    Memory { i: usize, end: usize },
    Disk { it: RunIter, rec: Vec<u8> },
}

impl Cursor {
    fn record(&self) -> &[u8] {
        match &self.src {
            Src::Memory { i, .. } => {
                let Data::Memory { bytes, ents } = &self.part.data else {
                    unreachable!("a memory cursor over a memory part")
                };
                let e = ents[*i];
                &bytes[e.at..e.at + e.len as usize]
            }
            Src::Disk { rec, .. } => rec,
        }
    }

    /// Reads the current record's key (compared by values).
    fn load(&mut self, cmp: &Cmp) -> Result<()> {
        if !cmp.normalized && !self.done {
            self.values = decode_keys(self.record(), cmp.flags.len(), &self.part.enums)?;
        }
        Ok(())
    }

    /// To the next record.
    fn advance(&mut self, cmp: &Cmp) -> Result<()> {
        match &mut self.src {
            Src::Memory { i, end } => {
                *i += 1;
                if *i >= *end {
                    self.done = true;
                    return Ok(());
                }
            }
            Src::Disk { it, rec } => match it.next_record()? {
                Some(r) => {
                    rec.clear();
                    rec.extend_from_slice(r);
                }
                None => {
                    self.done = true;
                    return Ok(());
                }
            },
        }
        self.load(cmp)?;
        self.check_hi(cmp);
        Ok(())
    }

    fn check_hi(&mut self, cmp: &Cmp) {
        if let (Some(hi), false) = (&self.hi, self.done) {
            let key = (norm(self.record()), &self.values[..]);
            if cmp.keys(key, (&hi.norm, &hi.values)) != Ordering::Less {
                self.done = true;
            }
        }
    }

    fn key(&self) -> (&[u8], &[Value]) {
        (norm(self.record()), &self.values)
    }
}

/// The first record of a memory part not below `k`.
fn lower_bound(cmp: &Cmp, part: &Part, k: &Key) -> Result<usize> {
    let Data::Memory { bytes, ents } = &part.data else {
        unreachable!("a memory part")
    };
    let (mut lo, mut hi) = (0, ents.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let e = ents[mid];
        let rec = &bytes[e.at..e.at + e.len as usize];
        let below = if cmp.normalized {
            norm(rec) < &k.norm[..]
        } else {
            let v = decode_keys(rec, cmp.flags.len(), &part.enums)?;
            compare(&cmp.flags, &v, &k.values) == Ordering::Less
        };
        if below {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

/// The place in a disk part's index to start reading for keys not below
/// `k` (`None`: from the start), and to stop by for keys below `hi`.
fn disk_bounds(cmp: &Cmp, part: &Part, lo: Option<&Key>, hi: Option<&Key>) -> Result<(u64, u64)> {
    let Data::Disk { index, end, .. } = &part.data else {
        unreachable!("a disk part")
    };
    // The last indexed record below the key: none before it is not.
    let below = |k: &Key| -> Result<usize> {
        let (mut lo, mut hi) = (0, index.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let key = cmp.key_of(&index[mid].1, part)?;
            if cmp.keys((&key.norm, &key.values), (&k.norm, &k.values)) == Ordering::Less {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    };
    let from = match lo {
        Some(k) => {
            let i = below(k)?;
            if i == 0 { 0 } else { index[i - 1].0 }
        }
        None => 0,
    };
    let to = match hi {
        // The first indexed record not below `hi`: every one after it is
        // not either.
        Some(k) => {
            let i = below(k)?;
            index.get(i).map_or(*end, |e| e.0)
        }
        None => *end,
    };
    Ok((from, to))
}

/// A merge of the parts' records between two keys, a row at a time.
struct RangeMerge {
    cmp: Arc<Cmp>,
    cursors: Vec<Cursor>,
    /// A heap of cursors (their numbers), the least record on top; ties
    /// to the lower number (the earlier part).
    heap: Vec<usize>,
}

impl RangeMerge {
    fn new(
        cmp: Arc<Cmp>,
        parts: &[Arc<Part>],
        lo: Option<&Key>,
        hi: Option<&Arc<Key>>,
    ) -> Result<RangeMerge> {
        let mut cursors = Vec::with_capacity(parts.len());
        for part in parts {
            let mut c = match &part.data {
                Data::Memory { ents, .. } => {
                    let i = match lo {
                        Some(k) => lower_bound(&cmp, part, k)?,
                        None => 0,
                    };
                    let end = match hi {
                        Some(k) => lower_bound(&cmp, part, k)?,
                        None => ents.len(),
                    };
                    if i >= end {
                        continue;
                    }
                    Cursor {
                        part: Arc::clone(part),
                        src: Src::Memory { i, end },
                        values: Vec::new(),
                        hi: None,
                        done: false,
                    }
                }
                Data::Disk { run, .. } => {
                    let (from, to) = disk_bounds(&cmp, part, lo, hi.map(|k| &**k))?;
                    if from >= to {
                        continue;
                    }
                    let mut it = run.range(from, to);
                    let mut rec = Vec::new();
                    let mut done = true;
                    // The records below the range's start skipped.
                    while let Some(r) = it.next_record()? {
                        let below = match lo {
                            None => false,
                            Some(k) => {
                                let key = cmp.key_of(r, part)?;
                                cmp.keys((&key.norm, &key.values), (&k.norm, &k.values))
                                    == Ordering::Less
                            }
                        };
                        if !below {
                            rec.extend_from_slice(r);
                            done = false;
                            break;
                        }
                    }
                    if done {
                        continue;
                    }
                    Cursor {
                        part: Arc::clone(part),
                        src: Src::Disk { it, rec },
                        values: Vec::new(),
                        hi: hi.cloned(),
                        done: false,
                    }
                }
            };
            c.load(&cmp)?;
            c.check_hi(&cmp);
            if !c.done {
                cursors.push(c);
            }
        }
        let mut m = RangeMerge {
            cmp,
            heap: (0..cursors.len()).collect(),
            cursors,
        };
        for i in (0..m.heap.len() / 2).rev() {
            m.sift(i);
        }
        Ok(m)
    }

    fn less(&self, a: usize, b: usize) -> bool {
        let (x, y) = (&self.cursors[a], &self.cursors[b]);
        match self.cmp.keys(x.key(), y.key()) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => a < b,
        }
    }

    fn sift(&mut self, mut i: usize) {
        let n = self.heap.len();
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut m = i;
            if l < n && self.less(self.heap[l], self.heap[m]) {
                m = l;
            }
            if r < n && self.less(self.heap[r], self.heap[m]) {
                m = r;
            }
            if m == i {
                return;
            }
            self.heap.swap(i, m);
            i = m;
        }
    }

    fn next_row(&mut self) -> Result<Option<Row>> {
        self.next_with(|rec, part| decode_row(rec, &part.enums))
    }

    /// `f` of the next record and its part.
    fn next_with<T>(&mut self, f: impl FnOnce(&[u8], &Part) -> Result<T>) -> Result<Option<T>> {
        let Some(&top) = self.heap.first() else {
            return Ok(None);
        };
        let c = &mut self.cursors[top];
        let out = f(c.record(), &c.part)?;
        c.advance(&self.cmp)?;
        if c.done {
            let last = self.heap.pop().expect("a top");
            if !self.heap.is_empty() {
                self.heap[0] = last;
            }
        }
        self.sift(0);
        Ok(Some(out))
    }
}

/// `group`'s records merged into one part on disk (their enums in one
/// table).
fn premerge(cmp: &Arc<Cmp>, group: &[Arc<Part>]) -> Result<Arc<Part>> {
    let mut kinds = Kinds::new(cmp.flags.len());
    for p in group {
        kinds.join(&p.kinds);
    }
    let raw = group.iter().all(|p| p.enums.is_empty());
    let width = cmp.flags.len();
    let mut enums = Enums::default();
    let mut run = crate::spill::Run::new()?;
    let mut index = Vec::new();
    let mut buf = Vec::new();
    let mut rows = 0;
    let mut m = RangeMerge::new(Arc::clone(cmp), group, None, None)?;
    loop {
        buf.clear();
        let more = m.next_with(|rec, part| {
            if raw {
                buf.extend_from_slice(rec);
            } else {
                let keys = decode_keys(rec, width, &part.enums)?;
                let row = decode_row(rec, &part.enums)?;
                encode_record(&mut buf, &cmp.flags, &keys, &row, &mut enums);
            }
            Ok(())
        })?;
        if more.is_none() {
            break;
        }
        if rows % INDEX_EVERY == 0 {
            index.push((run.at(), key_part(&buf).to_vec()));
        }
        run.push_record(&[&buf])?;
        rows += 1;
    }
    let end = run.at();
    Ok(Arc::new(Part {
        data: Data::Disk {
            run: run.seal()?,
            index,
            end,
        },
        enums: if raw { Vec::new() } else { enums.values },
        kinds,
        rows,
    }))
}

impl Iterator for RangeMerge {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Result<Row>> {
        self.next_row().transpose()
    }
}

/// The rows of `parts` (each sorted, given in input order) in one order —
/// ties in the order of their parts.
pub fn merge(
    mut parts: Vec<Arc<Part>>,
    flags: Arc<[(bool, bool)]>,
) -> Result<Box<dyn Iterator<Item = Result<Row>>>> {
    parts.retain(|p| p.rows > 0);
    let mut kinds = Kinds::new(flags.len());
    for p in &parts {
        kinds.join(&p.kinds);
    }
    let cmp = Arc::new(Cmp {
        flags: Arc::clone(&flags),
        normalized: kinds.normalized(),
    });
    // Very many parts: merged a group at a time into fewer first.
    while parts.len() > FAN_IN {
        parts = parts
            .chunks(FAN_IN)
            .map(|group| premerge(&cmp, group))
            .collect::<Result<_>>()?;
    }
    let rows: usize = parts.iter().map(|p| p.rows).sum();
    let workers = crate::budget::workers();
    if rows < PARALLEL_FROM || workers < 2 {
        return Ok(Box::new(RangeMerge::new(cmp, &parts, None, None)?));
    }
    // Ranges: enough that a few in flight a worker stay within
    // `work_mem` as rows — about four times their records' bytes.
    let bytes: usize = parts
        .iter()
        .map(|p| match &p.data {
            Data::Memory { bytes, .. } => bytes.len(),
            Data::Disk { end, .. } => *end as usize,
        })
        .sum();
    let limit = crate::budget::work_mem().max(1 << 20);
    let ranges = (4 * workers)
        .max(bytes.saturating_mul(4 * AHEAD * workers) / limit)
        .min(rows / RANGE_ROWS)
        .max(1);
    let bounds = splitters(&cmp, &parts, ranges)?;
    Ok(Box::new(Parallel::start(cmp, parts, bounds, workers)))
}

/// `n - 1` keys cutting the parts' records into about equal ranges: a
/// sample of each part's keys (its index's, on disk), sorted, taken at
/// even steps.
fn splitters(cmp: &Cmp, parts: &[Arc<Part>], n: usize) -> Result<Vec<Arc<Key>>> {
    let rows: usize = parts.iter().map(|p| p.rows).sum();
    let step = (rows / (n * 16)).max(1);
    let mut sample: Vec<Key> = Vec::new();
    for p in parts {
        match &p.data {
            Data::Memory { bytes, ents } => {
                for e in ents.iter().step_by(step) {
                    sample.push(cmp.key_of(&bytes[e.at..e.at + e.len as usize], p)?);
                }
            }
            Data::Disk { index, .. } => {
                let every = (step / INDEX_EVERY).max(1);
                for (_, k) in index.iter().step_by(every) {
                    sample.push(cmp.key_of(k, p)?);
                }
            }
        }
    }
    sample.sort_by(|a, b| cmp.keys((&a.norm, &a.values), (&b.norm, &b.values)));
    let mut out: Vec<Arc<Key>> = Vec::with_capacity(n);
    for i in 1..n {
        let k = &sample[i * sample.len() / n];
        let same = out.last().is_some_and(|l| {
            cmp.keys((&l.norm, &l.values), (&k.norm, &k.values)) == Ordering::Equal
        });
        if !same {
            out.push(Arc::new(k.clone()));
        }
    }
    Ok(out)
}

/// The ranges' merges on worker threads, read in order.
struct Parallel {
    shared: Arc<Shared>,
    threads: Vec<std::thread::JoinHandle<()>>,
    current: std::vec::IntoIter<Row>,
    next: usize,
    failed: bool,
}

struct Shared {
    cmp: Arc<Cmp>,
    parts: Vec<Arc<Part>>,
    bounds: Vec<Arc<Key>>,
    taken: AtomicUsize,
    stop: AtomicBool,
    state: Mutex<State>,
    changed: Condvar,
    ahead: usize,
}

struct State {
    slots: Vec<Option<Result<Vec<Row>>>>,
    read: usize,
}

impl Parallel {
    fn start(cmp: Arc<Cmp>, parts: Vec<Arc<Part>>, bounds: Vec<Arc<Key>>, workers: usize) -> Self {
        let n = bounds.len() + 1;
        let shared = Arc::new(Shared {
            cmp,
            parts,
            bounds,
            taken: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            state: Mutex::new(State {
                slots: (0..n).map(|_| None).collect(),
                read: 0,
            }),
            changed: Condvar::new(),
            ahead: AHEAD * workers,
        });
        let ambient = crate::parallel::Ambient::capture();
        let threads = (0..workers.min(n))
            .map(|_| {
                let shared = Arc::clone(&shared);
                let ambient = ambient.clone();
                nucleus_platform::session_thread("nucleus worker")
                    .spawn(move || {
                        ambient.run(|| shared.work());
                    })
                    .expect("a worker thread")
            })
            .collect();
        Parallel {
            shared,
            threads,
            current: Vec::new().into_iter(),
            next: 0,
            failed: false,
        }
    }
}

impl Shared {
    fn work(&self) {
        let n = self.bounds.len() + 1;
        loop {
            let r = self.taken.fetch_add(1, SeqCst);
            if r >= n {
                return;
            }
            {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                while r >= st.read + self.ahead && !self.stop.load(SeqCst) {
                    st = self.changed.wait(st).unwrap_or_else(|e| e.into_inner());
                }
            }
            if self.stop.load(SeqCst) {
                return;
            }
            let lo = r.checked_sub(1).map(|i| &*self.bounds[i]);
            let hi = self.bounds.get(r);
            let rows = RangeMerge::new(Arc::clone(&self.cmp), &self.parts, lo, hi)
                .and_then(|m| m.collect::<Result<Vec<_>>>());
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.slots[r] = Some(rows);
            self.changed.notify_all();
        }
    }
}

impl Iterator for Parallel {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Result<Row>> {
        loop {
            if let Some(row) = self.current.next() {
                return Some(Ok(row));
            }
            let n = self.shared.bounds.len() + 1;
            if self.failed || self.next >= n {
                return None;
            }
            let rows = {
                let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
                while st.slots[self.next].is_none() {
                    st = self
                        .shared
                        .changed
                        .wait(st)
                        .unwrap_or_else(|e| e.into_inner());
                }
                let rows = st.slots[self.next].take().expect("a merged range");
                st.read = self.next + 1;
                self.shared.changed.notify_all();
                rows
            };
            self.next += 1;
            match rows {
                Ok(rows) => self.current = rows.into_iter(),
                Err(e) => {
                    self.failed = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

impl Drop for Parallel {
    fn drop(&mut self) {
        self.shared.stop.store(true, SeqCst);
        {
            let _st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            self.shared.changed.notify_all();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DataType;

    fn xorshift(mut seed: u64) -> impl FnMut() -> u64 {
        move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        }
    }

    /// A value of kind `k` from `r`: every kind normalized keys take, with
    /// the edges of their orders, and kinds compared by value.
    fn value(k: u64, r: u64) -> Value {
        const TEXTS: [&str; 10] = ["", "a", "ab", "abc", "a\0", "a\0b", "b", "\0", "ab\0", "zz"];
        const FLOATS: [f64; 9] = [
            f64::NAN,
            -0.0,
            0.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            1.5,
            -1.5,
            f64::MIN_POSITIVE,
            -1e300,
        ];
        const DECIMALS: [&str; 9] = [
            "0",
            "1",
            "1.0",
            "1.50",
            "-1.5",
            "100",
            "0.001",
            "-0.01",
            "99999999999999999999.5",
        ];
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
            1 => Value::Text(TEXTS[(r % 10) as usize].to_string()),
            2 => Value::Char(format!("{:<4}", TEXTS[(r % 6) as usize])),
            3 => Value::Date((r % 9) as i32 - 4),
            4 => Value::Timestamp((r % 9) as i64 * 1000 - 3000),
            5 => Value::Bool(r.is_multiple_of(2)),
            6 => Value::Time((r % 5) as i64),
            7 => Value::Float(FLOATS[(r % 9) as usize]),
            8 => {
                Value::Numeric(crate::decimal::Decimal::parse(DECIMALS[(r % 9) as usize]).unwrap())
            }
            9 => Value::Bytea(TEXTS[(r % 10) as usize].as_bytes().into()),
            10 => {
                Value::Text(["1 day", "-2 hours", "1 mon", "30 days", "0"][(r % 5) as usize].into())
                    .cast(DataType::Interval)
                    .unwrap()
            }
            _ => {
                if r.is_multiple_of(2) {
                    Value::Int((r % 5) as i64)
                } else {
                    Value::Float((r % 9) as f64 / 2.0 - 1.0)
                }
            }
        }
    }

    /// The rows' numbers as `sort` puts them: rows `[n, keys…]` by keys.
    fn sorted(pairs: &[(Vec<Value>, usize)], flags: &[(bool, bool)], parts: usize) -> Vec<usize> {
        let flags: Arc<[(bool, bool)]> = flags.into();
        let budget = Budget::new(parts);
        let mut all = Vec::new();
        for chunk in pairs.chunks(pairs.len().div_ceil(parts)) {
            let mut s = Sorter::new(Arc::clone(&flags), Arc::clone(&budget));
            for (keys, n) in chunk {
                let mut row = vec![Value::Int(*n as i64)];
                row.extend(keys.iter().cloned());
                s.push(keys, &row).unwrap();
            }
            all.extend(s.finish().unwrap());
        }
        merge(all, flags)
            .unwrap()
            .map(|r| match r.unwrap()[0] {
                Value::Int(n) => n as usize,
                _ => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn sorts_are_stable_sorts_by_compare() {
        // Random keys of every kind — texts that are prefixes of each
        // other, with zero bytes, padded char(n), NaN and -0, decimals of
        // one value written two ways, NULLs —, under every direction and
        // NULL placement, in memory and on disk, merged on one thread and
        // in ranges on four: the order of a stable sort by `compare`.
        let mut next = xorshift(0x9E37_79B9_7F4A_7C15);
        for round in 0..36 {
            let kinds = [round % 12, (round * 5 + 3) % 12];
            let pairs: Vec<(Vec<Value>, usize)> = (0..600)
                .map(|i| (kinds.iter().map(|&k| value(k, next())).collect(), i))
                .collect();
            for flags in [
                [(false, false), (false, false)],
                [(true, false), (false, true)],
                [(false, true), (true, true)],
                [(true, true), (true, false)],
            ] {
                let mut want = pairs.clone();
                want.sort_by(|(a, _), (b, _)| compare(&flags, a, b));
                let want: Vec<usize> = want.iter().map(|p| p.1).collect();
                for (work_mem, workers, parts) in [
                    (64 << 20, 1, 1),
                    (64 << 20, 4, 3),
                    (6 << 10, 1, 2),
                    (6 << 10, 4, 5),
                    (1, 4, 7),
                ] {
                    let got = crate::budget::with_budget(
                        crate::budget::Budget {
                            work_mem: Some(work_mem),
                            workers: Some(workers),
                            ..Default::default()
                        },
                        || sorted(&pairs, &flags, parts),
                    );
                    assert_eq!(
                        got, want,
                        "kinds {kinds:?}, flags {flags:?}, work_mem {work_mem}, workers {workers}, parts {parts}"
                    );
                }
            }
        }
    }

    #[test]
    fn many_parts_are_merged_a_group_at_a_time() {
        let mut next = xorshift(7);
        let pairs: Vec<(Vec<Value>, usize)> = (0..20_000)
            .map(|i| (vec![value(i as u64 % 2 * 8, next())], i))
            .collect();
        let flags = [(false, true)];
        let mut want = pairs.clone();
        want.sort_by(|(a, _), (b, _)| compare(&flags, a, b));
        let want: Vec<usize> = want.iter().map(|p| p.1).collect();
        for workers in [1, 3] {
            let got = crate::budget::with_budget(
                crate::budget::Budget {
                    work_mem: Some(1),
                    workers: Some(workers),
                    ..Default::default()
                },
                || sorted(&pairs, &flags, FAN_IN + 300),
            );
            assert_eq!(got, want, "workers {workers}");
        }
    }

    #[test]
    fn enums_keep_their_tables() {
        // Enum values are kept by index in a part's table: rows with
        // enums, sorted in parts and merged, come back as they went in.
        let flags: Arc<[(bool, bool)]> = [(true, false), (false, false)].as_slice().into();
        let rows: Vec<Row> = (0..2000)
            .map(|i| {
                let label = ["low", "mid", "high"][(i % 3) as usize];
                let e = Value::Enum(Arc::new(crate::enums::EnumValue {
                    ty: None,
                    key: [b'a' + (i % 3) as u8].as_slice().into(),
                    label: label.into(),
                }));
                vec![
                    e.clone(),
                    Value::Int((i * 37) % 2000),
                    Value::Array(vec![Value::Int(i), e]),
                ]
            })
            .collect();
        let got: Vec<Row> = crate::budget::with_budget(
            crate::budget::Budget {
                work_mem: Some(1),
                workers: Some(2),
                ..Default::default()
            },
            || {
                let budget = Budget::new(2);
                let mut parts = Vec::new();
                for chunk in rows.chunks(300) {
                    let mut s = Sorter::new(Arc::clone(&flags), Arc::clone(&budget));
                    for r in chunk {
                        s.push(&r[..2], r).unwrap();
                    }
                    parts.extend(s.finish().unwrap());
                }
                merge(parts, Arc::clone(&flags))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect()
            },
        );
        let mut want = rows.clone();
        want.sort_by(|a, b| compare(&flags, &a[..2], &b[..2]));
        assert_eq!(got, want);
    }
}
