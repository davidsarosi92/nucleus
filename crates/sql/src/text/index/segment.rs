//! A text index's segment (PLAN.md §M14.21 T4): an immutable inverted
//! index of some documents — each document's primary key, the hash of its
//! text and its length in terms; the terms, sorted, each with its
//! documents in blocks of 128 (their ascending numbers as bit-packed
//! deltas, their frequencies bit-packed beside them — Lucene's and
//! Tantivy's layout) and its positions apart, read only by phrases.
//!
//! Layout (little-endian): version; documents, terms, total length;
//! primary keys (offsets, bytes); hashes; lengths; terms (offsets, bytes);
//! per term its document count and where its postings and positions
//! start; the postings; the positions.  A block: the bits of its deltas,
//! the bits of its frequencies less one, then each packed.  Positions: a
//! document's as varint deltas, documents in posting order.
//!
//! Version 2 adds, for each term in more than one block, a *skip list*
//! (per term its start, after the positions'): per block, varints of its
//! last document (less the block before's), its bytes, and its *impacts*
//! — the (frequency, length) pairs of its documents no other pair there
//! beats (a frequency as high in a text no longer; Lucene's competitive
//! impacts, frequency ascending, both as deltas).  Not scores: a block's
//! bound is BM25 of its impacts, by the snapshot's statistics, at query
//! time — so a search skips a block (MaxScore, [`super::store`]) without
//! decoding it, and reaches one without decoding the blocks before.
//! Version 1 (no skip lists) is read, its skip lists made as it is.

use std::collections::HashMap;
use std::sync::OnceLock;

use nucleus_kernels::MixState;

use crate::error::{Error, Result};
use crate::text::{Analyzer, Token};

/// Documents a block holds.
pub(crate) const BLOCK: usize = 128;
const VERSION: u8 = 2;

pub(crate) fn corrupt(what: &str) -> Error {
    Error::internal(format!("a text index is damaged: {what}"))
}

/// A document's terms, as an index keeps them: its length (terms counted
/// with repeats), and each distinct term with its positions, terms sorted
/// — or, a *weighted* document's (a sparse vector's, §M14.21 T7b), each
/// term's weight in `weights` and no positions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Doc {
    pub(crate) len: u32,
    pub(crate) terms: Vec<(String, Vec<u32>)>,
    /// Each term's weight where the document is weighted (its frequency
    /// in the postings, [`weight_bits`]); empty: a term's frequency is its
    /// positions' count.
    pub(crate) weights: Vec<u32>,
}

/// A weight as a frequency: `x`'s bits in an order that is the numbers'
/// (a negative's flipped, a positive's sign bit set — the radix sort's
/// trick), so the greatest frequency is the greatest weight; never 0 for
/// a value a sparse vector holds (no zeros, no NaN).
pub(crate) fn weight_bits(x: f32) -> u32 {
    let b = x.to_bits();
    if b >> 31 == 1 { !b } else { b | 1 << 31 }
}

/// [`weight_bits`]' weight back.
pub(crate) fn of_weight_bits(b: u32) -> f32 {
    f32::from_bits(if b >> 31 == 1 { b & !(1 << 31) } else { !b })
}

impl Doc {
    /// `text` analyzed by `a`.
    pub(crate) fn analyze(a: &dyn Analyzer, text: &str) -> Doc {
        let mut all: Vec<(String, u32)> = Vec::new();
        a.analyze(text, &mut |t: Token| all.push((t.term, t.position)));
        let len = all.len() as u32;
        // By term, each term's positions in order (a stable sort).
        all.sort_by(|a, b| a.0.cmp(&b.0));
        let mut terms: Vec<(String, Vec<u32>)> = Vec::new();
        for (t, p) in all {
            match terms.last_mut() {
                Some((last, ps)) if *last == t => ps.push(p),
                _ => terms.push((t, vec![p])),
            }
        }
        Doc {
            len,
            terms,
            weights: Vec::new(),
        }
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        varint(out, u64::from(self.len));
        varint(out, self.terms.len() as u64);
        for (k, (t, ps)) in self.terms.iter().enumerate() {
            varint(out, t.len() as u64);
            out.extend_from_slice(t.as_bytes());
            // A weighted term: no positions (a text's term has one at
            // least, so 0 tells it), then its weight.
            if let Some(&w) = self.weights.get(k) {
                varint(out, 0);
                varint(out, u64::from(w));
                continue;
            }
            varint(out, ps.len() as u64);
            let mut prev = 0;
            for &p in ps {
                varint(out, u64::from(p - prev));
                prev = p;
            }
        }
    }

    pub(crate) fn decode(r: &mut Reader) -> Result<Doc> {
        let len = r.varint32()?;
        let n = r.varint32()? as usize;
        let mut terms = Vec::with_capacity(n.min(1 << 16));
        let mut weights = Vec::new();
        for _ in 0..n {
            let tl = r.varint32()? as usize;
            let t = std::str::from_utf8(r.take(tl)?)
                .map_err(|_| corrupt("a term not UTF-8"))?
                .to_string();
            let f = r.varint32()? as usize;
            if f == 0 {
                weights.push(r.varint32()?);
                terms.push((t, Vec::new()));
                continue;
            }
            let mut ps = Vec::with_capacity(f.min(1 << 16));
            let mut p = 0u32;
            for _ in 0..f {
                p = p
                    .checked_add(r.varint32()?)
                    .ok_or_else(|| corrupt("a position"))?;
                ps.push(p);
            }
            terms.push((t, ps));
        }
        if !weights.is_empty() && weights.len() != terms.len() {
            return Err(corrupt("a document weighted in part"));
        }
        Ok(Doc {
            len,
            terms,
            weights,
        })
    }
}

pub(crate) fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Bytes read in order.
pub(crate) struct Reader<'a>(pub(crate) &'a [u8]);

impl<'a> Reader<'a> {
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(corrupt("cut short"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4")))
    }

    pub(crate) fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8")))
    }

    pub(crate) fn varint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.u8()?;
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Ok(v);
            }
        }
        Err(corrupt("a varint"))
    }

    pub(crate) fn varint32(&mut self) -> Result<u32> {
        u32::try_from(self.varint()?).map_err(|_| corrupt("a varint past 32 bits"))
    }

    fn u32s(&mut self, n: usize) -> Result<Vec<u32>> {
        let b = self.take(n.checked_mul(4).ok_or_else(|| corrupt("a count"))?)?;
        Ok(b.as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect())
    }
}

/// The bits `max` needs.
fn bits(max: u32) -> u8 {
    (32 - max.leading_zeros()) as u8
}

/// `vals`, each in `b` bits, appended.
fn pack(vals: &[u32], b: u8, out: &mut Vec<u8>) {
    let (mut acc, mut n) = (0u64, 0u32);
    for &v in vals {
        acc |= u64::from(v) << n;
        n += u32::from(b);
        while n >= 8 {
            out.push(acc as u8);
            acc >>= 8;
            n -= 8;
        }
    }
    if n > 0 {
        out.push(acc as u8);
    }
}

/// `out.len()` values of `b` bits from `src`; the bytes they took.
fn unpack(src: &[u8], b: u8, out: &mut [u32]) -> Result<usize> {
    let bytes = (out.len() * b as usize).div_ceil(8);
    if src.len() < bytes {
        return Err(corrupt("a block cut short"));
    }
    if b == 0 {
        out.fill(0);
        return Ok(0);
    }
    let mask = (1u64 << b) - 1;
    let (mut acc, mut n, mut at) = (0u64, 0u32, 0usize);
    for o in out.iter_mut() {
        while n < u32::from(b) {
            acc |= u64::from(src[at]) << n;
            at += 1;
            n += 8;
        }
        *o = (acc & mask) as u32;
        acc >>= b;
        n -= u32::from(b);
    }
    Ok(bytes)
}

/// A term's documents (ascending), their frequencies, and — when asked
/// for — their positions (`pos[at[i]..at[i + 1]]` document `i`'s).
#[derive(Default, Debug)]
pub(crate) struct Postings {
    pub(crate) docs: Vec<u32>,
    pub(crate) freqs: Vec<u32>,
    pub(crate) at: Vec<u32>,
    pub(crate) pos: Vec<u32>,
}

impl Postings {
    pub(crate) fn positions(&self, i: usize) -> &[u32] {
        if self.at.is_empty() {
            return &[]; // weighted, or not read
        }
        &self.pos[self.at[i] as usize..self.at[i + 1] as usize]
    }

    /// A weighted document's posting: its weight as its frequency, no
    /// positions.
    fn push_weighted(&mut self, doc: u32, weight: u32) {
        self.docs.push(doc);
        self.freqs.push(weight);
    }

    fn push(&mut self, doc: u32, positions: &[u32]) {
        self.docs.push(doc);
        self.freqs.push(positions.len() as u32);
        if self.at.is_empty() {
            self.at.push(0);
        }
        self.pos.extend_from_slice(positions);
        self.at.push(self.pos.len() as u32);
    }
}

/// A segment as written: documents (primary key, hash, length) and terms
/// with their postings, the terms added in ascending order.
#[derive(Default)]
pub(crate) struct Builder {
    pk_at: Vec<u32>,
    pks: Vec<u8>,
    hashes: Vec<u64>,
    lens: Vec<u32>,
    total: u64,
    t: Terms,
}

/// A segment's terms as written, with their postings, positions and skip
/// lists — some of them, in order: a run of a merge's ([`merge`]),
/// joined to the runs before.
#[derive(Default)]
struct Terms {
    term_at: Vec<u32>,
    terms: Vec<u8>,
    info: Vec<[u32; 4]>,
    postings: Vec<u8>,
    positions: Vec<u8>,
    skips: Vec<u8>,
}

fn too_big() -> Error {
    Error::internal("a segment past 4 GB")
}

fn offset(n: usize) -> Result<u32> {
    u32::try_from(n).map_err(|_| too_big())
}

impl Terms {
    /// Adds `term` (after every term added before) and its postings, with
    /// positions; `lens` the documents' lengths.
    fn add(&mut self, term: &[u8], p: &Postings, lens: &[u32]) -> Result<()> {
        if p.docs.is_empty() {
            return Ok(());
        }
        if self.term_at.is_empty() {
            self.term_at.push(0);
        }
        self.info.push([
            p.docs.len() as u32,
            offset(self.postings.len())?,
            offset(self.positions.len())?,
            offset(self.skips.len())?,
        ]);
        self.terms.extend_from_slice(term);
        self.term_at.push(offset(self.terms.len())?);
        let mut prev: i64 = -1;
        let mut deltas = [0u32; BLOCK];
        let mut freqs = [0u32; BLOCK];
        let mut sizes = Vec::with_capacity(p.docs.len().div_ceil(BLOCK));
        for (docs, fs) in p.docs.chunks(BLOCK).zip(p.freqs.chunks(BLOCK)) {
            let at = self.postings.len();
            for (i, &d) in docs.iter().enumerate() {
                deltas[i] = (i64::from(d) - prev - 1) as u32;
                prev = i64::from(d);
                freqs[i] = fs[i] - 1;
            }
            let n = docs.len();
            let (db, fb) = (
                bits(deltas[..n].iter().copied().max().unwrap_or(0)),
                bits(freqs[..n].iter().copied().max().unwrap_or(0)),
            );
            self.postings.push(db);
            self.postings.push(fb);
            pack(&deltas[..n], db, &mut self.postings);
            pack(&freqs[..n], fb, &mut self.postings);
            sizes.push((self.postings.len() - at) as u32);
        }
        if p.docs.len() > BLOCK {
            skip_list(&p.docs, &p.freqs, lens, &sizes, &mut self.skips);
        }
        for i in 0..p.docs.len() {
            let mut last = 0;
            for &q in p.positions(i) {
                varint(&mut self.positions, u64::from(q - last));
                last = q;
            }
        }
        Ok(())
    }

    /// `next`'s terms (all after these) added after these.
    fn join(&mut self, next: Terms) -> Result<()> {
        if next.info.is_empty() {
            return Ok(());
        }
        if self.term_at.is_empty() {
            self.term_at.push(0);
        }
        let base = [
            0,
            offset(self.postings.len())?,
            offset(self.positions.len())?,
            offset(self.skips.len())?,
        ];
        let at = offset(self.terms.len())?;
        for x in &next.term_at[1..] {
            self.term_at.push(at.checked_add(*x).ok_or_else(too_big)?);
        }
        for i in &next.info {
            let mut j = *i;
            for k in 1..4 {
                j[k] = base[k].checked_add(i[k]).ok_or_else(too_big)?;
            }
            self.info.push(j);
        }
        self.terms.extend_from_slice(&next.terms);
        self.postings.extend_from_slice(&next.postings);
        self.positions.extend_from_slice(&next.positions);
        self.skips.extend_from_slice(&next.skips);
        offset(
            self.postings
                .len()
                .max(self.positions.len())
                .max(self.skips.len()),
        )?;
        Ok(())
    }
}

impl Builder {
    /// Adds a document; its number is the count before.
    pub(crate) fn doc(&mut self, pk: &[u8], hash: u64, len: u32) {
        if self.pk_at.is_empty() {
            self.pk_at.push(0);
        }
        self.pks.extend_from_slice(pk);
        self.pk_at.push(self.pks.len() as u32);
        self.hashes.push(hash);
        self.lens.push(len);
        self.total += u64::from(len);
    }

    /// Adds `term` (after every term added before) and its postings, with
    /// positions.
    pub(crate) fn term(&mut self, term: &[u8], p: &Postings) -> Result<()> {
        self.t.add(term, p, &self.lens)
    }

    /// Documents added so far.
    pub(crate) fn docs(&self) -> u32 {
        self.hashes.len() as u32
    }

    pub(crate) fn encode(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            64 + self.pks.len()
                + self.hashes.len() * 16
                + self.t.terms.len()
                + self.t.info.len() * 16
                + self.t.postings.len()
                + self.t.positions.len()
                + self.t.skips.len(),
        );
        out.push(VERSION);
        let docs = self.hashes.len() as u32;
        out.extend_from_slice(&docs.to_le_bytes());
        out.extend_from_slice(&(self.t.info.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.total.to_le_bytes());
        let at = |v: &[u32], n: u32| if n == 0 { vec![0] } else { v.to_vec() };
        for x in at(&self.pk_at, docs) {
            out.extend_from_slice(&x.to_le_bytes());
        }
        out.extend_from_slice(&self.pks);
        for h in &self.hashes {
            out.extend_from_slice(&h.to_le_bytes());
        }
        for l in &self.lens {
            out.extend_from_slice(&l.to_le_bytes());
        }
        for x in at(&self.t.term_at, self.t.info.len() as u32) {
            out.extend_from_slice(&x.to_le_bytes());
        }
        out.extend_from_slice(&self.t.terms);
        for i in &self.t.info {
            for x in i {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        out.extend_from_slice(&(self.t.postings.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.t.postings);
        out.extend_from_slice(&(self.t.positions.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.t.positions);
        out.extend_from_slice(&(self.t.skips.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.t.skips);
        out
    }
}

/// The (frequency, length) pairs of `pairs` no other beats — none with a
/// frequency as high in a text no longer — frequency ascending (lengths
/// then ascending too).
pub(crate) fn impacts(pairs: &mut Vec<(u32, u32)>) {
    pairs.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut shortest = u32::MAX;
    pairs.retain(|&(_, len)| {
        let keep = len < shortest;
        shortest = shortest.min(len);
        keep
    });
    pairs.reverse();
}

/// A term's skip list: per block (of `sizes[i]` bytes) of `docs` and
/// `freqs`, its last document, its bytes and its impacts (`lens` each
/// document's length).
fn skip_list(docs: &[u32], freqs: &[u32], lens: &[u32], sizes: &[u32], out: &mut Vec<u8>) {
    let mut last = 0u32;
    let mut pairs = Vec::with_capacity(BLOCK);
    for ((ds, fs), &size) in docs.chunks(BLOCK).zip(freqs.chunks(BLOCK)).zip(sizes) {
        let l = *ds.last().expect("a block");
        varint(out, u64::from(l - last));
        last = l;
        varint(out, u64::from(size));
        pairs.clear();
        pairs.extend(ds.iter().zip(fs).map(|(&d, &f)| (f, lens[d as usize])));
        impacts(&mut pairs);
        varint(out, pairs.len() as u64);
        let (mut pf, mut pl) = (0, 0);
        for &(f, len) in &pairs {
            varint(out, u64::from(f - pf));
            varint(out, u64::from(len - pl));
            (pf, pl) = (f, len);
        }
    }
}

/// `docs` (primary key, hash, terms) as a segment, numbered in order.
pub(crate) fn of_docs(docs: &[(Vec<u8>, u64, Doc)]) -> Result<Vec<u8>> {
    let mut b = Builder::default();
    // Hashed, then sorted once: a tree's string comparisons were a
    // fifth of a build (§M14.21 T5b).
    let mut by: HashMap<&str, Postings, MixState> = HashMap::default();
    for (n, (pk, hash, doc)) in docs.iter().enumerate() {
        b.doc(pk, *hash, doc.len);
        for (k, (t, ps)) in doc.terms.iter().enumerate() {
            let p = by.entry(t.as_str()).or_default();
            match doc.weights.get(k) {
                Some(&w) => p.push_weighted(n as u32, w),
                None => p.push(n as u32, ps),
            }
        }
    }
    let mut by: Vec<(&str, Postings)> = by.into_iter().collect();
    by.sort_unstable_by(|a, b| a.0.cmp(b.0));
    for (t, p) in &by {
        b.term(t.as_bytes(), p)?;
    }
    Ok(b.encode())
}

/// A segment read into memory.
pub(crate) struct Segment {
    pk_at: Vec<u32>,
    pks: Vec<u8>,
    pub(crate) hashes: Vec<u64>,
    pub(crate) lens: Vec<u32>,
    pub(crate) total: u64,
    term_at: Vec<u32>,
    terms: Vec<u8>,
    /// Per term: documents, where its postings, positions and skip list
    /// start.
    info: Vec<[u32; 4]>,
    postings: Vec<u8>,
    positions: Vec<u8>,
    skips: Vec<u8>,
    /// Documents by primary key and hash ([`Segment::find_doc`]).
    by_pk: OnceLock<HashMap<u64, Vec<u32>, MixState>>,
}

fn pk_key(pk: &[u8], hash: u64) -> u64 {
    crate::object_cache::object_id(11, pk) ^ hash
}

impl Segment {
    pub(crate) fn decode(b: &[u8]) -> Result<Segment> {
        let mut r = Reader(b);
        let version = r.u8()?;
        if !(1..=VERSION).contains(&version) {
            return Err(corrupt("a segment of another version"));
        }
        let docs = r.u32()? as usize;
        let nterms = r.u32()? as usize;
        let total = r.u64()?;
        let pk_at = r.u32s(docs + 1)?;
        let pks = r.take(*pk_at.last().expect("one") as usize)?.to_vec();
        let hashes = r
            .take(docs * 8)?
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_le_bytes(*c))
            .collect();
        let lens = r.u32s(docs)?;
        let term_at = r.u32s(nterms + 1)?;
        let terms = r.take(*term_at.last().expect("one") as usize)?.to_vec();
        let width = if version == 1 { 3 } else { 4 };
        let flat = r.u32s(nterms * width)?;
        let info: Vec<[u32; 4]> = flat
            .chunks_exact(width)
            .map(|c| [c[0], c[1], c[2], c.get(3).copied().unwrap_or(0)])
            .collect();
        let n = r.u32()? as usize;
        let postings = r.take(n)?.to_vec();
        let n = r.u32()? as usize;
        let positions = r.take(n)?.to_vec();
        let mut skips = Vec::new();
        if version > 1 {
            let n = r.u32()? as usize;
            skips = r.take(n)?.to_vec();
        }
        if !r.0.is_empty() {
            return Err(corrupt("a segment with bytes after it"));
        }
        let mut s = Segment {
            pk_at,
            pks,
            hashes,
            lens,
            total,
            term_at,
            terms,
            info,
            postings,
            positions,
            skips,
            by_pk: OnceLock::new(),
        };
        if version == 1 {
            // Its skip lists, as version 2 writes them.
            let mut starts = Vec::with_capacity(s.terms());
            for t in 0..s.terms() {
                let df = s.info[t][0] as usize;
                starts.push(s.skips.len() as u32);
                if df <= BLOCK {
                    continue;
                }
                let p = s.postings(t, false)?;
                let (mut at, mut prev) = (s.info[t][1] as usize, -1i64);
                let mut sizes = Vec::with_capacity(df.div_ceil(BLOCK));
                let (mut docs, mut freqs) = ([0u32; BLOCK], [0u32; BLOCK]);
                for chunk in p.docs.chunks(BLOCK) {
                    let n = chunk.len();
                    let used = s.block(at, prev, &mut docs[..n], &mut freqs[..n])?;
                    sizes.push(used as u32);
                    at += used;
                    prev = i64::from(docs[n - 1]);
                }
                let mut out = std::mem::take(&mut s.skips);
                skip_list(&p.docs, &p.freqs, &s.lens, &sizes, &mut out);
                s.skips = out;
            }
            for (i, at) in s.info.iter_mut().zip(starts) {
                i[3] = at;
            }
        }
        Ok(s)
    }

    /// The bytes it holds in memory, about.
    pub(crate) fn bytes(&self) -> usize {
        self.pks.len()
            + self.hashes.len() * 16
            + self.pk_at.len() * 4
            + self.terms.len()
            + self.info.len() * 16
            + self.postings.len()
            + self.positions.len()
            + self.skips.len()
    }

    pub(crate) fn docs(&self) -> u32 {
        self.hashes.len() as u32
    }

    pub(crate) fn pk(&self, d: u32) -> &[u8] {
        &self.pks[self.pk_at[d as usize] as usize..self.pk_at[d as usize + 1] as usize]
    }

    pub(crate) fn terms(&self) -> usize {
        self.info.len()
    }

    pub(crate) fn term(&self, t: usize) -> &[u8] {
        &self.terms[self.term_at[t] as usize..self.term_at[t + 1] as usize]
    }

    /// Documents term `t` is in.
    pub(crate) fn df(&self, t: usize) -> u32 {
        self.info[t][0]
    }

    /// The term's number.
    pub(crate) fn find(&self, term: &[u8]) -> Option<usize> {
        let (mut lo, mut hi) = (0, self.terms());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.term(mid).cmp(term) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// The number of the first term at or after `term`.
    pub(crate) fn first_from(&self, term: &[u8]) -> usize {
        let (mut lo, mut hi) = (0, self.terms());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.term(mid) < term {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// The terms starting with `prefix`: a range of numbers.
    pub(crate) fn with_prefix(&self, prefix: &[u8]) -> std::ops::Range<usize> {
        let lower = |f: &dyn Fn(&[u8]) -> bool| {
            let (mut lo, mut hi) = (0, self.terms());
            while lo < hi {
                let mid = (lo + hi) / 2;
                if f(self.term(mid)) {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo
        };
        let start = lower(&|t| t < prefix);
        let end = lower(&|t| t < prefix || t.starts_with(prefix));
        start..end
    }

    /// Term `t`'s postings, its positions too when `positions`.
    pub(crate) fn postings(&self, t: usize, positions: bool) -> Result<Postings> {
        let [df, start, pos, _] = self.info[t];
        let df = df as usize;
        let mut p = Postings {
            docs: Vec::with_capacity(df),
            freqs: Vec::with_capacity(df),
            ..Postings::default()
        };
        let mut src = self
            .postings
            .get(start as usize..)
            .ok_or_else(|| corrupt("postings past the end"))?;
        let mut prev: i64 = -1;
        let mut buf = [0u32; BLOCK];
        while p.docs.len() < df {
            let n = (df - p.docs.len()).min(BLOCK);
            let [db, fb] = src
                .get(..2)
                .ok_or_else(|| corrupt("a block cut short"))?
                .try_into()
                .expect("2");
            src = &src[2..];
            let used = unpack(src, db, &mut buf[..n])?;
            src = &src[used..];
            for &d in &buf[..n] {
                prev += i64::from(d) + 1;
                p.docs.push(prev as u32);
            }
            let used = unpack(src, fb, &mut buf[..n])?;
            src = &src[used..];
            p.freqs.extend(buf[..n].iter().map(|f| f + 1));
        }
        if p.docs.last().is_some_and(|&d| d >= self.docs()) {
            return Err(corrupt("a posting past the documents"));
        }
        if positions {
            let mut r = Reader(
                self.positions
                    .get(pos as usize..)
                    .ok_or_else(|| corrupt("positions past the end"))?,
            );
            p.at.reserve(df + 1);
            p.at.push(0);
            for &f in &p.freqs {
                let mut last = 0u32;
                for _ in 0..f {
                    last = last
                        .checked_add(r.varint32()?)
                        .ok_or_else(|| corrupt("a position"))?;
                    p.pos.push(last);
                }
                p.at.push(p.pos.len() as u32);
            }
        }
        Ok(p)
    }

    /// Where term `t`'s one block starts in the postings, if it has one
    /// (no skip list).
    pub(crate) fn one_block(&self, t: usize) -> Option<usize> {
        let [df, start, _, _] = self.info[t];
        (df as usize <= BLOCK).then_some(start as usize)
    }

    /// Term `t`'s blocks without decoding them, each its last document,
    /// where it starts in the postings and its impacts ([`impacts`]) —
    /// `false` for a term in one block (no skip list: decode it).
    pub(crate) fn blocks(
        &self,
        t: usize,
        mut f: impl FnMut(u32, usize, &[(u32, u32)]),
    ) -> Result<bool> {
        let [df, start, _, skip] = self.info[t];
        if df as usize <= BLOCK {
            return Ok(false);
        }
        let mut r = Reader(
            self.skips
                .get(skip as usize..)
                .ok_or_else(|| corrupt("a skip list past the end"))?,
        );
        let (mut last, mut at) = (0u32, start as usize);
        let mut pairs = Vec::with_capacity(16);
        for _ in 0..(df as usize).div_ceil(BLOCK) {
            last = last
                .checked_add(r.varint32()?)
                .filter(|&l| l < self.docs())
                .ok_or_else(|| corrupt("a skip list's document"))?;
            let size = r.varint32()? as usize;
            let n = r.varint32()? as usize;
            if n > BLOCK {
                return Err(corrupt("a block's impacts"));
            }
            pairs.clear();
            let (mut pf, mut pl) = (0u32, 0u32);
            for _ in 0..n {
                pf = pf.saturating_add(r.varint32()?);
                pl = pl.saturating_add(r.varint32()?);
                pairs.push((pf, pl));
            }
            f(last, at, &pairs);
            at += size;
        }
        Ok(true)
    }

    /// The block at `at` in the postings, after document `prev` (−1: a
    /// term's first): its documents and their frequencies, as many as
    /// `docs` holds; the bytes it took.
    pub(crate) fn block(
        &self,
        at: usize,
        prev: i64,
        docs: &mut [u32],
        freqs: &mut [u32],
    ) -> Result<usize> {
        let n = docs.len();
        let src = self
            .postings
            .get(at..)
            .ok_or_else(|| corrupt("postings past the end"))?;
        let [db, fb] = src
            .get(..2)
            .ok_or_else(|| corrupt("a block cut short"))?
            .try_into()
            .expect("2");
        let used = unpack(&src[2..], db, docs)?;
        let mut d = prev;
        for x in docs.iter_mut() {
            d += i64::from(*x) + 1;
            *x = d as u32;
        }
        if d >= i64::from(self.docs()) {
            return Err(corrupt("a posting past the documents"));
        }
        let used2 = unpack(&src[2 + used..], fb, &mut freqs[..n])?;
        for f in freqs.iter_mut() {
            *f += 1;
        }
        Ok(2 + used + used2)
    }

    /// The first document with this primary key and hash that `skip`
    /// does not pass over.
    pub(crate) fn find_doc(&self, pk: &[u8], hash: u64, skip: impl Fn(u32) -> bool) -> Option<u32> {
        let by = self.by_pk.get_or_init(|| {
            let mut m: HashMap<u64, Vec<u32>, MixState> = HashMap::default();
            for d in 0..self.docs() {
                m.entry(pk_key(self.pk(d), self.hashes[d as usize]))
                    .or_default()
                    .push(d);
            }
            m
        });
        by.get(&pk_key(pk, hash))?
            .iter()
            .copied()
            .find(|&d| self.pk(d) == pk && self.hashes[d as usize] == hash && !skip(d))
    }
}

/// Segments `parts` (each with the documents to keep, by bit) as one:
/// documents in order, the dropped ones left out — the terms merged in
/// runs on up to `workers` threads (split at terms of the part with the
/// most), the runs joined in order.  `weighted`: the segments' documents
/// are (their frequencies weights, no positions).
pub(crate) fn merge(
    parts: &[(&Segment, Option<&[u64]>)],
    workers: usize,
    weighted: bool,
) -> Result<Vec<u8>> {
    let mut b = Builder::default();
    // Each part's documents' new numbers.
    let mut maps: Vec<Vec<Option<u32>>> = Vec::with_capacity(parts.len());
    for (s, dead) in parts {
        let mut map = Vec::with_capacity(s.docs() as usize);
        for d in 0..s.docs() {
            if dead.is_some_and(|bits| bit(bits, d)) {
                map.push(None);
            } else {
                map.push(Some(b.docs()));
                b.doc(s.pk(d), s.hashes[d as usize], s.lens[d as usize]);
            }
        }
        maps.push(map);
    }
    let most = parts.iter().map(|p| p.0).max_by_key(|s| s.terms());
    let n = most.map_or(0, Segment::terms);
    let runs = workers.min(n / MERGE_RUN).max(1);
    let cut = |r: usize| (r > 0 && r < runs).then(|| most.expect("a part").term(r * n / runs));
    let lens = &b.lens;
    let run = |r: usize| -> Result<Terms> {
        let (from, to) = (cut(r), cut(r + 1));
        let mut t = Terms::default();
        let mut at: Vec<usize> = parts
            .iter()
            .map(|(s, _)| from.map_or(0, |x| s.first_from(x)))
            .collect();
        let end: Vec<usize> = parts
            .iter()
            .map(|(s, _)| to.map_or(s.terms(), |x| s.first_from(x)))
            .collect();
        loop {
            let next = parts
                .iter()
                .enumerate()
                .filter(|(k, _)| at[*k] < end[*k])
                .map(|(k, (s, _))| s.term(at[k]))
                .min();
            let Some(term) = next.map(<[u8]>::to_vec) else {
                break;
            };
            let mut out = Postings::default();
            for (k, (s, _)) in parts.iter().enumerate() {
                if at[k] < end[k] && s.term(at[k]) == term.as_slice() {
                    let p = s.postings(at[k], !weighted)?;
                    for (i, &d) in p.docs.iter().enumerate() {
                        if let Some(n) = maps[k][d as usize] {
                            if weighted {
                                out.push_weighted(n, p.freqs[i]);
                            } else {
                                out.push(n, p.positions(i));
                            }
                        }
                    }
                    at[k] += 1;
                }
            }
            t.add(&term, &out, lens)?;
        }
        Ok(t)
    };
    let done: Vec<Result<Terms>> = if runs == 1 {
        vec![run(0)]
    } else {
        std::thread::scope(|sc| {
            let hs: Vec<_> = (0..runs).map(|r| sc.spawn(move || run(r))).collect();
            hs.into_iter()
                .map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
                .collect()
        })
    };
    let mut t = Terms::default();
    for d in done {
        t.join(d?)?;
    }
    b.t = t;
    Ok(b.encode())
}

/// Terms a run of a merge takes at least.
const MERGE_RUN: usize = 4096;

pub(crate) fn bit(bits: &[u64], d: u32) -> bool {
    bits.get(d as usize / 64)
        .is_some_and(|w| w >> (d % 64) & 1 == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(terms: &[(&str, &[u32])]) -> Doc {
        Doc {
            len: terms.iter().map(|t| t.1.len() as u32).sum(),
            terms: terms
                .iter()
                .map(|(t, p)| (t.to_string(), p.to_vec()))
                .collect(),
            weights: Vec::new(),
        }
    }

    #[test]
    fn weights_as_frequencies() {
        let xs = [-3e38f32, -2.5, -1e-30, 1e-30, 0.5, 1.0, 2.5, 3e38];
        for w in xs.windows(2) {
            assert!(weight_bits(w[0]) < weight_bits(w[1]), "{w:?}");
        }
        for &x in &xs {
            assert!(weight_bits(x) > 0);
            assert_eq!(of_weight_bits(weight_bits(x)).to_bits(), x.to_bits());
        }
        // A weighted document through the delta's encoding, and a text's
        // alike as before.
        let d = Doc {
            len: 2,
            terms: vec![
                ("0000002a".into(), Vec::new()),
                ("000000ff".into(), Vec::new()),
            ],
            weights: vec![weight_bits(0.25), weight_bits(-4.0)],
        };
        for d in [d, doc(&[("a", &[0, 3]), ("b", &[1])])] {
            let mut b = Vec::new();
            d.encode(&mut b);
            assert_eq!(Doc::decode(&mut Reader(&b)).unwrap(), d);
        }
    }

    #[test]
    fn packs_and_unpacks() {
        for b in 0..=32u8 {
            let max = if b == 32 { u32::MAX } else { (1u32 << b) - 1 };
            let vals: Vec<u32> = (0..BLOCK as u32)
                .map(|i| i.wrapping_mul(2_654_435_761) & max)
                .collect();
            let mut out = Vec::new();
            pack(&vals, b, &mut out);
            let mut got = vec![0; vals.len()];
            assert_eq!(unpack(&out, b, &mut got).unwrap(), out.len());
            assert_eq!(got, vals, "{b} bits");
        }
    }

    #[test]
    fn a_segment_round_trips() {
        // Many documents: several blocks, gaps of every size.
        let mut docs = Vec::new();
        for i in 0..1000u32 {
            let mut terms: Vec<(&str, &[u32])> = vec![("all", &[0])];
            if i % 3 == 0 {
                terms.push(("three", &[1, 5, 9]));
            }
            if i == 999 || i == 7 {
                terms.push(("rare", &[2]));
            }
            terms.sort();
            docs.push((i.to_be_bytes().to_vec(), u64::from(i) * 7, doc(&terms)));
        }
        let s = Segment::decode(&of_docs(&docs).unwrap()).unwrap();
        assert_eq!(s.docs(), 1000);
        assert_eq!(s.terms(), 3);
        let t = s.find(b"three").unwrap();
        let p = s.postings(t, true).unwrap();
        assert_eq!(p.docs, (0..1000).step_by(3).collect::<Vec<_>>());
        assert!(p.freqs.iter().all(|&f| f == 3));
        assert_eq!(p.positions(5), [1, 5, 9]);
        let p = s.postings(s.find(b"rare").unwrap(), false).unwrap();
        assert_eq!(p.docs, [7, 999]);
        assert_eq!(s.with_prefix(b"r"), 1..2);
        assert_eq!(s.with_prefix(b"t"), 2..3);
        assert_eq!(s.with_prefix(b"x"), 3..3);
        assert!(s.find(b"none").is_none());
        assert_eq!(s.find_doc(&5u32.to_be_bytes(), 35, |_| false), Some(5));
        assert_eq!(s.find_doc(&5u32.to_be_bytes(), 36, |_| false), None);
        assert_eq!(s.total, 1000 + 334 * 3 + 2);

        // Merged with itself, every other document dropped from the first.
        let dead: Vec<u64> = vec![0x5555_5555_5555_5555; 16];
        let m =
            Segment::decode(&merge(&[(&s, Some(&dead)), (&s, None)], 1, false).unwrap()).unwrap();
        assert_eq!(m.docs(), 1500);
        let p = m.postings(m.find(b"rare").unwrap(), true).unwrap();
        assert_eq!(p.docs, [3, 499, 500 + 7, 500 + 999]);
        assert_eq!(m.pk(0), 1u32.to_be_bytes());
    }

    /// `s` as version 1 wrote it: no skip lists.
    fn version_1(s: &Segment) -> Vec<u8> {
        let mut out = vec![1u8];
        out.extend_from_slice(&s.docs().to_le_bytes());
        out.extend_from_slice(&(s.terms() as u32).to_le_bytes());
        out.extend_from_slice(&s.total.to_le_bytes());
        for x in &s.pk_at {
            out.extend_from_slice(&x.to_le_bytes());
        }
        out.extend_from_slice(&s.pks);
        for h in &s.hashes {
            out.extend_from_slice(&h.to_le_bytes());
        }
        for l in &s.lens {
            out.extend_from_slice(&l.to_le_bytes());
        }
        for x in &s.term_at {
            out.extend_from_slice(&x.to_le_bytes());
        }
        out.extend_from_slice(&s.terms);
        for i in &s.info {
            for x in &i[..3] {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        for b in [&s.postings, &s.positions] {
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
        out
    }

    #[test]
    fn skip_lists_reach_and_bound_every_block() {
        // Lengths and frequencies that vary, over many blocks.
        let mut docs = Vec::new();
        for i in 0..2000u32 {
            let pad: Vec<u32> = (10..11 + i * 7 % 23).collect();
            let many: Vec<u32> = (1..2 + i * 13 % 5).collect();
            let mut terms: Vec<(&str, &[u32])> = vec![("all", &many), ("pad", &pad)];
            if i % 3 == 0 {
                terms.push(("three", &[0]));
            }
            terms.sort();
            docs.push((i.to_be_bytes().to_vec(), u64::from(i), doc(&terms)));
        }
        let s = Segment::decode(&of_docs(&docs).unwrap()).unwrap();
        let old = Segment::decode(&version_1(&s)).unwrap();
        for term in ["all", "three", "pad"] {
            let t = s.find(term.as_bytes()).unwrap();
            let p = s.postings(t, false).unwrap();
            let mut seen = Vec::new();
            assert!(
                s.blocks(t, |last, at, imp| seen.push((last, at, imp.to_vec())))
                    .unwrap()
            );
            let mut again = Vec::new();
            assert!(
                old.blocks(t, |last, at, imp| again.push((last, at, imp.to_vec())))
                    .unwrap()
            );
            assert_eq!(seen, again, "{term}: version 1's skip list");
            let chunks: Vec<_> = p.docs.chunks(BLOCK).zip(p.freqs.chunks(BLOCK)).collect();
            assert_eq!(seen.len(), chunks.len());
            let mut prev = -1i64;
            for ((last, at, imp), (ds, fs)) in seen.into_iter().zip(chunks) {
                assert_eq!(last, *ds.last().unwrap());
                let (mut d, mut f) = (vec![0; ds.len()], vec![0; ds.len()]);
                s.block(at, prev, &mut d, &mut f).unwrap();
                assert_eq!((d.as_slice(), f.as_slice()), (ds, fs));
                prev = i64::from(last);
                // Every document's pair is beaten or met by an impact,
                // and every impact is a document's.
                let pairs: Vec<(u32, u32)> = ds
                    .iter()
                    .zip(fs)
                    .map(|(&d, &f)| (f, s.lens[d as usize]))
                    .collect();
                for &(f, len) in &pairs {
                    assert!(imp.iter().any(|&(g, l)| g >= f && l <= len));
                }
                assert!(imp.iter().all(|x| pairs.contains(x)));
                assert!(imp.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1));
            }
        }
        // A term in one block has none.
        let mut one = vec![(b"k".to_vec(), 1, doc(&[("x", &[0])]))];
        one[0].2.len = 1;
        let s = Segment::decode(&of_docs(&one).unwrap()).unwrap();
        assert!(!s.blocks(0, |_, _, _| unreachable!()).unwrap());
    }

    #[test]
    fn a_merge_in_runs_is_a_merge() {
        // Terms enough for runs: each document's own, and shared ones.
        let mk = |from: u32| {
            let own: Vec<String> = (from..from + 6000).map(|i| format!("t{i:06}")).collect();
            let docs: Vec<(Vec<u8>, u64, Doc)> = own
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let mut terms: Vec<(&str, &[u32])> = vec![(t.as_str(), &[0]), ("all", &[1, 2])];
                    if i % 7 == 0 {
                        terms.push(("seven", &[3]));
                    }
                    terms.sort();
                    (
                        (from + i as u32).to_be_bytes().to_vec(),
                        u64::from(i as u32),
                        doc(&terms),
                    )
                })
                .collect();
            Segment::decode(&of_docs(&docs).unwrap()).unwrap()
        };
        let (a, b, c) = (mk(0), mk(3000), mk(20_000));
        let dead: Vec<u64> = vec![0x0101_0101_0101_0101; 100];
        let parts = [(&a, Some(dead.as_slice())), (&b, None), (&c, None)];
        let one = merge(&parts, 1, false).unwrap();
        for w in [2, 3, 4, 8] {
            assert!(one == merge(&parts, w, false).unwrap(), "{w} runs");
        }
        let m = Segment::decode(&one).unwrap();
        // 15,000 terms of their own, less the 375 only dead documents
        // held (every eighth of the first's 3,000), and two shared.
        assert_eq!(m.terms(), 15_000 - 375 + 2);
        assert!(m.find(b"t000008").is_none());
        let p = m.postings(m.find(b"t004001").unwrap(), true).unwrap();
        assert_eq!(p.docs.len(), 2);
    }

    #[test]
    fn an_empty_segment() {
        let s = Segment::decode(&of_docs(&[]).unwrap()).unwrap();
        assert_eq!((s.docs(), s.terms()), (0, 0));
        assert_eq!(s.with_prefix(b""), 0..0);
    }
}
