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

use super::segment::{self, BLOCK, Doc, Postings, Reader, Segment, bit, corrupt, varint};
use crate::error::Result;
use crate::object_cache::{self as cache, Kind};
use crate::text::query::{Leaf, Node, Query, eval, within};
use crate::text::score::{Matched, Scorer, Text, fixed, of_fixed};

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

    /// Segments made apart (a build's, [`segment::of_docs`] on its
    /// workers), each with its documents, added in order.
    pub(crate) fn append_segments(
        &self,
        txn: &mut dyn KvTxn,
        segments: Vec<(Vec<u8>, u32)>,
    ) -> Result<()> {
        let mut root = self.root(&*txn)?;
        for (b, docs) in segments {
            let id = new_id();
            self.keys.put_obj(txn, id, &b)?;
            root.segments.push(SegRef {
                id,
                docs,
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
        let Some(group) = due(&root).into_iter().next() else {
            return Ok(false);
        };
        self.merge_groups(txn, &root, &[group], 1)?;
        Ok(true)
    }

    /// Every merge due, those of one round on `workers` threads at once,
    /// until none is (a build's).
    pub(crate) fn merge_due(&self, txn: &mut dyn KvTxn, workers: usize) -> Result<()> {
        loop {
            let root = self.root(&*txn)?;
            let groups = due(&root);
            if groups.is_empty() {
                return Ok(());
            }
            self.merge_groups(txn, &root, &groups, workers)?;
        }
    }

    /// Each of `groups` of `root`'s segments (by position) written as
    /// one, in its first's place — the groups merged on up to `workers`
    /// threads at once.
    fn merge_groups(
        &self,
        txn: &mut dyn KvTxn,
        root: &Root,
        groups: &[Vec<usize>],
        workers: usize,
    ) -> Result<()> {
        let mut loaded = Vec::with_capacity(groups.len());
        for group in groups {
            let mut segs = Vec::with_capacity(group.len());
            for &i in group {
                let r = root.segments[i];
                let deleted = match r.deleted {
                    0 => None,
                    id => Some(self.deleted(&*txn, id)?),
                };
                segs.push((self.segment(&*txn, r.id)?, deleted));
            }
            loaded.push(segs);
        }
        // The workers shared out among the groups: a group's terms are
        // merged in runs on its share.
        let each = (workers / loaded.len()).max(1);
        let merge = |segs: &Vec<(Arc<Segment>, Option<Arc<Deleted>>)>| {
            let parts: Vec<(&Segment, Option<&[u64]>)> = segs
                .iter()
                .map(|(s, d)| (s.as_ref(), d.as_ref().map(|d| d.bits.as_slice())))
                .collect();
            segment::merge(&parts, each)
        };
        let merged: Vec<Result<Vec<u8>>> = if workers <= 1 || loaded.len() == 1 {
            loaded.iter().map(merge).collect()
        } else {
            let next = std::sync::atomic::AtomicUsize::new(0);
            let out: Vec<std::sync::Mutex<Option<Result<Vec<u8>>>>> =
                loaded.iter().map(|_| std::sync::Mutex::new(None)).collect();
            std::thread::scope(|sc| {
                for _ in 0..workers.min(loaded.len()) {
                    sc.spawn(|| {
                        loop {
                            let g = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(segs) = loaded.get(g) else { break };
                            *out[g].lock().unwrap_or_else(|e| e.into_inner()) = Some(merge(segs));
                        }
                    });
                }
            });
            out.into_iter()
                .map(|m| {
                    m.into_inner()
                        .unwrap_or_else(|e| e.into_inner())
                        .expect("merged")
                })
                .collect()
        };
        let mut first: HashMap<usize, SegRef> = HashMap::new();
        let mut gone = vec![false; root.segments.len()];
        for (group, b) in groups.iter().zip(merged) {
            let b = b?;
            let docs = group
                .iter()
                .map(|&i| root.segments[i].docs - root.segments[i].dead)
                .sum();
            let id = new_id();
            self.keys.put_obj(txn, id, &b)?;
            for &i in group {
                let r = root.segments[i];
                self.keys.delete_obj(txn, r.id)?;
                if r.deleted != 0 {
                    self.keys.delete_obj(txn, r.deleted)?;
                }
                gone[i] = true;
            }
            first.insert(
                group[0],
                SegRef {
                    id,
                    docs,
                    deleted: 0,
                    dead: 0,
                },
            );
        }
        let mut out = Root::default();
        for (i, s) in root.segments.iter().enumerate() {
            if let Some(m) = first.get(&i) {
                out.segments.push(*m);
            } else if !gone[i] {
                out.segments.push(*s);
            }
        }
        Ok(txn.put(&self.keys.root(), &out.encode())?)
    }

    /// Every segment merged into one (tests; `VACUUM`).
    pub(crate) fn merge_all(&self, txn: &mut dyn KvTxn, workers: usize) -> Result<()> {
        let root = self.root(&*txn)?;
        if root.segments.len() > 1 || root.segments.iter().any(|s| s.dead > 0) {
            let all: Vec<usize> = (0..root.segments.len()).collect();
            self.merge_groups(txn, &root, &[all], workers)?;
        }
        Ok(())
    }
}

/// The merges due in `root`: of each tier (segments' live documents in
/// powers of [`MERGE`]) holding [`MERGE`] segments or more, every
/// [`MERGE`] in order — the first tier's first.
fn due(root: &Root) -> Vec<Vec<usize>> {
    let tier = |s: &SegRef| {
        let live = u64::from(s.docs - s.dead).max(1);
        live.ilog(MERGE as u64)
    };
    let mut by_tier: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, s) in root.segments.iter().enumerate() {
        by_tier.entry(tier(s)).or_default().push(i);
    }
    let mut tiers: Vec<Vec<usize>> = by_tier.into_values().filter(|g| g.len() >= MERGE).collect();
    tiers.sort_by_key(|g| g[0]);
    tiers
        .iter()
        .flat_map(|g| g.as_chunks::<MERGE>().0.iter().map(|c| c.to_vec()))
        .collect()
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

/// The union of many sorted lists of a segment's `all` documents: through
/// a bitmap of them where the lists hold more than a sixteenth as many,
/// else sorted together — not pair by pair, a list copied each time.
fn union_all(mut parts: Vec<Vec<u32>>, all: u32) -> Vec<u32> {
    match parts.len() {
        0 => Vec::new(),
        1 => parts.pop().expect("one"),
        2 => union(&parts[0], &parts[1]),
        _ => {
            let total: usize = parts.iter().map(Vec::len).sum();
            if total as u64 * 16 > u64::from(all) {
                let mut bits = vec![0u64; (all as usize).div_ceil(64)];
                for p in &parts {
                    for &d in p {
                        bits[d as usize / 64] |= 1 << (d % 64);
                    }
                }
                let mut out = Vec::with_capacity(total);
                for (w, &b) in bits.iter().enumerate() {
                    let mut b = b;
                    while b != 0 {
                        out.push(w as u32 * 64 + b.trailing_zeros());
                        b &= b - 1;
                    }
                }
                out
            } else {
                let mut out = Vec::with_capacity(total);
                for p in &parts {
                    out.extend_from_slice(p);
                }
                out.sort_unstable();
                out.dedup();
                out
            }
        }
    }
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

/// A query's matches with their scores, read greatest first in slices
/// ([`View::ranked`]): each slice found by a selection over the rest and
/// sorted, so reading `n` of `m` matches costs about `m` per slice and
/// `n log n` — not a sort of them all.  The matches are the `n` best
/// asked for and every one scoring as the `n`-th — all of them where
/// [`Ranked::complete`].
pub(crate) struct Ranked<'v> {
    view: &'v View,
    scored: Vec<(At, f64)>,
    done: usize,
    complete: bool,
}

impl Ranked<'_> {
    /// Whether these are all the matches: none scoring below them left
    /// out.
    pub(crate) fn complete(&self) -> bool {
        self.complete
    }

    /// The matches not read yet.
    pub(crate) fn left(&self) -> usize {
        self.scored.len() - self.done
    }

    /// The next `n` greatest matches not read (fewer at the end), greatest
    /// first: each its primary key and score.
    pub(crate) fn next(&mut self, n: usize) -> impl Iterator<Item = (&[u8], f64)> {
        let start = self.done;
        let rest = &mut self.scored[start..];
        let n = n.min(rest.len());
        if n > 0 && n < rest.len() {
            rest.select_nth_unstable_by(n - 1, |a, b| b.1.total_cmp(&a.1));
        }
        rest[..n].sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
        self.done += n;
        let view = self.view;
        self.scored[start..start + n]
            .iter()
            .map(move |(at, s)| (view.pk(*at), *s))
    }
}

/// Where a version is: a segment's document, or an insert of the delta.
#[derive(Clone, Copy, Debug)]
enum At {
    Seg(u32, u32),
    Delta(u32),
}

/// A segment's document as a score reads it: where each leaf's postings
/// hold it, found by cursors moving forward as documents ascend.
struct InSegment<'a> {
    leaves: &'a [Postings],
    at: Vec<usize>,
    here: Vec<Option<usize>>,
    len: f64,
}

impl InSegment<'_> {
    /// To document `d` (past the last one), of length `len`.
    fn go(&mut self, d: u32, len: u32) {
        self.len = f64::from(len);
        for (l, p) in self.leaves.iter().enumerate() {
            let at = &mut self.at[l];
            *at += p.docs[*at..].partition_point(|&x| x < d);
            self.here[l] = (p.docs.get(*at) == Some(&d)).then_some(*at);
        }
    }
}

impl Text for InSegment<'_> {
    fn len(&self) -> f64 {
        self.len
    }
    fn freq(&self, l: usize) -> u32 {
        self.here[l].map_or(0, |i| self.leaves[l].freqs[i])
    }
    fn positions(&self, l: usize) -> &[u32] {
        self.here[l].map_or(&[], |i| self.leaves[l].positions(i))
    }
}

/// The `n` best scores (in units: [`crate::text::score::fixed`]) of
/// definitely live versions seen so far: the threshold below which a
/// version cannot be among the `n` best.
struct Top {
    n: usize,
    heap: std::collections::BinaryHeap<std::cmp::Reverse<i64>>,
}

impl Top {
    fn new(n: usize) -> Top {
        Top {
            n: n.max(1),
            heap: std::collections::BinaryHeap::new(),
        }
    }

    /// No threshold: every version.
    fn all() -> Top {
        Top::new(usize::MAX)
    }

    fn add(&mut self, s: i64) {
        if self.n == usize::MAX {
        } else if self.heap.len() < self.n {
            self.heap.push(std::cmp::Reverse(s));
        } else if self.heap.peek().is_some_and(|m| s > m.0) {
            self.heap.pop();
            self.heap.push(std::cmp::Reverse(s));
        }
    }

    /// The `n`-th best score (`i64::MIN` before `n` were seen): what a
    /// version must score at least.
    fn threshold(&self) -> i64 {
        match self.heap.peek() {
            Some(m) if self.heap.len() >= self.n => m.0,
            _ => i64::MIN,
        }
    }
}

/// Documents a window of MaxScore spans at most (Lucene's inner window).
const WINDOW: u32 = 4096;

/// A block's bound in units: BM25 of its best impact rounded as a
/// score's part is, and a unit more — what a text there scores in BM25
/// computed in floating point cannot pass it, whatever the rounding.
fn bound(x: f64) -> i64 {
    crate::text::score::fixed(x) + 1
}

/// No document: past a cursor's last.
const END: u32 = u32::MAX;

/// A leaf's postings in a segment as MaxScore reads them (Turtle and
/// Flood 1995; Lucene's `MaxScoreBulkScorer`): a block at a time, each
/// block's bound the best score its impacts give, a block skipped
/// without being decoded.
struct Cursor {
    /// The term, its blocks decoded from the segment; `None`: `mem`'s.
    term: Option<usize>,
    mem: Postings,
    df: usize,
    /// Per block: its last document, where it starts, its bound.
    last: Vec<u32>,
    at: Vec<usize>,
    bound: Vec<i64>,
    /// The bound of them all.
    max: i64,
    /// The block loaded (`last.len()`: past the end), its documents and
    /// frequencies, the one at.
    b: usize,
    docs: Vec<u32>,
    freqs: Vec<u32>,
    n: usize,
    i: usize,
    /// The first block a window's bound reads.
    shallow: usize,
}

impl Cursor {
    /// Leaf `l` of a bag of words in `s`, its bounds by `sc`.
    fn new(s: &Segment, leaf: &Leaf, l: usize, sc: &Scorer) -> Result<Cursor> {
        let mut c = Cursor {
            term: None,
            mem: Postings::default(),
            df: 0,
            last: Vec::new(),
            at: Vec::new(),
            bound: Vec::new(),
            max: 0,
            b: 0,
            docs: Vec::new(),
            freqs: Vec::new(),
            n: 0,
            i: 0,
            shallow: 0,
        };
        let one = match leaf {
            Leaf::Exact(t) => s.find(t.as_bytes()),
            _ => None,
        };
        let skipped = match one {
            Some(t) => s.blocks(t, |last, at, impacts| {
                c.last.push(last);
                c.at.push(at);
                c.bound.push(bound(impacts.iter().fold(0f64, |m, &(f, len)| {
                    m.max(sc.leaf_f64(l, f, sc.norm(f64::from(len))))
                })));
            })?,
            None => false,
        };
        if skipped {
            c.term = one;
            c.df = s.df(one.expect("a term")) as usize;
        } else if let Some((t, at)) = one.and_then(|t| Some((t, s.one_block(t)?))) {
            // A term in one block: decoded into the cursor once, its bound
            // the score of its greatest frequency in its shortest text —
            // BM25 grows with the one and falls with the other.
            let n = s.df(t) as usize;
            (c.docs, c.freqs) = (vec![0; n], vec![0; n]);
            s.block(at, -1, &mut c.docs, &mut c.freqs)?;
            c.term = Some(t);
            c.df = n;
            c.last.push(c.docs[n - 1]);
            c.at.push(at);
            let f = c.freqs[..n].iter().copied().max().expect("a posting");
            let len = c.docs[..n].iter().map(|&d| s.lens[d as usize]).min().expect("a posting");
            c.bound.push(sc.leaf(l, f, sc.norm(f64::from(len))));
            c.max = c.bound[0];
            c.n = n;
            return Ok(c);
        } else {
            // In memory: a leaf of many terms (each scoring 1), its blocks'
            // bounds as a one-block term's.
            c.mem = leaf_postings(s, leaf, false)?;
            c.df = c.mem.docs.len();
            for (j, (ds, fs)) in c.mem.docs.chunks(BLOCK).zip(c.mem.freqs.chunks(BLOCK)).enumerate()
            {
                c.last.push(*ds.last().expect("a block"));
                c.at.push(j * BLOCK);
                let f = fs.iter().copied().max().expect("a block");
                let len = ds.iter().map(|&d| s.lens[d as usize]).min().expect("a block");
                c.bound.push(sc.leaf(l, f, sc.norm(f64::from(len))));
            }
        }
        c.max = c.bound.iter().copied().max().unwrap_or(0);
        // A block's room, no more than its postings need.
        let room = c.df.min(BLOCK);
        (c.docs, c.freqs) = (vec![0; room], vec![0; room]);
        c.load(s, 0)?;
        Ok(c)
    }

    /// Block `j` loaded, at its first document.
    fn load(&mut self, s: &Segment, j: usize) -> Result<()> {
        self.b = j;
        self.i = 0;
        if j >= self.last.len() {
            self.n = 0;
            return Ok(());
        }
        let n = (self.df - j * BLOCK).min(BLOCK);
        self.n = n;
        match self.term {
            Some(_) => {
                let prev = if j == 0 { -1 } else { i64::from(self.last[j - 1]) };
                s.block(self.at[j], prev, &mut self.docs[..n], &mut self.freqs[..n])?;
            }
            None => {
                let at = self.at[j];
                self.docs[..n].copy_from_slice(&self.mem.docs[at..at + n]);
                self.freqs[..n].copy_from_slice(&self.mem.freqs[at..at + n]);
            }
        }
        Ok(())
    }

    fn doc(&self) -> u32 {
        if self.i < self.n { self.docs[self.i] } else { END }
    }

    fn freq(&self) -> u32 {
        self.freqs[self.i]
    }

    /// To the next document.
    fn next(&mut self, s: &Segment) -> Result<u32> {
        self.i += 1;
        if self.i == self.n {
            self.load(s, self.b + 1)?;
        }
        Ok(self.doc())
    }

    /// To the first document at or past `d`: blocks before it skipped
    /// by their last documents, undecoded.
    fn advance(&mut self, s: &Segment, d: u32) -> Result<u32> {
        if self.doc() >= d {
            return Ok(self.doc());
        }
        if self.docs[self.n - 1] < d {
            let j = self.b + self.last[self.b..].partition_point(|&x| x < d);
            self.load(s, j)?;
            if self.n == 0 {
                return Ok(END);
            }
        }
        self.i += self.docs[self.i..self.n].partition_point(|&x| x < d);
        Ok(self.doc())
    }

    /// The bound of its documents from `lo` to `hi` (excluded), windows
    /// read in order, and the most documents it may hold there (its
    /// blocks').
    fn bound_in(&mut self, lo: u32, hi: u32) -> (i64, usize) {
        let nb = self.last.len();
        while self.shallow < nb && self.last[self.shallow] < lo {
            self.shallow += 1;
        }
        let mut m = 0;
        let mut j = self.shallow;
        while j < nb && (j == 0 || self.last[j - 1] + 1 < hi) {
            m = m.max(self.bound[j]);
            j += 1;
        }
        (m, (j - self.shallow) * BLOCK)
    }
}

/// A document of a window that may reach the threshold: its offset
/// there, its length's normalization, and its score so far.
struct Candidate {
    o: u16,
    norm: f64,
    part: i64,
}

/// How many documents a leaf's postings may hold, per candidate, for
/// them to be read through rather than each candidate looked up in them
/// (a lookup a search in a block, a posting read a step).
const READ_THROUGH: usize = 4;

/// A bag of words' matches in a segment, scored (windowed MaxScore —
/// Turtle and Flood's; Lucene's `MaxScoreBulkScorer`, its windows' terms
/// summed a term at a time): documents in windows; in each, the leaves
/// sorted by their bounds there (their blocks' impacts), those whose
/// bounds together stay below the threshold *non-essential* — a document
/// only they hold cannot reach it.  The essential leaves' postings are
/// summed into the window's buffer; the documents found that may still
/// reach the threshold are the candidates, and each non-essential leaf,
/// best bound first, is matched against them — its postings read through
/// where they are few beside the candidates, else each candidate looked
/// up — and those that can no longer reach it dropped.  A candidate left
/// is given to `found`, which raises the threshold (`top`): its score is
/// `Scorer::score`'s, its parts summed in units in whatever order.  With
/// no threshold yet every leaf is essential and every match scored: a
/// full scoring.
fn max_score(
    s: &Seg,
    q: &Query,
    flat: &[usize],
    sc: &Scorer,
    top: &mut Top,
    mut found: impl FnMut(u32, i64, &mut Top),
) -> Result<()> {
    let seg = &s.seg;
    let m = flat.len();
    let mut cs = flat
        .iter()
        .map(|&l| Cursor::new(seg, &q.leaves[l], l, sc))
        .collect::<Result<Vec<_>>>()?;
    let docs = seg.docs();
    let w = WINDOW as usize;
    let mut acc = vec![0i64; w];
    let mut hit = vec![0u64; w / 64];
    let mut wb = vec![0i64; m];
    let mut wn = vec![0usize; m];
    let mut order: Vec<usize> = (0..m).collect();
    let mut cand: Vec<Candidate> = Vec::with_capacity(w);
    // Leaves by their bound over the segment, ascending: those the
    // threshold leaves essential hold every document worth a look.
    let mut by_max: Vec<usize> = (0..m).collect();
    by_max.sort_by_key(|&j| cs[j].max);
    let mut lo = 0u32;
    while lo < docs {
        let theta = top.threshold();
        // The first document a leaf essential over the segment holds.
        let mut sum = 0i64;
        let mut first = END;
        for &j in &by_max {
            sum += cs[j].max;
            if sum >= theta {
                first = first.min(cs[j].advance(seg, lo)?);
            }
        }
        if first == END {
            break;
        }
        lo = lo.max(first);
        let hi = lo.saturating_add(WINDOW).min(docs);
        for j in 0..m {
            (wb[j], wn[j]) = cs[j].bound_in(lo, hi);
        }
        order.sort_by_key(|&j| wb[j]);
        let mut ne = 0;
        let mut ne_sum = 0i64;
        while ne < m && ne_sum + wb[order[ne]] < theta {
            ne_sum += wb[order[ne]];
            ne += 1;
        }
        if ne == m {
            lo = hi;
            continue;
        }
        // The essential leaves' postings in the window, summed.
        for &j in &order[ne..] {
            let c = &mut cs[j];
            let mut d = c.advance(seg, lo)?;
            while d < hi {
                let o = (d - lo) as usize;
                acc[o] += sc.leaf(flat[j], c.freq(), sc.norm(f64::from(seg.lens[d as usize])));
                hit[o / 64] |= 1 << (o % 64);
                d = c.next(seg)?;
            }
        }
        let span = (hi - lo) as usize;
        if ne == 0 {
            // Every leaf essential: the sums are the scores.
            for (wi, word) in hit[..span.div_ceil(64)].iter_mut().enumerate() {
                let mut bits = std::mem::take(word);
                while bits != 0 {
                    let o = wi * 64 + bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    let d = lo + o as u32;
                    let part = std::mem::take(&mut acc[o]);
                    if !s.dead(d) && part >= top.threshold() {
                        found(d, part, top);
                    }
                }
            }
            lo = hi;
            continue;
        }
        // The candidates: documents found, live, that may reach it.
        cand.clear();
        for (wi, word) in hit[..span.div_ceil(64)].iter_mut().enumerate() {
            let mut bits = std::mem::take(word);
            while bits != 0 {
                let o = wi * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let d = lo + o as u32;
                let part = std::mem::take(&mut acc[o]);
                if s.dead(d) || part + ne_sum < theta {
                    continue;
                }
                cand.push(Candidate {
                    o: o as u16,
                    norm: sc.norm(f64::from(seg.lens[d as usize])),
                    part,
                });
            }
        }
        // The non-essential leaves, best bound first, matched against
        // the candidates while some may still reach the threshold.
        let mut rest = ne_sum;
        for &j in order[..ne].iter().rev() {
            if cand.is_empty() {
                break;
            }
            rest -= wb[j];
            let c = &mut cs[j];
            let l = flat[j];
            if wn[j] <= READ_THROUGH * cand.len() {
                let mut d = c.advance(seg, lo + u32::from(cand[0].o))?;
                let mut k = 0;
                while d < hi && k < cand.len() {
                    let want = lo + u32::from(cand[k].o);
                    if d < want {
                        d = c.next(seg)?;
                    } else {
                        if d == want {
                            cand[k].part += sc.leaf(l, c.freq(), cand[k].norm);
                        }
                        k += 1;
                    }
                }
            } else {
                for k in cand.iter_mut() {
                    let d = lo + u32::from(k.o);
                    if c.advance(seg, d)? == d {
                        k.part += sc.leaf(l, c.freq(), k.norm);
                    }
                }
            }
            cand.retain(|k| k.part + rest >= theta);
        }
        for k in &cand {
            found(lo + u32::from(k.o), k.part, top);
        }
        lo = hi;
    }
    Ok(())
}

/// A bag of words' matches in a segment, every one scored: each leaf's
/// postings added to their documents' sums, leaf after leaf (term at a
/// time, as T5 scored; no bounds, no windows — MaxScore's bookkeeping
/// without a threshold to skip by costs a long query on a small corpus a
/// seventh more, measured on ArguAna).
fn every_match(
    s: &Seg,
    q: &Query,
    flat: &[usize],
    sc: &Scorer,
    top: &mut Top,
    mut found: impl FnMut(u32, i64, &mut Top),
) -> Result<()> {
    let seg = &s.seg;
    let docs = seg.docs() as usize;
    let mut acc = vec![0i64; docs];
    let mut hit = vec![0u64; docs.div_ceil(64)];
    for &l in flat {
        let p = leaf_postings(seg, &q.leaves[l], false)?;
        for (&d, &f) in p.docs.iter().zip(&p.freqs) {
            acc[d as usize] += sc.leaf(l, f, sc.norm(f64::from(seg.lens[d as usize])));
            hit[d as usize / 64] |= 1 << (d % 64);
        }
    }
    for (w, &b) in hit.iter().enumerate() {
        let mut b = b;
        while b != 0 {
            let d = w as u32 * 64 + b.trailing_zeros();
            b &= b - 1;
            if !s.dead(d) {
                found(d, acc[d as usize], top);
            }
        }
    }
    Ok(())
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
                union_all(
                    should.iter().map(|c| eval_docs(c, leaves, all)).collect(),
                    all,
                )
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

/// `f`, its keys borrowed for as long as the view is.
fn keyed<'v, F: FnMut(At, (&'v [u8], u64), i64, &mut Top)>(f: F) -> F {
    f
}

impl View {
    /// The versions `q` matches that are live in the snapshot, each once,
    /// with its score by `sc` (0 without) — with `top`'s `n`, those of the
    /// `n` best and every one scoring as the `n`-th, others maybe too
    /// (whether all are: the second).  A segment's document not marked
    /// deleted is live unless a tombstone in the delta names it — a row
    /// has one version at a time, so no other copy of it is live: only
    /// those named are counted (inserts against tombstones), by primary
    /// key and hash.  The copies of one are of one text and score alike,
    /// so none is left out by a threshold that another reaches; only
    /// versions not named raise it.
    fn live_matches(
        &self,
        q: &Query,
        sc: Option<&Scorer>,
        top: &mut Top,
    ) -> Result<(Vec<(At, f64)>, bool)> {
        let Some(root) = &q.root else {
            return Ok((Vec::new(), true));
        };
        let mut need = vec![false; q.leaves.len()];
        phrase_leaves(root, &mut need);
        let named: std::collections::HashSet<(&[u8], u64), MixState> = self
            .delta
            .gone
            .iter()
            .map(|g| (g.pk.as_slice(), g.hash))
            .collect();
        let mut out: Vec<(At, i64)> = Vec::new();
        let mut counted: HashMap<(&[u8], u64), (i64, Option<(At, i64)>), MixState> =
            HashMap::default();
        // Past this many, the versions below the threshold are let go.
        let tidy = top.n.saturating_mul(2).saturating_add(4096);
        let mut found = keyed(|at: At, key: (&[u8], u64), score: i64, top: &mut Top| {
            if named.is_empty() || !named.contains(&key) {
                if score >= top.threshold() {
                    out.push((at, score));
                    top.add(score);
                    if out.len() > tidy {
                        let theta = top.threshold();
                        out.retain(|x| x.1 >= theta);
                    }
                }
            } else {
                let e = counted.entry(key).or_insert((0, None));
                e.0 += 1;
                e.1 = Some((at, score));
            }
        });
        // The delta first: its few rows raise the threshold early.
        let mut hits = vec![Vec::new(); q.leaves.len()];
        for (n, i) in self.delta.inserts.iter().enumerate() {
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
            if !eval(root, &hits) {
                continue;
            }
            let m = Matched {
                len: f64::from(i.doc.len),
                hits: &hits,
            };
            let score = sc.map_or(0, |sc| fixed(sc.score(&m)));
            found(At::Delta(n as u32), (i.pk.as_slice(), i.hash), score, top);
        }
        let flat = sc.and_then(Scorer::flat);
        for (n, s) in self.segs.iter().enumerate() {
            let key = |d: u32| (s.seg.pk(d), s.seg.hashes[d as usize]);
            if let (Some(sc), Some(flat)) = (sc, &flat) {
                let found = |d, score, top: &mut Top| found(At::Seg(n as u32, d), key(d), score, top);
                if top.n == usize::MAX {
                    every_match(s, q, flat, sc, top, found)?;
                } else {
                    max_score(s, q, flat, sc, top, found)?;
                }
                continue;
            }
            let leaves = q
                .leaves
                .iter()
                .zip(&need)
                .map(|(l, &p)| leaf_postings(&s.seg, l, p))
                .collect::<Result<Vec<_>>>()?;
            let mut doc = InSegment {
                leaves: &leaves,
                at: vec![0; leaves.len()],
                here: vec![None; leaves.len()],
                len: 0.0,
            };
            for d in eval_docs(root, &leaves, s.seg.docs()) {
                if s.dead(d) {
                    continue;
                }
                let score = match sc {
                    Some(sc) => {
                        doc.go(d, s.seg.lens[d as usize]);
                        fixed(sc.score(&doc))
                    }
                    None => 0,
                };
                found(At::Seg(n as u32, d), key(d), score, top);
            }
        }
        for g in &self.delta.gone {
            if let Some(e) = counted.get_mut(&(g.pk.as_slice(), g.hash)) {
                e.0 -= 1;
            }
        }
        let theta = top.threshold();
        out.extend(
            counted
                .into_values()
                .filter(|(n, _)| *n > 0)
                .filter_map(|(_, v)| v),
        );
        let complete = theta == i64::MIN;
        if !complete {
            out.retain(|x| x.1 >= theta);
        }
        Ok((out.into_iter().map(|(at, x)| (at, of_fixed(x))).collect(), complete))
    }

    /// The primary key of the version at `at`.
    fn pk(&self, at: At) -> &[u8] {
        match at {
            At::Seg(s, d) => self.segs[s as usize].seg.pk(d),
            At::Delta(i) => &self.delta.inserts[i as usize].pk,
        }
    }

    /// The primary keys of the rows `q` matches, sorted.
    pub(crate) fn matching(&self, q: &Query) -> Result<Vec<Vec<u8>>> {
        let (found, _) = self.live_matches(q, None, &mut Top::all())?;
        let mut out: Vec<Vec<u8>> = found.iter().map(|(at, _)| self.pk(*at).to_vec()).collect();
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// The rows `q` matches, scored by `sc`, to be read greatest first:
    /// the `n` best and every one tied with the `n`-th, at least —
    /// scored from the postings, as [`Scorer::score`] scores a row's text:
    /// the same numbers.
    pub(crate) fn ranked(&self, q: &Query, sc: &Scorer, n: usize) -> Result<Ranked<'_>> {
        let (scored, complete) = self.live_matches(q, Some(sc), &mut Top::new(n))?;
        Ok(Ranked {
            view: self,
            scored,
            done: 0,
            complete,
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::score::{Params, Stats as ScoreStats};

    /// MaxScore's matches against every document scored on its text,
    /// over segments of many windows (whole windows skipped, documents
    /// skipped to, blocks bounded): the `n` best, every one tied with the
    /// `n`-th, the same scores — none scoring below the `n`-th unless all
    /// are there.
    #[test]
    fn max_score_keeps_the_best_and_their_ties() {
        let mut seed = 99u64;
        let mut below = |n: u64| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) % n
        };
        let weights: Vec<f64> = (0..40).map(|i| 1.0 / f64::from(i + 1)).collect();
        let total: f64 = weights.iter().sum();
        crate::text::register_builtins();
        let a = crate::text::named(crate::text::DEFAULT).unwrap();
        let mut texts = Vec::new();
        for _ in 0..30_000 {
            let long = below(4) == 0;
            let n = 1 + below(if long { 60 } else { 8 });
            let words: Vec<String> = (0..n)
                .map(|_| {
                    let mut x = below(1 << 30) as f64 / f64::from(1 << 30) * total;
                    let mut w = 0;
                    while w < 39 && x >= weights[w] {
                        x -= weights[w];
                        w += 1;
                    }
                    format!("w{w}")
                })
                .collect();
            texts.push(words.join(" "));
        }
        let docs: Vec<(Vec<u8>, u64, Doc)> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| ((i as u32).to_be_bytes().to_vec(), 0, Doc::analyze(a.as_ref(), t)))
            .collect();
        let dead = |i: usize| i < 20_000 && i % 9 == 4;
        let mut deleted = Deleted::default();
        for (i, d) in docs.iter().enumerate().take(20_000) {
            if dead(i) {
                let terms: Vec<Vec<u8>> = d.2.terms.iter().map(|t| t.0.as_bytes().to_vec()).collect();
                deleted.add(i as u32, d.2.len, &terms);
            }
        }
        let seg = |ds: &[(Vec<u8>, u64, Doc)]| {
            Arc::new(Segment::decode(&segment::of_docs(ds).unwrap()).unwrap())
        };
        let r = SegRef {
            id: 0,
            docs: 0,
            deleted: 0,
            dead: 0,
        };
        let view = View {
            root: Root::default(),
            segs: vec![
                Seg {
                    r,
                    seg: seg(&docs[..20_000]),
                    deleted: Some(Arc::new(deleted)),
                },
                Seg {
                    r,
                    seg: seg(&docs[20_000..]),
                    deleted: None,
                },
            ],
            delta: Delta::default(),
        };
        for text in [
            "w0",
            "w39",
            "w0 w39",
            "w38 w39 w37",
            "w0 w1 w2 w3",
            "w2 w2 w35",
            "w3* w0 w30",
            "w0 w1 w2 w3 w4 w5 w6 w7 w8 w9 w10 w11",
        ] {
            let q = crate::text::query::cached(text, &a).unwrap();
            let words: Vec<&str> = q
                .leaves
                .iter()
                .map(|l| match l {
                    Leaf::Exact(t) => t.as_str(),
                    _ => "",
                })
                .collect();
            let st = view.stats(&words);
            let stats = ScoreStats {
                docs: st.docs as f64,
                len: st.len as f64,
                df: st
                    .df
                    .iter()
                    .zip(&words)
                    .map(|(&n, w)| if w.is_empty() { 0.0 } else { n as f64 })
                    .collect(),
            };
            let sc = Scorer::new(&q, &stats, Params { k1: 1.2, b: 0.75 });
            // Every live document scored on its terms.
            let mut all: Vec<(Vec<u8>, f64)> = Vec::new();
            let mut hits = vec![Vec::new(); q.leaves.len()];
            for (i, (pk, _, doc)) in docs.iter().enumerate() {
                if dead(i) {
                    continue;
                }
                for h in &mut hits {
                    h.clear();
                }
                for (t, ps) in &doc.terms {
                    for (leaf, h) in q.leaves.iter().zip(hits.iter_mut()) {
                        if leaf.matches(t) {
                            h.extend_from_slice(ps);
                        }
                    }
                }
                for h in &mut hits {
                    h.sort_unstable();
                }
                if eval(q.root.as_ref().unwrap(), &hits) {
                    let m = Matched {
                        len: f64::from(doc.len),
                        hits: &hits,
                    };
                    all.push((pk.clone(), sc.score(&m)));
                }
            }
            all.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            for n in [1, 2, 10, 100, 1000, 100_000] {
                let mut ranked = view.ranked(&q, &sc, n).unwrap();
                let left = ranked.left();
                let mut got: Vec<(Vec<u8>, f64)> =
                    ranked.next(left).map(|(pk, s)| (pk.to_vec(), s)).collect();
                got.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                let cut = all.get(n - 1).map_or(f64::NEG_INFINITY, |x| x.1);
                let want: Vec<_> = all.iter().filter(|x| x.1 >= cut).cloned().collect();
                if ranked.complete() {
                    assert_eq!(got, all, "{text} {n}: all");
                } else {
                    assert_eq!(got, want, "{text} {n}");
                }
            }
        }
    }
}
