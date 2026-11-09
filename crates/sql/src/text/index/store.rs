//! A text index's storage on the KV (PLAN.md §M14.21 T4): model (C), as
//! the HNSW index's (§M14.12 V4) — but exact in every snapshot.
//!
//! A write puts a *delta* key (primary key, nonce: the row's terms) or,
//! for a row deleted or changed, a *tombstone* (primary key, the hash of
//! the text it had, nonce: that text's length and terms) — new keys only,
//! so writers never conflict.  A *seal* moves the delta into a new
//! immutable segment ([`super::segment`]) in one transaction: a tombstone
//! of a row the delta holds takes it out; one of a row in an older
//! segment marks it in that segment's *deleted* object (its documents by
//! bit, and how many of them each term was in — SQLite FTS5's tombstones
//! applied as Tantivy's delete bitsets), which the seal writes anew.  A
//! *merge* writes segments of about one size as one, their deleted
//! documents left out (logarithmic: eight at a time, as Tantivy's).
//!
//! A snapshot reads the root, the segments and their deleted objects
//! (decoded once, from memory: [`crate::object_cache`]) and the delta.  A
//! row is there as many times as it was inserted less the times it was
//! deleted, wherever each of those is — so what a snapshot finds, and the
//! statistics it counts (documents, their total length, each term's
//! documents), are those of its rows, whatever the seals and merges did.
//!
//! Keys under the index's prefix: `r` the root; `o` + id + part an
//! object's part (of 1 MiB); `d` + primary key + nonce a delta row; `x`
//! + primary key + text hash + nonce a tombstone.

use std::collections::HashMap;
use std::ops::Bound;
use std::sync::Arc;

use nucleus_kernels::MixState;
use nucleus_kv::{KvTxn, prefix_end};

use super::segment::{self, Doc, Postings, Reader, Segment, bit, corrupt, varint};
use crate::error::Result;
use crate::object_cache::{self as cache, Kind};
use crate::text::query::{Leaf, Node, Query, eval, within};

/// An object's parts are at most this long.
const PART: usize = 1 << 20;
/// Segments of a tier merged together.
pub(crate) const MERGE: usize = 8;
const ROOT_VERSION: u8 = 1;

/// A new object's id: a nonce — never 0, which means none ([`SegRef`]).
/// An object is written once under its id, so two objects never share
/// one (a content hash would: a segment written again as it was, after
/// its rows were deleted and inserted again).
fn new_id() -> u64 {
    loop {
        let n = crate::hnsw::store::nonce();
        if n != 0 {
            return n;
        }
    }
}

/// The keys of one index.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Keys(Vec<u8>);

impl Keys {
    pub(crate) fn new(prefix: Vec<u8>) -> Self {
        Keys(prefix)
    }

    fn tagged(&self, tag: u8) -> Vec<u8> {
        let mut k = self.0.clone();
        k.push(tag);
        k
    }

    fn root(&self) -> Vec<u8> {
        self.tagged(b'r')
    }

    fn obj(&self, id: u64) -> Vec<u8> {
        let mut k = self.tagged(b'o');
        k.extend_from_slice(&id.to_be_bytes());
        k
    }

    pub(crate) fn delta(&self, pk: &[u8], nonce: u64) -> Vec<u8> {
        let mut k = self.tagged(b'd');
        k.extend_from_slice(pk);
        k.extend_from_slice(&nonce.to_be_bytes());
        k
    }

    pub(crate) fn tombstone(&self, pk: &[u8], hash: u64, nonce: u64) -> Vec<u8> {
        let mut k = self.tagged(b'x');
        k.extend_from_slice(pk);
        k.extend_from_slice(&hash.to_be_bytes());
        k.extend_from_slice(&nonce.to_be_bytes());
        k
    }

    pub(crate) fn range(&self, tag: u8) -> (Vec<u8>, Vec<u8>) {
        let start = self.tagged(tag);
        let end = prefix_end(&start).expect("a tagged prefix");
        (start, end)
    }

    fn put_obj(&self, txn: &mut dyn KvTxn, id: u64, b: &[u8]) -> Result<()> {
        let base = self.obj(id);
        for (n, part) in b.chunks(PART).enumerate() {
            let mut k = base.clone();
            k.extend_from_slice(&(n as u32).to_be_bytes());
            txn.put(&k, part)?;
        }
        if b.is_empty() {
            let mut k = base;
            k.extend_from_slice(&0u32.to_be_bytes());
            txn.put(&k, &[])?;
        }
        Ok(())
    }

    fn get_obj(&self, txn: &dyn KvTxn, id: u64) -> Result<Vec<u8>> {
        let start = self.obj(id);
        let end = prefix_end(&start).expect("an object prefix");
        let mut out = Vec::new();
        let mut parts = 0;
        for e in txn.scan(Bound::Included(&start), Bound::Excluded(&end))? {
            out.extend_from_slice(&e?.1);
            parts += 1;
        }
        if parts == 0 {
            return Err(corrupt("an object missing"));
        }
        Ok(out)
    }

    fn delete_obj(&self, txn: &mut dyn KvTxn, id: u64) -> Result<()> {
        let start = self.obj(id);
        let end = prefix_end(&start).expect("an object prefix");
        let keys: Vec<Vec<u8>> = txn
            .scan(Bound::Included(&start), Bound::Excluded(&end))?
            .map(|e| e.map(|(k, _)| k))
            .collect::<std::result::Result<_, _>>()?;
        for k in keys {
            txn.delete(&k)?;
        }
        Ok(())
    }
}

/// A segment as the root lists it: its object, documents, and its deleted
/// object (0: none deleted).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SegRef {
    pub(crate) id: u64,
    pub(crate) docs: u32,
    pub(crate) deleted: u64,
    pub(crate) dead: u32,
}

/// The root: the segments, the oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Root {
    pub(crate) segments: Vec<SegRef>,
}

impl Root {
    fn encode(&self) -> Vec<u8> {
        let mut out = vec![ROOT_VERSION];
        out.extend_from_slice(&(self.segments.len() as u32).to_le_bytes());
        for s in &self.segments {
            out.extend_from_slice(&s.id.to_le_bytes());
            out.extend_from_slice(&s.docs.to_le_bytes());
            out.extend_from_slice(&s.deleted.to_le_bytes());
            out.extend_from_slice(&s.dead.to_le_bytes());
        }
        out
    }

    fn decode(b: &[u8]) -> Result<Root> {
        let mut r = Reader(b);
        if r.u8()? != ROOT_VERSION {
            return Err(corrupt("a root of another version"));
        }
        let n = r.u32()? as usize;
        let mut segments = Vec::with_capacity(n.min(1 << 16));
        for _ in 0..n {
            segments.push(SegRef {
                id: r.u64()?,
                docs: r.u32()?,
                deleted: r.u64()?,
                dead: r.u32()?,
            });
        }
        Ok(Root { segments })
    }
}

/// A segment's deleted documents: by bit, how many, their total length,
/// and per term how many of them it was in (sorted by term).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Deleted {
    pub(crate) bits: Vec<u64>,
    pub(crate) count: u32,
    pub(crate) len: u64,
    pub(crate) terms: Vec<(Vec<u8>, u32)>,
}

impl Deleted {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&self.len.to_le_bytes());
        out.extend_from_slice(&(self.bits.len() as u32).to_le_bytes());
        for w in &self.bits {
            out.extend_from_slice(&w.to_le_bytes());
        }
        varint(&mut out, self.terms.len() as u64);
        for (t, n) in &self.terms {
            varint(&mut out, t.len() as u64);
            out.extend_from_slice(t);
            varint(&mut out, u64::from(*n));
        }
        out
    }

    fn decode(b: &[u8]) -> Result<Deleted> {
        let mut r = Reader(b);
        let count = r.u32()?;
        let len = r.u64()?;
        let words = r.u32()? as usize;
        let mut bits = Vec::with_capacity(words.min(1 << 20));
        for _ in 0..words {
            bits.push(r.u64()?);
        }
        let n = r.varint()? as usize;
        let mut terms = Vec::with_capacity(n.min(1 << 20));
        for _ in 0..n {
            let l = r.varint()? as usize;
            let t = r.take(l)?.to_vec();
            terms.push((t, r.varint32()?));
        }
        Ok(Deleted {
            bits,
            count,
            len,
            terms,
        })
    }

    pub(crate) fn term(&self, t: &[u8]) -> u32 {
        self.terms
            .binary_search_by(|(x, _)| x.as_slice().cmp(t))
            .map_or(0, |i| self.terms[i].1)
    }

    fn bytes(&self) -> usize {
        self.bits.len() * 8 + self.terms.iter().map(|t| t.0.len() + 24).sum::<usize>()
    }

    /// Document `d`, of `len` terms and these distinct ones, deleted too.
    fn add(&mut self, d: u32, len: u32, terms: &[Vec<u8>]) {
        let w = d as usize / 64;
        if self.bits.len() <= w {
            self.bits.resize(w + 1, 0);
        }
        self.bits[w] |= 1 << (d % 64);
        self.count += 1;
        self.len += u64::from(len);
        for t in terms {
            match self.terms.binary_search_by(|(x, _)| x.as_slice().cmp(t)) {
                Ok(i) => self.terms[i].1 += 1,
                Err(i) => self.terms.insert(i, (t.clone(), 1)),
            }
        }
    }
}

/// A row as the delta has it.
pub(crate) struct Insert {
    pub(crate) pk: Vec<u8>,
    pub(crate) hash: u64,
    pub(crate) doc: Doc,
}

/// A row's text gone: its length and distinct terms.
pub(crate) struct Gone {
    pub(crate) pk: Vec<u8>,
    pub(crate) hash: u64,
    pub(crate) len: u32,
    pub(crate) terms: Vec<Vec<u8>>,
}

pub(crate) fn encode_insert(hash: u64, doc: &Doc) -> Vec<u8> {
    let mut out = hash.to_le_bytes().to_vec();
    doc.encode(&mut out);
    out
}

pub(crate) fn encode_gone(doc: &Doc) -> Vec<u8> {
    let mut out = Vec::new();
    varint(&mut out, u64::from(doc.len));
    varint(&mut out, doc.terms.len() as u64);
    for (t, _) in &doc.terms {
        varint(&mut out, t.len() as u64);
        out.extend_from_slice(t.as_bytes());
    }
    out
}

/// The delta a snapshot sees, and its keys.
#[derive(Default)]
pub(crate) struct Delta {
    pub(crate) inserts: Vec<Insert>,
    pub(crate) gone: Vec<Gone>,
    keys: Vec<Vec<u8>>,
}

/// One index: its keys.
pub(crate) struct Store {
    pub(crate) keys: Keys,
}

/// A segment and its deleted documents, as a snapshot sees them.
pub(crate) struct Seg {
    pub(crate) r: SegRef,
    pub(crate) seg: Arc<Segment>,
    pub(crate) deleted: Option<Arc<Deleted>>,
}

impl Seg {
    fn dead(&self, d: u32) -> bool {
        self.deleted.as_ref().is_some_and(|x| bit(&x.bits, d))
    }
}

/// An index as a snapshot sees it.
pub(crate) struct View {
    pub(crate) root: Root,
    pub(crate) segs: Vec<Seg>,
    pub(crate) delta: Delta,
}

/// The statistics BM25 reads, exact for a snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    /// Documents.
    pub(crate) docs: u64,
    /// Their total length in terms.
    pub(crate) len: u64,
    /// For each term asked for, the documents it is in.
    pub(crate) df: Vec<u64>,
}

impl Store {
    pub(crate) fn root(&self, txn: &dyn KvTxn) -> Result<Root> {
        match txn.get(&self.keys.root())? {
            Some(b) => Root::decode(&b),
            None => Ok(Root::default()),
        }
    }

    fn segment(&self, txn: &dyn KvTxn, id: u64) -> Result<Arc<Segment>> {
        cache::get(Kind::TextSegment, id, || {
            let s = Segment::decode(&self.keys.get_obj(txn, id)?)?;
            let n = s.bytes();
            Ok((s, n))
        })
    }

    fn deleted(&self, txn: &dyn KvTxn, id: u64) -> Result<Arc<Deleted>> {
        cache::get(Kind::TextDeleted, id, || {
            let d = Deleted::decode(&self.keys.get_obj(txn, id)?)?;
            let n = d.bytes();
            Ok((d, n))
        })
    }

    pub(crate) fn delta(&self, txn: &dyn KvTxn) -> Result<Delta> {
        let mut d = Delta::default();
        let p = self.keys.0.len() + 1;
        let (start, end) = self.keys.range(b'd');
        for e in txn.scan(Bound::Included(&start), Bound::Excluded(&end))? {
            let (k, v) = e?;
            if k.len() < p + 8 || v.len() < 8 {
                return Err(corrupt("a delta key"));
            }
            let mut r = Reader(&v);
            let hash = r.u64()?;
            let doc = Doc::decode(&mut r)?;
            d.inserts.push(Insert {
                pk: k[p..k.len() - 8].to_vec(),
                hash,
                doc,
            });
            d.keys.push(k);
        }
        let (start, end) = self.keys.range(b'x');
        for e in txn.scan(Bound::Included(&start), Bound::Excluded(&end))? {
            let (k, v) = e?;
            if k.len() < p + 16 {
                return Err(corrupt("a tombstone key"));
            }
            let n = k.len();
            let hash = u64::from_be_bytes(k[n - 16..n - 8].try_into().expect("8"));
            let mut r = Reader(&v);
            let len = r.varint32()?;
            let count = r.varint()? as usize;
            let mut terms = Vec::with_capacity(count.min(1 << 16));
            for _ in 0..count {
                let l = r.varint()? as usize;
                terms.push(r.take(l)?.to_vec());
            }
            d.gone.push(Gone {
                pk: k[p..n - 16].to_vec(),
                hash,
                len,
                terms,
            });
            d.keys.push(k);
        }
        Ok(d)
    }

    /// The index as `txn` sees it.
    pub(crate) fn view(&self, txn: &dyn KvTxn) -> Result<View> {
        let root = self.root(txn)?;
        let mut segs = Vec::with_capacity(root.segments.len());
        for r in &root.segments {
            segs.push(Seg {
                r: *r,
                seg: self.segment(txn, r.id)?,
                deleted: match r.deleted {
                    0 => None,
                    id => Some(self.deleted(txn, id)?),
                },
            });
        }
        Ok(View {
            root,
            segs,
            delta: self.delta(txn)?,
        })
    }

    /// A segment of `docs` added (a build's batch of rows); the root is
    /// written even with none.
    pub(crate) fn append(&self, txn: &mut dyn KvTxn, docs: &[(Vec<u8>, u64, Doc)]) -> Result<()> {
        let mut root = self.root(&*txn)?;
        if !docs.is_empty() {
            let b = segment::of_docs(docs)?;
            let id = new_id();
            self.keys.put_obj(txn, id, &b)?;
            root.segments.push(SegRef {
                id,
                docs: docs.len() as u32,
                deleted: 0,
                dead: 0,
            });
        }
        Ok(txn.put(&self.keys.root(), &root.encode())?)
    }

    /// Seals the delta `txn` sees: its rows a new segment, its tombstones
    /// applied; the delta's keys (0: none were there).
    pub(crate) fn seal(&self, txn: &mut dyn KvTxn) -> Result<usize> {
        let mut view = self.view(&*txn)?;
        let keys = std::mem::take(&mut view.delta.keys);
        if keys.is_empty() {
            return Ok(0);
        }
        let Delta {
            mut inserts, gone, ..
        } = view.delta;
        // Tombstones of rows the delta holds take them out; the others
        // are rows of the segments.
        let mut marked: Vec<Option<Deleted>> = vec![None; view.segs.len()];
        for g in gone {
            if let Some(i) = inserts
                .iter()
                .position(|x| x.pk == g.pk && x.hash == g.hash)
            {
                inserts.swap_remove(i);
                continue;
            }
            let mut found = false;
            for (s, m) in view.segs.iter().zip(marked.iter_mut()) {
                let d = s.seg.find_doc(&g.pk, g.hash, |d| {
                    s.dead(d) || m.as_ref().is_some_and(|m| bit(&m.bits, d))
                });
                if let Some(d) = d {
                    m.get_or_insert_with(|| s.deleted.as_deref().cloned().unwrap_or_default())
                        .add(d, g.len, &g.terms);
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(corrupt("a deleted row the index does not hold"));
            }
        }
        let mut root = view.root.clone();
        for ((s, m), r) in view.segs.iter().zip(marked).zip(root.segments.iter_mut()) {
            let Some(m) = m else { continue };
            let id = new_id();
            self.keys.put_obj(txn, id, &m.encode())?;
            if s.r.deleted != 0 {
                self.keys.delete_obj(txn, s.r.deleted)?;
            }
            r.deleted = id;
            r.dead = m.count;
        }
        if !inserts.is_empty() {
            // In primary key order: a merge's documents then are too, for
            // runs of one table's rows.
            inserts.sort_by(|a, b| a.pk.cmp(&b.pk));
            let docs: Vec<(Vec<u8>, u64, Doc)> =
                inserts.into_iter().map(|i| (i.pk, i.hash, i.doc)).collect();
            let b = segment::of_docs(&docs)?;
            let id = new_id();
            self.keys.put_obj(txn, id, &b)?;
            root.segments.push(SegRef {
                id,
                docs: docs.len() as u32,
                deleted: 0,
                dead: 0,
            });
        }
        // Segments every document of which is deleted go.
        let mut kept = Vec::with_capacity(root.segments.len());
        for s in root.segments {
            if s.dead == s.docs {
                self.keys.delete_obj(txn, s.id)?;
                if s.deleted != 0 {
                    self.keys.delete_obj(txn, s.deleted)?;
                }
            } else {
                kept.push(s);
            }
        }
        root.segments = kept;
        txn.put(&self.keys.root(), &root.encode())?;
        for k in &keys {
            txn.delete(k)?;
        }
        Ok(keys.len())
    }

    /// One merge, if one is due: [`MERGE`] segments of a tier (their
    /// live documents' count in powers of [`MERGE`]) written as one.
    /// Whether one was.
    pub(crate) fn merge(&self, txn: &mut dyn KvTxn) -> Result<bool> {
        let root = self.root(&*txn)?;
        let tier = |s: &SegRef| {
            let live = u64::from(s.docs - s.dead).max(1);
            live.ilog(MERGE as u64)
        };
        let mut by_tier: HashMap<u32, Vec<usize>> = HashMap::new();
        for (i, s) in root.segments.iter().enumerate() {
            by_tier.entry(tier(s)).or_default().push(i);
        }
        let Some(group) = by_tier
            .into_values()
            .filter(|g| g.len() >= MERGE)
            .min_by_key(|g| g[0])
        else {
            return Ok(false);
        };
        let group = &group[..MERGE];
        self.merge_segments(txn, &root, group)?;
        Ok(true)
    }

    /// Segments `group` of `root` (by position) written as one, in the
    /// first's place.
    fn merge_segments(&self, txn: &mut dyn KvTxn, root: &Root, group: &[usize]) -> Result<()> {
        let mut segs = Vec::with_capacity(group.len());
        for &i in group {
            let r = root.segments[i];
            let deleted = match r.deleted {
                0 => None,
                id => Some(self.deleted(&*txn, id)?),
            };
            segs.push((self.segment(&*txn, r.id)?, deleted));
        }
        let parts: Vec<(&Segment, Option<&[u64]>)> = segs
            .iter()
            .map(|(s, d)| (s.as_ref(), d.as_ref().map(|d| d.bits.as_slice())))
            .collect();
        let b = segment::merge(&parts)?;
        let docs = parts.iter().map(|(s, _)| s.docs()).sum::<u32>()
            - group.iter().map(|&i| root.segments[i].dead).sum::<u32>();
        let id = new_id();
        self.keys.put_obj(txn, id, &b)?;
        for &i in group {
            let r = root.segments[i];
            self.keys.delete_obj(txn, r.id)?;
            if r.deleted != 0 {
                self.keys.delete_obj(txn, r.deleted)?;
            }
        }
        let mut out = Root::default();
        for (i, s) in root.segments.iter().enumerate() {
            if i == group[0] {
                out.segments.push(SegRef {
                    id,
                    docs,
                    deleted: 0,
                    dead: 0,
                });
            } else if !group.contains(&i) {
                out.segments.push(*s);
            }
        }
        Ok(txn.put(&self.keys.root(), &out.encode())?)
    }

    /// Every segment merged into one (tests; `VACUUM`).
    pub(crate) fn merge_all(&self, txn: &mut dyn KvTxn) -> Result<()> {
        let root = self.root(&*txn)?;
        if root.segments.len() > 1 || root.segments.iter().any(|s| s.dead > 0) {
            let all: Vec<usize> = (0..root.segments.len()).collect();
            self.merge_segments(txn, &root, &all)?;
        }
        Ok(())
    }
}

/// Sorted lists of documents.
fn intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

fn union(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (mut i, mut j, mut out) = (0, 0, Vec::with_capacity(a.len() + b.len()));
    while i < a.len() || j < b.len() {
        if j == b.len() || i < a.len() && a[i] < b[j] {
            out.push(a[i]);
            i += 1;
        } else if i == a.len() || b[j] < a[i] {
            out.push(b[j]);
            j += 1;
        } else {
            out.push(a[i]);
            i += 1;
            j += 1;
        }
    }
    out
}

fn minus(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut j = 0;
    a.iter()
        .copied()
        .filter(|&x| {
            while j < b.len() && b[j] < x {
                j += 1;
            }
            j == b.len() || b[j] != x
        })
        .collect()
}

/// Where a leaf is in a segment: the postings of every term it matches,
/// as one (positions merged).
fn leaf_postings(s: &Segment, leaf: &Leaf, positions: bool) -> Result<Postings> {
    let terms: Vec<usize> = match leaf {
        Leaf::Exact(t) => s.find(t.as_bytes()).into_iter().collect(),
        Leaf::Prefix(p) => s.with_prefix(p.as_bytes()).collect(),
        Leaf::Fuzzy(t, k) => (0..s.terms())
            .filter(|&i| std::str::from_utf8(s.term(i)).is_ok_and(|x| within(t, x, *k as usize)))
            .collect(),
    };
    match terms.as_slice() {
        [] => Ok(Postings::default()),
        [t] => s.postings(*t, positions),
        _ => {
            let mut all: Vec<(u32, Vec<u32>)> = Vec::new();
            for &t in &terms {
                let p = s.postings(t, positions)?;
                for (i, &d) in p.docs.iter().enumerate() {
                    let ps = if positions {
                        p.positions(i).to_vec()
                    } else {
                        Vec::new()
                    };
                    all.push((d, ps));
                }
            }
            all.sort_by_key(|x| x.0);
            let mut out = Postings::default();
            out.at.push(0);
            for (d, mut ps) in all {
                if out.docs.last() == Some(&d) {
                    out.pos.append(&mut ps);
                    let from = out.at[out.at.len() - 2] as usize;
                    out.pos[from..].sort_unstable();
                    *out.at.last_mut().expect("one") = out.pos.len() as u32;
                    *out.freqs.last_mut().expect("one") += 1;
                } else {
                    out.docs.push(d);
                    out.freqs.push(1);
                    out.pos.append(&mut ps);
                    out.at.push(out.pos.len() as u32);
                }
            }
            Ok(out)
        }
    }
}

/// Marks the leaves `n` reads positions of (its phrases').
fn phrase_leaves(n: &Node, out: &mut [bool]) {
    match n {
        Node::Leaf(_) => {}
        Node::Phrase(ts) => ts.iter().for_each(|&(l, _)| out[l] = true),
        Node::Bool { must, should, not } => {
            must.iter()
                .chain(should)
                .chain(not)
                .for_each(|c| phrase_leaves(c, out));
        }
    }
}

/// The documents of a segment `n` holds for.
fn eval_docs(n: &Node, leaves: &[Postings], all: u32) -> Vec<u32> {
    match n {
        Node::Leaf(l) => leaves[*l].docs.clone(),
        Node::Phrase(ts) => {
            let mut cand = leaves[ts[0].0].docs.clone();
            for &(l, _) in &ts[1..] {
                cand = intersect(&cand, &leaves[l].docs);
            }
            cand.retain(|&d| {
                let at = |l: usize| {
                    let p = &leaves[l];
                    p.positions(p.docs.binary_search(&d).expect("intersected"))
                };
                let (first, base) = ts[0];
                at(first).iter().any(|&p| {
                    ts[1..].iter().all(|&(l, off)| {
                        let want = u64::from(p) + u64::from(off) - u64::from(base);
                        u32::try_from(want).is_ok_and(|w| at(l).binary_search(&w).is_ok())
                    })
                })
            });
            cand
        }
        Node::Bool { must, should, not } => {
            let mut out = if !must.is_empty() {
                let mut it = must.iter().map(|c| eval_docs(c, leaves, all));
                let first = it.next().expect("one");
                it.fold(first, |a, b| intersect(&a, &b))
            } else if !should.is_empty() {
                should
                    .iter()
                    .map(|c| eval_docs(c, leaves, all))
                    .fold(Vec::new(), |a, b| union(&a, &b))
            } else {
                (0..all).collect()
            };
            for c in not {
                out = minus(&out, &eval_docs(c, leaves, all));
            }
            out
        }
    }
}

impl View {
    /// The primary keys of the rows `q` matches, sorted.
    pub(crate) fn matching(&self, q: &Query) -> Result<Vec<Vec<u8>>> {
        let Some(root) = &q.root else {
            return Ok(Vec::new());
        };
        let mut need = vec![false; q.leaves.len()];
        phrase_leaves(root, &mut need);
        // Each row's versions found, by primary key and hash.
        let mut found: HashMap<(&[u8], u64), u32, MixState> = HashMap::default();
        for s in &self.segs {
            let leaves = q
                .leaves
                .iter()
                .zip(&need)
                .map(|(l, &p)| leaf_postings(&s.seg, l, p))
                .collect::<Result<Vec<_>>>()?;
            for d in eval_docs(root, &leaves, s.seg.docs()) {
                if !s.dead(d) {
                    *found
                        .entry((s.seg.pk(d), s.seg.hashes[d as usize]))
                        .or_default() += 1;
                }
            }
        }
        let mut hits = vec![Vec::new(); q.leaves.len()];
        for i in &self.delta.inserts {
            for h in &mut hits {
                h.clear();
            }
            for (t, ps) in &i.doc.terms {
                for (leaf, h) in q.leaves.iter().zip(hits.iter_mut()) {
                    if leaf.matches(t) {
                        h.extend_from_slice(ps);
                    }
                }
            }
            for h in &mut hits {
                h.sort_unstable();
            }
            if eval(root, &hits) {
                *found.entry((&i.pk, i.hash)).or_default() += 1;
            }
        }
        for g in &self.delta.gone {
            if let Some(n) = found.get_mut(&(g.pk.as_slice(), g.hash)) {
                *n = n.saturating_sub(1);
            }
        }
        let mut out: Vec<Vec<u8>> = found
            .into_iter()
            .filter(|(_, n)| *n > 0)
            .map(|((pk, _), _)| pk.to_vec())
            .collect();
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// The statistics of `terms`, for the snapshot's rows.
    pub(crate) fn stats(&self, terms: &[&str]) -> Stats {
        let mut st = Stats {
            df: vec![0; terms.len()],
            ..Stats::default()
        };
        let (mut docs, mut len, mut df) = (0i64, 0i64, vec![0i64; terms.len()]);
        for s in &self.segs {
            docs += i64::from(s.seg.docs());
            len += s.seg.total as i64;
            if let Some(d) = &s.deleted {
                docs -= i64::from(d.count);
                len -= d.len as i64;
            }
            for (t, n) in terms.iter().zip(df.iter_mut()) {
                if let Some(i) = s.seg.find(t.as_bytes()) {
                    *n += i64::from(s.seg.df(i));
                }
                if let Some(d) = &s.deleted {
                    *n -= i64::from(d.term(t.as_bytes()));
                }
            }
        }
        for i in &self.delta.inserts {
            docs += 1;
            len += i64::from(i.doc.len);
            for (t, n) in terms.iter().zip(df.iter_mut()) {
                if i.doc
                    .terms
                    .binary_search_by(|x| x.0.as_str().cmp(t))
                    .is_ok()
                {
                    *n += 1;
                }
            }
        }
        for g in &self.delta.gone {
            docs -= 1;
            len -= i64::from(g.len);
            for (t, n) in terms.iter().zip(df.iter_mut()) {
                if g.terms.iter().any(|x| x.as_slice() == t.as_bytes()) {
                    *n -= 1;
                }
            }
        }
        st.docs = docs.max(0) as u64;
        st.len = len.max(0) as u64;
        for (o, n) in st.df.iter_mut().zip(df) {
            *o = n.max(0) as u64;
        }
        st
    }

    /// Every row's primary key and hash, as many times as it is there
    /// (the integrity check).
    pub(crate) fn live(&self) -> Vec<(Vec<u8>, u64)> {
        let mut count: HashMap<(&[u8], u64), i64> = HashMap::new();
        for s in &self.segs {
            for d in 0..s.seg.docs() {
                if !s.dead(d) {
                    *count
                        .entry((s.seg.pk(d), s.seg.hashes[d as usize]))
                        .or_default() += 1;
                }
            }
        }
        for i in &self.delta.inserts {
            *count.entry((&i.pk, i.hash)).or_default() += 1;
        }
        for g in &self.delta.gone {
            *count.entry((&g.pk, g.hash)).or_default() -= 1;
        }
        // A row deleted more often than it is there shows as its hash
        // inverted, so it differs from every row.
        let mut out = Vec::new();
        for ((pk, hash), n) in count {
            for _ in 0..n {
                out.push((pk.to_vec(), hash));
            }
            if n < 0 {
                out.push((pk.to_vec(), !hash));
            }
        }
        out.sort();
        out
    }

    /// The rows waiting in the delta.
    pub(crate) fn waiting(&self) -> usize {
        self.delta.keys.len()
    }
}
