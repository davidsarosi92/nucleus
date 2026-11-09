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

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use nucleus_kernels::MixState;

use crate::error::{Error, Result};
use crate::text::{Analyzer, Token};

/// Documents a block holds.
pub(crate) const BLOCK: usize = 128;
const VERSION: u8 = 1;

pub(crate) fn corrupt(what: &str) -> Error {
    Error::internal(format!("a text index is damaged: {what}"))
}

/// A document's terms, as an index keeps them: its length (terms counted
/// with repeats), and each distinct term with its positions, terms sorted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Doc {
    pub(crate) len: u32,
    pub(crate) terms: Vec<(String, Vec<u32>)>,
}

impl Doc {
    /// `text` analyzed by `a`.
    pub(crate) fn analyze(a: &dyn Analyzer, text: &str) -> Doc {
        let mut by: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        let mut len = 0u32;
        a.analyze(text, &mut |t: Token| {
            len += 1;
            by.entry(t.term).or_default().push(t.position);
        });
        Doc {
            len,
            terms: by.into_iter().collect(),
        }
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        varint(out, u64::from(self.len));
        varint(out, self.terms.len() as u64);
        for (t, ps) in &self.terms {
            varint(out, t.len() as u64);
            out.extend_from_slice(t.as_bytes());
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
        for _ in 0..n {
            let tl = r.varint32()? as usize;
            let t = std::str::from_utf8(r.take(tl)?)
                .map_err(|_| corrupt("a term not UTF-8"))?
                .to_string();
            let f = r.varint32()? as usize;
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
        Ok(Doc { len, terms })
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
        &self.pos[self.at[i] as usize..self.at[i + 1] as usize]
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
    term_at: Vec<u32>,
    terms: Vec<u8>,
    info: Vec<[u32; 3]>,
    postings: Vec<u8>,
    positions: Vec<u8>,
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
        if p.docs.is_empty() {
            return Ok(());
        }
        if self.term_at.is_empty() {
            self.term_at.push(0);
        }
        let offset =
            |n: usize| u32::try_from(n).map_err(|_| Error::internal("a segment past 4 GB"));
        self.info.push([
            p.docs.len() as u32,
            offset(self.postings.len())?,
            offset(self.positions.len())?,
        ]);
        self.terms.extend_from_slice(term);
        self.term_at.push(offset(self.terms.len())?);
        let mut prev: i64 = -1;
        let mut deltas = [0u32; BLOCK];
        let mut freqs = [0u32; BLOCK];
        for (docs, fs) in p.docs.chunks(BLOCK).zip(p.freqs.chunks(BLOCK)) {
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

    /// Documents added so far.
    pub(crate) fn docs(&self) -> u32 {
        self.hashes.len() as u32
    }

    pub(crate) fn encode(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            64 + self.pks.len()
                + self.hashes.len() * 16
                + self.terms.len()
                + self.info.len() * 16
                + self.postings.len()
                + self.positions.len(),
        );
        out.push(VERSION);
        let docs = self.hashes.len() as u32;
        out.extend_from_slice(&docs.to_le_bytes());
        out.extend_from_slice(&(self.info.len() as u32).to_le_bytes());
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
        for x in at(&self.term_at, self.info.len() as u32) {
            out.extend_from_slice(&x.to_le_bytes());
        }
        out.extend_from_slice(&self.terms);
        for i in &self.info {
            for x in i {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        out.extend_from_slice(&(self.postings.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.postings);
        out.extend_from_slice(&(self.positions.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.positions);
        out
    }
}

/// `docs` (primary key, hash, terms) as a segment, numbered in order.
pub(crate) fn of_docs(docs: &[(Vec<u8>, u64, Doc)]) -> Result<Vec<u8>> {
    let mut b = Builder::default();
    let mut by: BTreeMap<&str, Postings> = BTreeMap::new();
    for (n, (pk, hash, doc)) in docs.iter().enumerate() {
        b.doc(pk, *hash, doc.len);
        for (t, ps) in &doc.terms {
            by.entry(t.as_str()).or_default().push(n as u32, ps);
        }
    }
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
    /// Per term: documents, where its postings start, its positions.
    info: Vec<[u32; 3]>,
    postings: Vec<u8>,
    positions: Vec<u8>,
    /// Documents by primary key and hash ([`Segment::find_doc`]).
    by_pk: OnceLock<HashMap<u64, Vec<u32>, MixState>>,
}

fn pk_key(pk: &[u8], hash: u64) -> u64 {
    crate::object_cache::object_id(11, pk) ^ hash
}

impl Segment {
    pub(crate) fn decode(b: &[u8]) -> Result<Segment> {
        let mut r = Reader(b);
        if r.u8()? != VERSION {
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
        let flat = r.u32s(nterms * 3)?;
        let info = flat.as_chunks::<3>().0.to_vec();
        let n = r.u32()? as usize;
        let postings = r.take(n)?.to_vec();
        let n = r.u32()? as usize;
        let positions = r.take(n)?.to_vec();
        if !r.0.is_empty() {
            return Err(corrupt("a segment with bytes after it"));
        }
        Ok(Segment {
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
            by_pk: OnceLock::new(),
        })
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
        let [df, start, pos] = self.info[t];
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
/// documents in order, the dropped ones left out.
pub(crate) fn merge(parts: &[(&Segment, Option<&[u64]>)]) -> Result<Vec<u8>> {
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
    // The terms of every part, merged in order.
    let mut at = vec![0usize; parts.len()];
    loop {
        let next = parts
            .iter()
            .zip(&at)
            .filter(|((s, _), i)| **i < s.terms())
            .map(|((s, _), i)| s.term(*i))
            .min();
        let Some(term) = next.map(<[u8]>::to_vec) else {
            break;
        };
        let mut out = Postings::default();
        for (k, (s, _)) in parts.iter().enumerate() {
            if at[k] < s.terms() && s.term(at[k]) == term.as_slice() {
                let p = s.postings(at[k], true)?;
                for (i, &d) in p.docs.iter().enumerate() {
                    if let Some(n) = maps[k][d as usize] {
                        out.push(n, p.positions(i));
                    }
                }
                at[k] += 1;
            }
        }
        b.term(&term, &out)?;
    }
    Ok(b.encode())
}

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
        let m = Segment::decode(&merge(&[(&s, Some(&dead)), (&s, None)]).unwrap()).unwrap();
        assert_eq!(m.docs(), 1500);
        let p = m.postings(m.find(b"rare").unwrap(), true).unwrap();
        assert_eq!(p.docs, [3, 499, 500 + 7, 500 + 999]);
        assert_eq!(m.pk(0), 1u32.to_be_bytes());
    }

    #[test]
    fn an_empty_segment() {
        let s = Segment::decode(&of_docs(&[]).unwrap()).unwrap();
        assert_eq!((s.docs(), s.terms()), (0, 0));
        assert_eq!(s.with_prefix(b""), 0..0);
    }
}
