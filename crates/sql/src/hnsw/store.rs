//! An HNSW index's storage on the KV (PLAN.md §M14.12 V4, model (C), as
//! measured in `bench/ann/src/paged.rs`; V4b): one graph stored as
//! immutable objects, each keyed by a hash of its contents — vector pages
//! (written once: new nodes go to new pages), adjacency pages, patches of
//! the edge lists changed since the pages were written, a directory of the
//! pages in parts, the dead nodes — so decoded objects are shared by every
//! snapshot and a hop is a memory access ([`super::cache`]).  Pages hold 64
//! nodes (what a write rewrites); in memory they are decoded into chunks of
//! 4,096.
//!
//! Rows written since the graph was folded are a *delta*: a key a write,
//! under the row's primary key and a nonce — every key a write puts is
//! new, so writers never conflict with each other or with the upkeep.  A
//! deleted or updated row's old vector gets a *dead* key (primary key, a
//! hash of the vector, a nonce).  A *seal* moves the delta and dead keys
//! it sees into an immutable sealed object listed in the root, in one
//! transaction (an LSM's memtable flush); a *fold* inserts the sealed rows
//! still in the table into the graph (ParlayANN's batches) and marks the
//! nodes of rows gone dead — once they are 2 % of the graph, or a
//! compaction is due, it cuts them out (FreshDiskANN's consolidation).
//! A search reads the graph and the sealed objects from memory and the
//! rest of the delta from the KV; the caller checks what it found against
//! the table's rows in its snapshot.
//!
//! Keys under the index's prefix: `r` the root; `o` + id an object; `d` +
//! primary key + nonce a delta row (its vector); `x` + primary key + hash
//! + nonce a dead row.

use std::collections::{BTreeSet, HashMap};
use std::ops::Bound;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

use nucleus_kernels::{MixState, mix};
use nucleus_kv::{KvRead, KvTxn, prefix_end};

use super::cache::{self, Kind};
use super::graph::{self, Graph, GraphMut, Near, Node, Params, Scratch};
use crate::error::{Error, Result};

/// The share of a graph's nodes dead past which a fold cuts them out.
const CONSOLIDATE: f64 = 0.02;
/// Pages per directory part (path copying, as a persistent B-tree's).
const PART: usize = 256;
/// The alignment of a chunk's vectors (a cache line on Apple's cores).
const LINE: usize = 128;

/// An index's shape: the graph's parameters and how it is stored.
#[derive(Clone, Copy, Debug)]
pub struct Shape {
    /// The graph.
    pub params: Params,
    /// A vector's bytes.
    pub width: usize,
    /// Nodes per page (what a write rewrites).
    pub page: usize,
    /// Nodes per chunk in memory (a multiple of `page`).
    pub chunk: usize,
    /// The patches' share of the graph's edges past which a fold rewrites
    /// the pages they touch.
    pub compact: f64,
    /// How a fold batches its inserts.
    pub batch: graph::Batching,
}

impl Shape {
    /// The measured defaults (`bench/ann`) for `params`, `width` bytes a
    /// vector, `threads` for a fold.
    pub fn new(params: Params, width: usize, threads: usize) -> Self {
        Shape {
            params,
            width,
            page: 64,
            chunk: 4096,
            compact: 0.1,
            batch: graph::Batching {
                ratio: 0.02,
                cap: 1000,
                join: true,
                threads,
            },
        }
    }
}

/// The keys of one index.
#[derive(Clone)]
pub struct Keys(Vec<u8>);

impl Keys {
    /// Under the index's `prefix`.
    pub fn new(prefix: Vec<u8>) -> Self {
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

    /// A delta row's key.
    pub fn delta(&self, pk: &[u8], nonce: u64) -> Vec<u8> {
        let mut k = self.tagged(b'd');
        k.extend_from_slice(pk);
        k.extend_from_slice(&nonce.to_be_bytes());
        k
    }

    /// A dead row's key.
    pub fn dead(&self, pk: &[u8], hash: u64, nonce: u64) -> Vec<u8> {
        let mut k = self.tagged(b'x');
        k.extend_from_slice(pk);
        k.extend_from_slice(&hash.to_be_bytes());
        k.extend_from_slice(&nonce.to_be_bytes());
        k
    }

    /// The ranges a seal empties: the delta's rows and its dead keys.
    pub fn queues(&self) -> [(Vec<u8>, Vec<u8>); 2] {
        [self.range(b'd'), self.range(b'x')]
    }

    fn range(&self, tag: u8) -> (Vec<u8>, Vec<u8>) {
        let start = self.tagged(tag);
        let end = prefix_end(&start).expect("a tagged prefix");
        (start, end)
    }

    /// A delta key's primary key.
    fn delta_pk<'k>(&self, k: &'k [u8]) -> &'k [u8] {
        &k[self.0.len() + 1..k.len() - 8]
    }

    /// A dead key's primary key and hash.
    fn dead_pk<'k>(&self, k: &'k [u8]) -> (&'k [u8], u64) {
        let n = k.len();
        let hash = u64::from_be_bytes(k[n - 16..n - 8].try_into().expect("8 bytes"));
        (&k[self.0.len() + 1..n - 16], hash)
    }
}

/// A nonce for a write's key: unique in the process, and across restarts
/// (a counter from the clock at the first).
pub fn nonce() -> u64 {
    static NEXT: OnceLock<AtomicU64> = OnceLock::new();
    NEXT.get_or_init(|| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos() as u64);
        AtomicU64::new(mix(now ^ u64::from(std::process::id())))
    })
    .fetch_add(1, Relaxed)
}

/// A vector's hash (a deleted row's, matched to its node).
pub fn vector_hash(v: &[u8]) -> u64 {
    object_id(7, v)
}

/// An object's id: a hash of its kind and bytes (the column store's
/// `segment_id`).
fn object_id(kind: u64, b: &[u8]) -> u64 {
    let mut h = mix(kind ^ 0x6e75_636c_6575_7321 ^ b.len() as u64);
    for w in b.chunks(8) {
        let mut x = [0u8; 8];
        x[..w.len()].copy_from_slice(w);
        h = mix(h ^ u64::from_le_bytes(x));
    }
    h | 1
}

fn corrupt(what: &str) -> Error {
    Error::internal(format!("hnsw index: {what}"))
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if n > self.0.len() {
            return Err(corrupt("an object ends early"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("2")))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4")))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8")))
    }
    fn bytes(&mut self) -> Result<&[u8]> {
        let n = self.u16()? as usize;
        self.take(n)
    }
}

fn put_bytes(b: &mut Vec<u8>, x: &[u8]) {
    b.extend_from_slice(&(x.len() as u16).to_le_bytes());
    b.extend_from_slice(x);
}

// ------------------------------------------------------------------ root

/// The graph's root: its pages, the patches written since they were (the
/// oldest first) and their bytes, its nodes, entry point and dead nodes.
#[derive(Clone, Default, Debug)]
struct GraphRoot {
    dir: u64,
    patches: Vec<u64>,
    patch_bytes: u64,
    nodes: u32,
    entry: Option<(Node, usize)>,
    dead: u64,
}

/// The root: the graph's lineage (a nonce from the build: what in-memory
/// state derived from the graph is keyed by), the graph and the sealed
/// objects (each with its rows) not yet folded, the oldest first.
#[derive(Clone, Default, Debug)]
pub struct Root {
    lineage: u64,
    graph: GraphRoot,
    sealed: Vec<(u64, u32)>,
}

const ROOT_VERSION: u8 = 1;

impl Root {
    fn encode(&self) -> Vec<u8> {
        let g = &self.graph;
        let mut b = vec![ROOT_VERSION];
        b.extend_from_slice(&self.lineage.to_le_bytes());
        b.extend_from_slice(&g.dir.to_le_bytes());
        b.extend_from_slice(&g.patch_bytes.to_le_bytes());
        b.extend_from_slice(&(g.patches.len() as u32).to_le_bytes());
        for p in &g.patches {
            b.extend_from_slice(&p.to_le_bytes());
        }
        b.extend_from_slice(&g.nodes.to_le_bytes());
        match g.entry {
            Some((n, l)) => {
                b.push(1);
                b.extend_from_slice(&n.to_le_bytes());
                b.push(l as u8);
            }
            None => b.push(0),
        }
        b.extend_from_slice(&g.dead.to_le_bytes());
        b.extend_from_slice(&(self.sealed.len() as u32).to_le_bytes());
        for (id, rows) in &self.sealed {
            b.extend_from_slice(&id.to_le_bytes());
            b.extend_from_slice(&rows.to_le_bytes());
        }
        b
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader(b);
        if r.u8()? != ROOT_VERSION {
            return Err(corrupt("an unknown root version"));
        }
        let lineage = r.u64()?;
        let (dir, patch_bytes) = (r.u64()?, r.u64()?);
        let patches = (0..r.u32()?).map(|_| r.u64()).collect::<Result<_>>()?;
        let nodes = r.u32()?;
        let entry = if r.u8()? == 1 {
            Some((r.u32()?, r.u8()? as usize))
        } else {
            None
        };
        let dead = r.u64()?;
        let sealed = (0..r.u32()?)
            .map(|_| Ok((r.u64()?, r.u32()?)))
            .collect::<Result<_>>()?;
        Ok(Root {
            lineage,
            graph: GraphRoot {
                dir,
                patches,
                patch_bytes,
                nodes,
                entry,
                dead,
            },
            sealed,
        })
    }

    /// The graph's nodes.
    pub fn nodes(&self) -> u32 {
        self.graph.nodes
    }

    /// The rows sealed and not yet folded.
    pub fn sealed_rows(&self) -> u64 {
        self.sealed.iter().map(|s| u64::from(s.1)).sum()
    }

    /// The rows the graph and the sealed objects hold (the graph's dead
    /// nodes too; not the delta's).
    pub fn rows(&self) -> usize {
        self.graph.nodes as usize + self.sealed_rows() as usize
    }
}

// --------------------------------------------------------------- objects

/// What [`Index::candidates`] found.
pub struct Candidates {
    /// Primary keys with the graph's distance from the query, nearest
    /// first.
    pub near: Vec<(f32, Vec<u8>)>,
    /// Whether they are all the rows the index holds (fewer than asked
    /// for were there).
    pub all: bool,
    /// The rows the index holds: the graph's nodes (dead ones too), the
    /// sealed rows and the delta's.
    pub held: usize,
    /// The sealed rows scanned, and the ns it took.
    pub sealed: (usize, u64),
    /// The delta's rows scanned, and the ns it took.
    pub delta: (usize, u64),
}

/// A graph's dead nodes: sorted, and as bits.
pub(super) struct Dead {
    nodes: Vec<Node>,
    bits: Vec<u64>,
}

impl Dead {
    fn new(nodes: Vec<Node>) -> Self {
        let mut bits = vec![0u64; nodes.last().map_or(0, |&n| n as usize / 64 + 1)];
        for &n in &nodes {
            bits[n as usize / 64] |= 1 << (n % 64);
        }
        Dead { nodes, bits }
    }

    #[inline]
    fn has(&self, n: Node) -> bool {
        self.bits
            .get(n as usize / 64)
            .is_some_and(|w| w >> (n % 64) & 1 == 1)
    }

    fn encode(nodes: &[Node]) -> Vec<u8> {
        let mut b = Vec::with_capacity(4 + 4 * nodes.len());
        b.extend_from_slice(&(nodes.len() as u32).to_le_bytes());
        for n in nodes {
            b.extend_from_slice(&n.to_le_bytes());
        }
        b
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader(b);
        let nodes = (0..r.u32()?).map(|_| r.u32()).collect::<Result<_>>()?;
        Ok(Dead::new(nodes))
    }
}

/// Rows' primary keys, one buffer.
#[derive(Default)]
struct Pks {
    bytes: Vec<u8>,
    at: Vec<u32>,
}

impl Pks {
    fn push(&mut self, pk: &[u8]) {
        if self.at.is_empty() {
            self.at.push(0);
        }
        self.bytes.extend_from_slice(pk);
        self.at.push(self.bytes.len() as u32);
    }

    fn len(&self) -> usize {
        self.at.len().saturating_sub(1)
    }

    fn get(&self, i: usize) -> &[u8] {
        &self.bytes[self.at[i] as usize..self.at[i + 1] as usize]
    }

    fn size(&self) -> usize {
        self.bytes.len() + 4 * self.at.len()
    }
}

/// A chunk's nodes' rows and vectors (the vectors on a cache line).
struct VecChunk {
    pks: Pks,
    buf: Vec<u8>,
    off: usize,
    len: usize,
}

impl VecChunk {
    fn encode_page(pks: &[&[u8]], data: &[u8]) -> Vec<u8> {
        let mut b = Vec::with_capacity(4 + 10 * pks.len() + data.len());
        b.extend_from_slice(&(pks.len() as u32).to_le_bytes());
        for pk in pks {
            put_bytes(&mut b, pk);
        }
        b.extend_from_slice(data);
        b
    }

    /// Pages decoded into one chunk.
    fn join(pages: &[Vec<u8>]) -> Result<Self> {
        let mut pks = Pks::default();
        let mut data: Vec<&[u8]> = Vec::with_capacity(pages.len());
        for b in pages {
            let mut r = Reader(b);
            for _ in 0..r.u32()? {
                pks.push(r.bytes()?);
            }
            data.push(r.0);
        }
        let len = data.iter().map(|d| d.len()).sum();
        let mut buf = vec![0u8; len + LINE];
        let off = buf.as_ptr().align_offset(LINE).min(LINE);
        let mut at = off;
        for d in data {
            buf[at..at + d.len()].copy_from_slice(d);
            at += d.len();
        }
        Ok(VecChunk { pks, buf, off, len })
    }

    #[inline]
    fn data(&self) -> &[u8] {
        &self.buf[self.off..self.off + self.len]
    }
}

/// A chunk's nodes' out-edges, flat (compressed sparse rows): node `i`'s
/// lists are `layers[at[i]..at[i + 1]]`, each list `edges[layers[j] ..
/// layers[j + 1]]`.
struct AdjChunk {
    at: Vec<u32>,
    layers: Vec<u32>,
    edges: Vec<Node>,
}

impl AdjChunk {
    fn encode_page(lists: &[Vec<Vec<Node>>]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&(lists.len() as u32).to_le_bytes());
        for node in lists {
            b.push(node.len() as u8);
            for l in node {
                b.extend_from_slice(&(l.len() as u16).to_le_bytes());
                for e in l {
                    b.extend_from_slice(&e.to_le_bytes());
                }
            }
        }
        b
    }

    #[inline]
    fn list(&self, i: usize, layer: usize) -> &[Node] {
        let (Some(&a), Some(&b)) = (self.at.get(i), self.at.get(i + 1)) else {
            return &[];
        };
        let j = a as usize + layer;
        if j >= b as usize {
            return &[];
        }
        &self.edges[self.layers[j] as usize..self.layers[j + 1] as usize]
    }

    /// Pages' lists as one chunk: each page's (`None`: no lists yet),
    /// padded to `page` nodes.
    fn join(pages: &[Option<Vec<u8>>], page: usize) -> Result<Self> {
        let (mut at, mut layers, mut edges) = (Vec::new(), Vec::new(), Vec::new());
        for b in pages {
            let mut r = Reader(b.as_deref().unwrap_or(&[0, 0, 0, 0]));
            let n = r.u32()? as usize;
            for i in 0..page {
                at.push(layers.len() as u32);
                if i < n {
                    for _ in 0..r.u8()? {
                        layers.push(edges.len() as u32);
                        let len = r.u16()? as usize;
                        for _ in 0..len {
                            edges.push(r.u32()?);
                        }
                    }
                }
            }
        }
        at.push(layers.len() as u32);
        layers.push(edges.len() as u32);
        Ok(AdjChunk { at, layers, edges })
    }

    fn size(&self) -> usize {
        4 * (self.at.len() + self.layers.len() + self.edges.len())
    }
}

/// A node's list on a layer, as one key.
fn slot(n: Node, layer: usize) -> u64 {
    (u64::from(n) << 8) | layer as u64
}

type Lists = HashMap<u64, Vec<Node>, MixState>;
/// Each changed list as it now is, and its length before the fold if it
/// was only appended to (`None`: replaced).
type Changed = HashMap<u64, (Vec<Node>, Option<usize>), MixState>;

/// A fold's changes to edge lists: a list replaced or edges appended.
struct Patch {
    entries: Vec<(u64, bool, Vec<Node>)>,
}

impl Patch {
    fn encode(changed: &Changed) -> Vec<u8> {
        let mut keys: Vec<&u64> = changed.keys().collect();
        keys.sort();
        let mut b = Vec::new();
        b.extend_from_slice(&(keys.len() as u32).to_le_bytes());
        for k in keys {
            let (l, was) = &changed[k];
            let part = &l[was.unwrap_or(0)..];
            b.extend_from_slice(&k.to_le_bytes());
            b.push(u8::from(was.is_some()));
            b.extend_from_slice(&(part.len() as u16).to_le_bytes());
            for e in part {
                b.extend_from_slice(&e.to_le_bytes());
            }
        }
        b
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader(b);
        let n = r.u32()? as usize;
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            let k = r.u64()?;
            let append = r.u8()? == 1;
            let len = r.u16()? as usize;
            let edges = (0..len).map(|_| r.u32()).collect::<Result<_>>()?;
            entries.push((k, append, edges));
        }
        Ok(Patch { entries })
    }
}

/// Patches resolved in memory: the lists some patches changed as they are
/// after them, over the patches before; which nodes it has lists for; how
/// many patches it holds.  A chain kept short by the logarithmic method
/// (Bentley and Saxe 1980): a level takes in the one before it while that
/// holds no more patches than it does — sizes as a binary counter's bits,
/// so a lookup walks O(log patches) levels, not one a fold.
struct Level {
    prev: Option<Arc<Level>>,
    lists: Lists,
    dirty: Vec<u64>,
    patches: usize,
}

impl Level {
    /// `self` with the levels before it merged in while they hold no more
    /// patches than it (its lists win: they are newer).
    fn merged(mut self) -> Self {
        while let Some(p) = self.prev.clone() {
            if p.patches > self.patches {
                break;
            }
            for (k, v) in &p.lists {
                self.lists.entry(*k).or_insert_with(|| v.clone());
            }
            if self.dirty.len() < p.dirty.len() {
                self.dirty.resize(p.dirty.len(), 0);
            }
            for (a, b) in self.dirty.iter_mut().zip(&p.dirty) {
                *a |= b;
            }
            self.patches += p.patches;
            self.prev = p.prev.clone();
        }
        self
    }

    fn size(&self) -> usize {
        self.lists.values().map(|l| 16 + 4 * l.len()).sum::<usize>() + 8 * self.dirty.len()
    }

    #[inline]
    fn get(&self, n: Node, layer: usize) -> Option<&[Node]> {
        let (w, b) = (n as usize / 64, n as usize % 64);
        let mut at = Some(self);
        while let Some(l) = at {
            if l.dirty.get(w).is_some_and(|x| x >> b & 1 == 1)
                && let Some(v) = l.lists.get(&slot(n, layer))
            {
                return Some(v);
            }
            at = l.prev.as_deref();
        }
        None
    }
}

/// A sealed object: rows written (primary key, vector) and rows gone
/// (primary key, the hash of the vector it had), in memory.
pub(super) struct Sealed {
    pks: Pks,
    data: Vec<u8>,
    gone: Vec<(Vec<u8>, u64)>,
}

impl Sealed {
    fn encode(rows: &[(Vec<u8>, Vec<u8>)], gone: &[(Vec<u8>, u64)]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&(rows.len() as u32).to_le_bytes());
        for (pk, v) in rows {
            put_bytes(&mut b, pk);
            b.extend_from_slice(v);
        }
        b.extend_from_slice(&(gone.len() as u32).to_le_bytes());
        for (pk, h) in gone {
            put_bytes(&mut b, pk);
            b.extend_from_slice(&h.to_le_bytes());
        }
        b
    }

    fn decode(b: &[u8], width: usize) -> Result<Self> {
        let mut r = Reader(b);
        let mut pks = Pks::default();
        let mut data = Vec::new();
        for _ in 0..r.u32()? {
            pks.push(r.bytes()?);
            data.extend_from_slice(r.take(width)?);
        }
        let mut gone = Vec::new();
        for _ in 0..r.u32()? {
            let pk = r.bytes()?.to_vec();
            gone.push((pk, r.u64()?));
        }
        Ok(Sealed { pks, data, gone })
    }

    fn size(&self) -> usize {
        self.pks.size() + self.data.len() + self.gone.iter().map(|g| g.0.len() + 32).sum::<usize>()
    }
}

/// A graph's pages: each page's vector page and adjacency page (0: none
/// yet), the parts that list them, and the decoded chunks as first read.
struct Dir {
    pages: Vec<(u64, u64)>,
    parts: Vec<u64>,
    vecs: Vec<OnceLock<Arc<VecChunk>>>,
    adjs: Vec<OnceLock<Arc<AdjChunk>>>,
}

impl Dir {
    fn new(pages: Vec<(u64, u64)>, parts: Vec<u64>, per: usize) -> Self {
        let n = pages.len().div_ceil(per);
        Dir {
            pages,
            parts,
            vecs: (0..n).map(|_| OnceLock::new()).collect(),
            adjs: (0..n).map(|_| OnceLock::new()).collect(),
        }
    }

    fn encode(pages: usize, parts: &[u64]) -> Vec<u8> {
        let mut b = Vec::with_capacity(8 + 8 * parts.len());
        b.extend_from_slice(&(pages as u32).to_le_bytes());
        b.extend_from_slice(&(parts.len() as u32).to_le_bytes());
        for p in parts {
            b.extend_from_slice(&p.to_le_bytes());
        }
        b
    }

    fn encode_part(pages: &[(u64, u64)]) -> Vec<u8> {
        let mut b = Vec::with_capacity(4 + 16 * pages.len());
        b.extend_from_slice(&(pages.len() as u32).to_le_bytes());
        for (v, a) in pages {
            b.extend_from_slice(&v.to_le_bytes());
            b.extend_from_slice(&a.to_le_bytes());
        }
        b
    }

    fn decode_part(b: &[u8]) -> Result<Vec<(u64, u64)>> {
        let mut r = Reader(b);
        let n = r.u32()? as usize;
        (0..n).map(|_| Ok((r.u64()?, r.u64()?))).collect()
    }

    /// Directory `id`, its parts read through the cache.
    fn load(o: &Objects, id: u64, per: usize) -> Result<Self> {
        let b = o.read(id)?;
        let mut r = Reader(&b);
        let n = r.u32()? as usize;
        let parts: Vec<u64> = (0..r.u32()?).map(|_| r.u64()).collect::<Result<_>>()?;
        let mut pages = Vec::with_capacity(n);
        for &p in &parts {
            let part = cache::get(Kind::Part, p, || {
                let pages = Dir::decode_part(&o.read(p)?)?;
                let bytes = 16 * pages.len();
                Ok((pages, bytes))
            })?;
            pages.extend_from_slice(&part);
        }
        Ok(Dir::new(pages, parts, per))
    }

    /// `pages` as a directory written to `w` (the parts unchanged from
    /// this one's kept); its id.
    fn write(&self, pages: &[(u64, u64)], w: &mut Writes) -> u64 {
        let mut parts = Vec::new();
        for (i, part) in pages.chunks(PART).enumerate() {
            let old = self.parts.get(i).copied();
            let same = self.pages.get(i * PART..i * PART + part.len()) == Some(part)
                && (self.pages.len().min((i + 1) * PART) == i * PART + part.len());
            let id = match old {
                Some(o) if same => o,
                _ => w.object(5, Dir::encode_part(part)),
            };
            if let Some(o) = old
                && o != id
            {
                w.dropped.insert(o);
            }
            parts.push(id);
        }
        w.dropped.extend(self.parts.iter().skip(parts.len()));
        w.object(4, Dir::encode(pages.len(), &parts))
    }
}

/// A chunk's cache key: its pages' ids, hashed.
fn chunk_key(ids: impl Iterator<Item = u64>) -> u64 {
    ids.fold(0x9e37_79b9_7f4a_7c15u64, |h, id| mix(h ^ id))
}

/// Reads an index's objects in a snapshot.
pub struct Objects<'a> {
    keys: &'a Keys,
    txn: &'a dyn KvRead,
}

impl Objects<'_> {
    fn read(&self, id: u64) -> Result<Vec<u8>> {
        self.txn
            .get(&self.keys.obj(id))?
            .ok_or_else(|| corrupt(&format!("object {id:x} is missing")))
    }
}

/// A fold's writes: objects put, objects no longer referenced deleted.
struct Writes {
    put: Vec<(u64, Vec<u8>)>,
    dropped: BTreeSet<u64>,
}

impl Writes {
    fn new() -> Self {
        Writes {
            put: Vec::new(),
            dropped: BTreeSet::new(),
        }
    }

    fn object(&mut self, kind: u64, bytes: Vec<u8>) -> u64 {
        let id = object_id(kind, &bytes);
        self.put.push((id, bytes));
        id
    }

    /// Puts the objects and deletes those dropped (not put again).
    fn apply(self, txn: &mut dyn KvTxn, keys: &Keys) -> Result<()> {
        let live: BTreeSet<u64> = self.put.iter().map(|(id, _)| *id).collect();
        for (id, b) in &self.put {
            txn.put(&keys.obj(*id), b)?;
        }
        for id in self.dropped.difference(&live) {
            txn.delete(&keys.obj(*id))?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ view

/// The graph as a snapshot sees it.  A read that fails is noted (and the
/// graph then looks empty there) — the search or fold returns it.
pub struct View<'a> {
    shape: Shape,
    objects: Objects<'a>,
    root: GraphRoot,
    lineage: u64,
    dir: Arc<Dir>,
    level: Option<Arc<Level>>,
    dead: Option<Arc<Dead>>,
    failed: Mutex<Option<Error>>,
    zeros: Vec<u8>,
}

impl<'a> View<'a> {
    fn open(shape: Shape, keys: &'a Keys, txn: &'a dyn KvRead, root: &Root) -> Result<View<'a>> {
        let objects = Objects { keys, txn };
        let g = root.graph.clone();
        let per = shape.chunk / shape.page;
        let dir = if g.dir == 0 {
            Arc::new(Dir::new(Vec::new(), Vec::new(), per))
        } else {
            cache::get(Kind::Dir, g.dir, || {
                let d = Dir::load(&objects, g.dir, per)?;
                let bytes = 16 * d.pages.len();
                Ok((d, bytes))
            })?
        };
        let dead = if g.dead == 0 {
            None
        } else {
            Some(cache::get(Kind::Dead, g.dead, || {
                let d = Dead::decode(&objects.read(g.dead)?)?;
                let bytes = 4 * d.nodes.len() + 8 * d.bits.len();
                Ok((d, bytes))
            })?)
        };
        let mut view = View {
            shape,
            objects,
            root: g,
            lineage: root.lineage,
            dir,
            level: None,
            dead,
            failed: Mutex::new(None),
            zeros: vec![0; shape.width],
        };
        view.level = view.resolve()?;
        Ok(view)
    }

    fn fail(&self, e: Error) {
        let mut f = self.failed.lock().unwrap_or_else(|e| e.into_inner());
        if f.is_none() {
            *f = Some(e);
        }
    }

    /// The first read that failed, if one did.
    fn check(&self) -> Result<()> {
        match self.failed.lock().unwrap_or_else(|e| e.into_inner()).take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// The patches resolved, each level cached by the lineage and the
    /// patches up to it.
    fn resolve(&self) -> Result<Option<Arc<Level>>> {
        let mut level: Option<Arc<Level>> = None;
        let mut h = mix(self.lineage ^ 0x9e37_79b9_7f4a_7c15);
        for &id in &self.root.patches {
            h = mix(h ^ id);
            let prev = level.clone();
            level = Some(cache::get(Kind::Level, h, || {
                let patch = Patch::decode(&self.objects.read(id)?)?;
                let mut lists =
                    Lists::with_capacity_and_hasher(patch.entries.len(), MixState::default());
                let mut dirty = vec![0u64; (self.root.nodes as usize).div_ceil(64)];
                for (k, append, edges) in &patch.entries {
                    let (n, layer) = ((k >> 8) as Node, (k & 0xff) as usize);
                    let list = if *append {
                        let mut l = match prev.as_deref().and_then(|l| l.get(n, layer)) {
                            Some(l) => l.to_vec(),
                            None => self.page_list(n, layer).to_vec(),
                        };
                        l.extend_from_slice(edges);
                        l
                    } else {
                        edges.clone()
                    };
                    lists.insert(*k, list);
                    let w = n as usize / 64;
                    if w >= dirty.len() {
                        dirty.resize(w + 1, 0);
                    }
                    dirty[w] |= 1 << (n as usize % 64);
                }
                let level = Level {
                    prev: prev.clone(),
                    lists,
                    dirty,
                    patches: 1,
                }
                .merged();
                let bytes = level.size();
                Ok((level, bytes))
            })?);
        }
        self.check()?;
        Ok(level)
    }

    fn chunk_pages(&self, c: usize) -> &[(u64, u64)] {
        let per = self.shape.chunk / self.shape.page;
        &self.dir.pages[c * per..((c + 1) * per).min(self.dir.pages.len())]
    }

    fn vec_chunk(&self, c: usize) -> Option<&VecChunk> {
        let cell = self.dir.vecs.get(c)?;
        if let Some(v) = cell.get() {
            return Some(v);
        }
        let pages = self.chunk_pages(c);
        let got = cache::get(Kind::Vectors, chunk_key(pages.iter().map(|p| p.0)), || {
            let bytes: Vec<Vec<u8>> = pages
                .iter()
                .map(|p| self.objects.read(p.0))
                .collect::<Result<_>>()?;
            let v = VecChunk::join(&bytes)?;
            let size = v.buf.len() + v.pks.size();
            Ok((v, size))
        });
        match got {
            Ok(v) => Some(cell.get_or_init(|| v)),
            Err(e) => {
                self.fail(e);
                None
            }
        }
    }

    #[inline]
    fn at(&self, n: Node) -> &[u8] {
        let (chunk, w) = (self.shape.chunk, self.shape.width);
        let i = n as usize % chunk * w;
        match self.vec_chunk(n as usize / chunk) {
            Some(c) if i + w <= c.len => &c.data()[i..i + w],
            _ => &self.zeros,
        }
    }

    /// Node `n`'s row (its primary key).
    fn pk(&self, n: Node) -> Option<&[u8]> {
        let chunk = self.shape.chunk;
        let c = self.vec_chunk(n as usize / chunk)?;
        let i = n as usize % chunk;
        (i < c.pks.len()).then(|| c.pks.get(i))
    }

    fn page_list(&self, n: Node, layer: usize) -> &[Node] {
        let chunk = self.shape.chunk;
        let c = n as usize / chunk;
        let Some(cell) = self.dir.adjs.get(c) else {
            return &[];
        };
        let adj = match cell.get() {
            Some(a) => a,
            None => {
                let pages = self.chunk_pages(c);
                let got = cache::get(Kind::Lists, chunk_key(pages.iter().map(|p| p.1)), || {
                    let bytes: Vec<Option<Vec<u8>>> = pages
                        .iter()
                        .map(|p| (p.1 != 0).then(|| self.objects.read(p.1)).transpose())
                        .collect::<Result<_>>()?;
                    let a = AdjChunk::join(&bytes, self.shape.page)?;
                    let size = a.size();
                    Ok((a, size))
                });
                match got {
                    Ok(a) => cell.get_or_init(|| a),
                    Err(e) => {
                        self.fail(e);
                        return &[];
                    }
                }
            }
        };
        adj.list(n as usize % chunk, layer)
    }

    fn is_dead(&self, n: Node) -> bool {
        self.dead.as_ref().is_some_and(|d| d.has(n))
    }

    /// The pages the patches have lists for.
    fn touched(&self) -> BTreeSet<usize> {
        let mut out = BTreeSet::new();
        let mut at = self.level.as_deref();
        while let Some(l) = at {
            out.extend(l.lists.keys().map(|k| (k >> 8) as usize / self.shape.page));
            at = l.prev.as_deref();
        }
        out
    }
}

impl Graph for View<'_> {
    #[inline]
    fn distance(&self, q: &[u8], n: Node) -> f32 {
        self.shape.params.metric.distance(q, self.at(n))
    }

    fn vector(&self, n: Node, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(self.at(n));
    }

    #[inline]
    fn neighbors(&self, n: Node, layer: usize, out: &mut Vec<Node>) {
        out.clear();
        match self.level.as_deref().and_then(|l| l.get(n, layer)) {
            Some(l) => out.extend_from_slice(l),
            None => out.extend_from_slice(self.page_list(n, layer)),
        }
    }

    fn entry(&self) -> Option<(Node, usize)> {
        self.root.entry
    }
}

/// A fold's graph: the snapshot's, with the nodes and lists it changed.
struct Fold<'v, 'a> {
    view: &'v View<'a>,
    width: usize,
    base: u32,
    vectors: Vec<u8>,
    changed: Changed,
    entry: Option<(Node, usize)>,
}

impl Fold<'_, '_> {
    fn list(&self, n: Node, layer: usize, out: &mut Vec<Node>) {
        match self.changed.get(&slot(n, layer)) {
            Some((l, _)) => {
                out.clear();
                out.extend_from_slice(l);
            }
            None if n >= self.base => out.clear(),
            None => self.view.neighbors(n, layer, out),
        }
    }

    #[inline]
    fn at(&self, n: Node) -> &[u8] {
        if n >= self.base {
            let i = (n - self.base) as usize * self.width;
            &self.vectors[i..i + self.width]
        } else {
            self.view.at(n)
        }
    }
}

impl Graph for Fold<'_, '_> {
    #[inline]
    fn distance(&self, q: &[u8], n: Node) -> f32 {
        self.view.shape.params.metric.distance(q, self.at(n))
    }

    fn vector(&self, n: Node, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(self.at(n));
    }

    fn neighbors(&self, n: Node, layer: usize, out: &mut Vec<Node>) {
        self.list(n, layer, out);
    }

    fn entry(&self) -> Option<(Node, usize)> {
        self.entry
    }
}

impl GraphMut for Fold<'_, '_> {
    fn add_node(&mut self, n: Node, level: usize, _vector: &[u8], edges: &[Vec<Node>]) {
        for (l, e) in edges.iter().enumerate().take(level + 1) {
            self.changed.insert(slot(n, l), (e.clone(), None));
        }
    }

    fn add_edge(&mut self, n: Node, layer: usize, to: Node) -> usize {
        let k = slot(n, layer);
        if !self.changed.contains_key(&k) {
            let mut l = Vec::new();
            self.list(n, layer, &mut l);
            let was = l.len();
            self.changed.insert(k, (l, Some(was)));
        }
        let (l, _) = self.changed.get_mut(&k).expect("inserted above");
        l.push(to);
        l.len()
    }

    fn set_neighbors(&mut self, n: Node, layer: usize, edges: &[Node]) {
        // A list only appended to stays an append when the new one extends
        // what it was.
        let k = slot(n, layer);
        let was = match self.changed.get(&k) {
            Some((old, Some(w))) if edges.len() >= *w && edges[..*w] == old[..*w] => Some(*w),
            Some(_) => None,
            None if n >= self.base => None,
            None => {
                let mut l = Vec::new();
                self.view.neighbors(n, layer, &mut l);
                (edges.len() >= l.len() && edges[..l.len()] == l[..]).then_some(l.len())
            }
        };
        self.changed.insert(k, (edges.to_vec(), was));
    }

    fn set_entry(&mut self, n: Node, level: usize) {
        self.entry = Some((n, level));
    }
}

/// Adjacency pages `touched` of a graph of `nodes` nodes written whole,
/// each node's lists as `list` gives them (in the shape's threads): each
/// page and its bytes.
fn adjacency_pages(
    touched: &[usize],
    nodes: u32,
    shape: &Shape,
    list: &(dyn Fn(Node, usize, &mut Vec<Node>) + Sync),
) -> Vec<(usize, Vec<u8>)> {
    let mut s = Scratch::default();
    graph::par_map(touched, shape.batch.threads, &mut s, |&p, _| {
        let lo = p * shape.page;
        let hi = ((p + 1) * shape.page).min(nodes as usize);
        let lists: Vec<Vec<Vec<Node>>> = (lo..hi)
            .map(|n| {
                let mut node = Vec::new();
                for layer in 0..16 {
                    let mut l = Vec::new();
                    list(n as Node, layer, &mut l);
                    node.push(l);
                }
                while node.last().is_some_and(Vec::is_empty) {
                    node.pop();
                }
                node
            })
            .collect();
        (p, AdjChunk::encode_page(&lists))
    })
}

/// Inserts `rows` (primary key, stored vector) into `view`'s graph and
/// sets its dead nodes to `dead` (`None`: as they were); the new graph
/// root, its objects written to `w` (those it replaced dropped).
/// `compact`: every changed list written into its page (no patch).
fn fold_into(
    view: &View,
    rows: &[(Vec<u8>, Vec<u8>)],
    compact: bool,
    dead: Option<Vec<Node>>,
    w: &mut Writes,
) -> Result<GraphRoot> {
    let shape = view.shape;
    let base = view.root.nodes;
    let mut f = Fold {
        view,
        width: shape.width,
        base,
        vectors: Vec::with_capacity(rows.len() * shape.width),
        changed: Changed::default(),
        entry: view.root.entry,
    };
    let mut s = Scratch::default();
    for (_, v) in rows {
        f.vectors.extend_from_slice(v);
    }
    let nodes_in: Vec<(Node, &[u8])> = rows
        .iter()
        .enumerate()
        .map(|(i, (_, v))| (base + i as Node, v.as_slice()))
        .collect();
    graph::insert_batch(&mut f, &shape.params, &nodes_in, &shape.batch, &mut s);
    view.check()?;
    let nodes = base + rows.len() as u32;

    // The dead nodes cut out once they are a share of the graph (or a
    // compaction is due anyway): each list through one rewritten, the
    // entry point moved off one; the graph then has none.
    let dead = dead.unwrap_or_else(|| view.dead.as_ref().map_or(Vec::new(), |d| d.nodes.clone()));
    let consolidate =
        !dead.is_empty() && (compact || dead.len() as f64 > CONSOLIDATE * f64::from(nodes));
    let dead = if consolidate {
        let set = Dead::new(dead);
        let is = |x: Node| set.has(x);
        let p = &shape.params;
        let all: Vec<Node> = (0..nodes).collect();
        let fixes = graph::par_map(&all, shape.batch.threads, &mut s, |&n, s| {
            let mut out = Vec::new();
            let mut l = Vec::new();
            for layer in 0..=p.level(n) {
                if set.has(n) {
                    out.push((layer, Vec::new()));
                    continue;
                }
                f.neighbors(n, layer, &mut l);
                if let Some(new) = graph::consolidate(&f, p, n, layer, &l, &is, s) {
                    out.push((layer, new));
                }
            }
            out
        });
        for (n, fix) in fixes.into_iter().enumerate() {
            for (layer, list) in fix {
                f.set_neighbors(n as Node, layer, &list);
            }
        }
        if let Some((e, _)) = f.entry
            && set.has(e)
        {
            f.entry = (0..nodes)
                .filter(|&n| !set.has(n))
                .max_by_key(|&n| (p.level(n), std::cmp::Reverse(n)))
                .map(|n| (n, p.level(n)));
        }
        Vec::new()
    } else {
        dead
    };
    view.check()?;
    let compact = compact || consolidate;
    let (changed, entry, new_vectors) = (f.changed, f.entry, f.vectors);
    let mut pages = view.dir.pages.clone();
    pages.resize((nodes as usize).div_ceil(shape.page), (0, 0));

    // Vector pages: the last partial one rewritten, new ones after.
    for (p, page) in pages
        .iter_mut()
        .enumerate()
        .skip(base as usize / shape.page)
    {
        let lo = (p * shape.page).max(base as usize);
        let hi = ((p + 1) * shape.page).min(nodes as usize);
        let mut pks: Vec<&[u8]> = Vec::with_capacity(shape.page);
        let mut data = Vec::with_capacity(shape.page * shape.width);
        for n in p * shape.page..lo {
            pks.push(
                view.pk(n as Node)
                    .ok_or_else(|| corrupt("a node has no row"))?,
            );
            data.extend_from_slice(view.at(n as Node));
        }
        let (a, b) = (lo - base as usize, hi - base as usize);
        for (pk, _) in &rows[a..b] {
            pks.push(pk);
        }
        data.extend_from_slice(&new_vectors[a * shape.width..b * shape.width]);
        let old = page.0;
        page.0 = w.object(1, VecChunk::encode_page(&pks, &data));
        if old != 0 && old != page.0 {
            w.dropped.insert(old);
        }
    }
    view.check()?;

    // A patch of the fold's changes — or, building or consolidating, the
    // pages they touch rewritten.  Otherwise the patches are rewritten
    // into pages apart from the folds ([`Index::compaction`]).
    let patch = Patch::encode(&changed);
    let (patches, patch_bytes) = if compact {
        let mut touched: BTreeSet<usize> = changed
            .keys()
            .map(|k| (k >> 8) as usize / shape.page)
            .collect();
        touched.extend(view.touched());
        let touched: Vec<usize> = touched.into_iter().collect();
        let written = adjacency_pages(&touched, nodes, &shape, &|n, layer, out| match changed
            .get(&slot(n, layer))
        {
            Some((l, _)) => {
                out.clear();
                out.extend_from_slice(l);
            }
            None if n < base => view.neighbors(n, layer, out),
            None => out.clear(),
        });
        for (p, bytes) in written {
            let old = pages[p].1;
            pages[p].1 = w.object(2, bytes);
            if old != 0 && old != pages[p].1 {
                w.dropped.insert(old);
            }
        }
        w.dropped.extend(view.root.patches.iter().copied());
        (Vec::new(), 0)
    } else if changed.is_empty() {
        (view.root.patches.clone(), view.root.patch_bytes)
    } else {
        let mut chain = view.root.patches.clone();
        chain.push(w.object(3, patch.clone()));
        (chain, view.root.patch_bytes + patch.len() as u64)
    };
    view.check()?;
    let dir = view.dir.write(&pages, w);
    if view.root.dir != 0 && view.root.dir != dir {
        w.dropped.insert(view.root.dir);
    }
    let dead = if dead.is_empty() {
        0
    } else {
        w.object(6, Dead::encode(&dead))
    };
    if view.root.dead != 0 && view.root.dead != dead {
        w.dropped.insert(view.root.dead);
    }
    Ok(GraphRoot {
        dir,
        patches,
        patch_bytes,
        nodes,
        entry,
        dead,
    })
}

// ----------------------------------------------------------------- index

/// One index: where it is, its shape.
pub struct Index {
    /// Its keys.
    pub keys: Keys,
    /// Its shape.
    pub shape: Shape,
}

impl Compaction {
    /// Its pages in batches of about `budget` bytes, each put in a
    /// transaction of its own — small, so none takes the KV's path for
    /// large ones (holding the others up), and none conflicts: the keys
    /// are new.
    pub fn batches(&self, budget: usize) -> Vec<std::ops::Range<usize>> {
        let mut out = Vec::new();
        let (mut start, mut bytes) = (0, 0);
        for (i, (_, b)) in self.objects.iter().enumerate() {
            bytes += b.len();
            if bytes >= budget {
                out.push(start..i + 1);
                (start, bytes) = (i + 1, 0);
            }
        }
        if start < self.objects.len() {
            out.push(start..self.objects.len());
        }
        out
    }

    /// Lets go of its pages' bytes (put).
    pub fn put(&mut self) {
        self.objects = Vec::new();
    }
}

/// A compaction worked out ([`Index::compaction`]), for
/// [`Index::install`]: the graph it was worked out from, and each page it
/// rewrote with its adjacency object then and now.
pub struct Compaction {
    lineage: u64,
    from: GraphRoot,
    pages: Vec<(usize, u64, u64)>,
    objects: Vec<(u64, Vec<u8>)>,
}

/// Each row's nodes in a graph, by a hash of the primary key: what a fold
/// marks dead.  Kept in memory per lineage, grown as nodes are added
/// (a node's row never changes).
#[derive(Default)]
pub struct ByRow {
    lineage: u64,
    covered: u32,
    sorted: Vec<(u64, Node)>,
}

fn pk_hash(pk: &[u8]) -> u64 {
    object_id(9, pk)
}

impl Index {
    fn root_in(&self, txn: &dyn KvRead) -> Result<Root> {
        match txn.get(&self.keys.root())? {
            Some(b) => Root::decode(&b),
            None => Ok(Root::default()),
        }
    }

    /// The root as `txn` sees it.
    pub fn root(&self, txn: &dyn KvRead) -> Result<Root> {
        self.root_in(txn)
    }

    /// Builds the graph over `rows` (primary key, stored vector) in `txn`
    /// (the index's prefix empty).
    pub fn build(&self, txn: &mut dyn KvTxn, rows: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        let mut w = Writes::new();
        let root = Root {
            lineage: nonce(),
            ..Root::default()
        };
        let graph = {
            let r = txn.shared().ok_or_else(|| {
                Error::internal("hnsw needs a KV that shares a transaction's reads")
            })?;
            let view = View::open(self.shape, &self.keys, r, &root)?;
            fold_into(&view, rows, true, None, &mut w)?
        };
        w.apply(txn, &self.keys)?;
        let root = Root { graph, ..root };
        txn.put(&self.keys.root(), &root.encode())?;
        Ok(())
    }

    /// Seals the delta `txn` sees: its rows and dead keys moved into one
    /// sealed object; the rows moved (0: none were there).
    pub fn seal(&self, txn: &mut dyn KvTxn) -> Result<usize> {
        let (mut rows, mut gone) = (Vec::new(), Vec::new());
        let mut keys = Vec::new();
        {
            let (start, end) = self.keys.range(b'd');
            for e in txn.scan(Bound::Included(&start), Bound::Excluded(&end))? {
                let (k, v) = e?;
                if v.len() != self.shape.width {
                    return Err(corrupt("a delta row of another width"));
                }
                rows.push((self.keys.delta_pk(&k).to_vec(), v));
                keys.push(k);
            }
            let (start, end) = self.keys.range(b'x');
            for e in txn.scan(Bound::Included(&start), Bound::Excluded(&end))? {
                let (k, _) = e?;
                let (pk, hash) = self.keys.dead_pk(&k);
                gone.push((pk.to_vec(), hash));
                keys.push(k);
            }
        }
        let n = rows.len() + gone.len();
        if n == 0 {
            return Ok(0);
        }
        let mut root = self.root_in(
            txn.shared()
                .ok_or_else(|| Error::internal("shared reads"))?,
        )?;
        let b = Sealed::encode(&rows, &gone);
        let id = object_id(8, &b);
        txn.put(&self.keys.obj(id), &b)?;
        root.sealed.push((id, n as u32));
        txn.put(&self.keys.root(), &root.encode())?;
        for k in keys {
            txn.delete(&k)?;
        }
        Ok(n)
    }

    /// Sealed object `id`, from memory.
    fn sealed(&self, txn: &dyn KvRead, id: u64) -> Result<Arc<Sealed>> {
        let width = self.shape.width;
        cache::get(Kind::Sealed, id, || {
            let b = txn
                .get(&self.keys.obj(id))?
                .ok_or_else(|| corrupt("a sealed object is missing"))?;
            let s = Sealed::decode(&b, width)?;
            let size = s.size();
            Ok((s, size))
        })
    }

    /// Folds every sealed object into the graph in `txn`: the rows the
    /// table still has as they are (`current`: a row's stored vector now,
    /// `None` if it has none) inserted, the nodes of rows changed or gone
    /// marked dead; the rows folded.
    pub fn fold(
        &self,
        txn: &mut dyn KvTxn,
        by_row: &mut ByRow,
        current: &dyn Fn(&dyn KvTxn, &[u8]) -> Result<Option<Vec<u8>>>,
    ) -> Result<usize> {
        let mut w = Writes::new();
        let (root, folded) = {
            let ro: &dyn KvTxn = &*txn;
            let r = ro.shared().ok_or_else(|| Error::internal("shared reads"))?;
            let root = self.root_in(r)?;
            if root.sealed.is_empty() {
                return Ok(0);
            }
            let mut rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            let mut gone: Vec<(Vec<u8>, u64)> = Vec::new();
            let mut folded = 0;
            for &(id, n) in &root.sealed {
                let s = self.sealed(r, id)?;
                folded += n as usize;
                for (i, v) in s.data.chunks_exact(self.shape.width).enumerate() {
                    rows.push((s.pks.get(i).to_vec(), v.to_vec()));
                }
                gone.extend(s.gone.iter().cloned());
            }
            // Only rows as the table still has them (the newest write of
            // a row wins; one written twice the same, once).
            let mut kept: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(rows.len());
            let mut seen: std::collections::HashSet<Vec<u8>> = Default::default();
            for (pk, v) in rows.into_iter().rev() {
                if seen.contains(&pk) {
                    continue;
                }
                if current(ro, &pk)?.as_deref() == Some(v.as_slice()) {
                    seen.insert(pk.clone());
                    kept.push((pk, v));
                }
            }
            kept.reverse();
            let view = View::open(self.shape, &self.keys, r, &root)?;
            // Rows changed or gone: their nodes whose vector the row no
            // longer has, dead.  A row whose new vector is folded now has
            // its old node marked too (it is in `gone`).
            let marks = if gone.is_empty() {
                None
            } else {
                Some(self.mark(&view, ro, by_row, &gone, current)?)
            };
            let graph = fold_into(&view, &kept, false, marks, &mut w)?;
            let mut root = root;
            for &(id, _) in &root.sealed {
                w.dropped.insert(id);
            }
            root.sealed.clear();
            root.graph = graph;
            (root, folded)
        };
        w.apply(txn, &self.keys)?;
        txn.put(&self.keys.root(), &root.encode())?;
        Ok(folded)
    }

    /// Whether the graph's patches, as `txn` sees them, have outgrown
    /// `compact` of its adjacency: a [`compaction`](Self::compaction) due.
    pub fn compaction_due(&self, txn: &dyn KvRead) -> Result<bool> {
        let g = self.root_in(txn)?.graph;
        let adjacency = f64::from(g.nodes) * (self.shape.params.m * 2 * 4) as f64;
        Ok(!g.patches.is_empty() && g.patch_bytes as f64 > self.shape.compact * adjacency)
    }

    /// The pages the graph's patches touch, rewritten with them as `r`
    /// sees the graph — for [`put`](Self::put_compaction) (new objects
    /// only, no key another transaction writes) and
    /// [`install`](Self::install), which swaps them in for the patches.
    /// The rewrite is the long part, so it is done from a snapshot apart
    /// from the folds, which go on meanwhile: an LSM's compaction beside
    /// its flushes.  `None`: no patches.
    pub fn compaction(&self, r: &dyn KvRead) -> Result<Option<Compaction>> {
        let root = self.root_in(r)?;
        if root.graph.patches.is_empty() {
            return Ok(None);
        }
        let view = View::open(self.shape, &self.keys, r, &root)?;
        let touched: Vec<usize> = view.touched().into_iter().collect();
        let written = adjacency_pages(&touched, root.graph.nodes, &self.shape, &|n, layer, out| {
            view.neighbors(n, layer, out)
        });
        view.check()?;
        let mut objects = Vec::with_capacity(written.len());
        let pages = written
            .into_iter()
            .map(|(p, bytes)| {
                let id = object_id(2, &bytes);
                objects.push((id, bytes));
                (p, view.dir.pages[p].1, id)
            })
            .collect();
        Ok(Some(Compaction {
            lineage: root.lineage,
            from: root.graph,
            pages,
            objects,
        }))
    }

    /// Puts compaction `c`'s pages in batch `b` of
    /// [`Compaction::batches`] in `txn`.
    pub fn put_compaction(
        &self,
        txn: &mut dyn KvTxn,
        c: &Compaction,
        b: std::ops::Range<usize>,
    ) -> Result<()> {
        for (id, bytes) in &c.objects[b] {
            txn.put(&self.keys.obj(*id), bytes)?;
        }
        Ok(())
    }

    /// Swaps compaction `c` in for the patches it took, in `txn`: true if
    /// the graph changed since only by folds (their patches, after `c`'s,
    /// stay); otherwise (a consolidation or another compaction rewrote
    /// pages) false, and its pages deleted.
    pub fn install(&self, txn: &mut dyn KvTxn, c: &Compaction) -> Result<bool> {
        let mut w = Writes::new();
        let root = {
            let ro: &dyn KvTxn = &*txn;
            let r = ro.shared().ok_or_else(|| Error::internal("shared reads"))?;
            let mut root = self.root_in(r)?;
            let objects = Objects {
                keys: &self.keys,
                txn: r,
            };
            let dir = Dir::load(&objects, root.graph.dir, self.shape.chunk / self.shape.page)?;
            let ok = root.lineage == c.lineage
                && root.graph.patches.starts_with(&c.from.patches)
                && c.pages
                    .iter()
                    .all(|&(p, old, _)| dir.pages.get(p).is_some_and(|x| x.1 == old));
            if !ok {
                let live: BTreeSet<u64> = dir.pages.iter().map(|x| x.1).collect();
                for &(_, _, new) in &c.pages {
                    if !live.contains(&new) {
                        w.dropped.insert(new);
                    }
                }
                None
            } else {
                let mut pages = dir.pages.clone();
                for &(p, old, new) in &c.pages {
                    pages[p].1 = new;
                    if old != 0 && old != new {
                        w.dropped.insert(old);
                    }
                }
                w.dropped.extend(c.from.patches.iter().copied());
                let id = dir.write(&pages, &mut w);
                if root.graph.dir != id {
                    w.dropped.insert(root.graph.dir);
                }
                root.graph.dir = id;
                root.graph.patches.drain(..c.from.patches.len());
                root.graph.patch_bytes -= c.from.patch_bytes;
                Some(root)
            }
        };
        w.apply(txn, &self.keys)?;
        match root {
            Some(root) => {
                txn.put(&self.keys.root(), &root.encode())?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Reads the graph's adjacency into memory as `txn` sees it — after a
    /// compaction, so the searches do not (RocksDB's
    /// `prepopulate_block_cache` for a compaction's output).
    pub fn warm(&self, txn: &dyn KvRead) -> Result<()> {
        let root = self.root_in(txn)?;
        let view = View::open(self.shape, &self.keys, txn, &root)?;
        for c in 0..view.dir.adjs.len() {
            view.page_list((c * self.shape.chunk) as Node, 0);
        }
        view.check()
    }

    /// `view`'s dead nodes with those of the rows `gone` (each row's node
    /// with the vector it had, unless the table has the row with that
    /// vector again), sorted.
    fn mark(
        &self,
        view: &View,
        ro: &dyn KvTxn,
        by_row: &mut ByRow,
        gone: &[(Vec<u8>, u64)],
        current: &dyn Fn(&dyn KvTxn, &[u8]) -> Result<Option<Vec<u8>>>,
    ) -> Result<Vec<Node>> {
        if by_row.lineage != view.lineage || by_row.covered > view.root.nodes {
            *by_row = ByRow {
                lineage: view.lineage,
                ..ByRow::default()
            };
        }
        if by_row.covered < view.root.nodes {
            for n in by_row.covered..view.root.nodes {
                let pk = view.pk(n).ok_or_else(|| corrupt("a node has no row"))?;
                by_row.sorted.push((pk_hash(pk), n));
            }
            view.check()?;
            by_row.sorted.sort_unstable();
            by_row.covered = view.root.nodes;
        }
        let mut dead: Vec<Node> = view.dead.as_ref().map_or(Vec::new(), |d| d.nodes.clone());
        for (pk, hash) in gone {
            let h = pk_hash(pk);
            let from = by_row.sorted.partition_point(|e| e.0 < h);
            let now = current(ro, pk)?;
            for &(eh, n) in &by_row.sorted[from..] {
                if eh != h {
                    break;
                }
                let v = view.at(n);
                if view.pk(n) == Some(pk.as_slice())
                    && vector_hash(v) == *hash
                    && now.as_deref() != Some(v)
                {
                    dead.push(n);
                }
            }
        }
        view.check()?;
        dead.sort_unstable();
        dead.dedup();
        Ok(dead)
    }

    /// The rows nearest `q` (a stored vector) as `txn` sees the index: up
    /// to `ef` from the graph (its dead nodes passed over) and the nearest
    /// `ef` of the sealed and delta rows, nearest first by the graph's
    /// distance — unchecked: a row may be there more than once, with a
    /// vector it no longer has, or gone.
    pub fn candidates(
        &self,
        txn: &dyn KvRead,
        q: &[u8],
        ef: usize,
        s: &mut Scratch,
    ) -> Result<Candidates> {
        let root = self.root_in(txn)?;
        let metric = self.shape.params.metric;
        let mut out: Vec<(f32, Vec<u8>)> = Vec::new();
        let mut held = root.graph.nodes as usize;
        let mut all = true;
        if root.graph.entry.is_some() {
            let view = View::open(self.shape, &self.keys, txn, &root)?;
            let found = graph::search(&view, q, ef, ef, s);
            all = found.len() < ef;
            for Near { d, n } in found {
                if view.is_dead(n) {
                    continue;
                }
                if let Some(pk) = view.pk(n) {
                    out.push((d, pk.to_vec()));
                }
            }
            view.check()?;
        }
        let t1 = std::time::Instant::now();
        // The sealed objects from memory, the delta from the KV: the
        // nearest `ef` of them.
        let mut near: std::collections::BinaryHeap<(OrdF32, Vec<u8>)> = Default::default();
        let mut others = 0;
        let mut push = |d: f32, pk: &[u8]| {
            others += 1;
            if near.len() < ef {
                near.push((OrdF32(d), pk.to_vec()));
            } else if near.peek().is_some_and(|top| OrdF32(d) < top.0) {
                near.pop();
                near.push((OrdF32(d), pk.to_vec()));
            }
        };
        let mut sealed_n = 0;
        for &(id, _) in &root.sealed {
            let sealed = self.sealed(txn, id)?;
            sealed_n += sealed.data.len() / self.shape.width;
            for (i, v) in sealed.data.chunks_exact(self.shape.width).enumerate() {
                push(metric.distance(q, v), sealed.pks.get(i));
            }
        }
        let t2 = std::time::Instant::now();
        let (start, end) = self.keys.range(b'd');
        for e in txn.scan(Bound::Included(&start), Bound::Excluded(&end))? {
            let (k, v) = e?;
            if v.len() == self.shape.width {
                push(metric.distance(q, &v), self.keys.delta_pk(&k));
            }
        }
        let t3 = std::time::Instant::now();
        all &= others <= ef;
        held += others;
        out.extend(near.into_iter().map(|(d, pk)| (d.0, pk)));
        out.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let delta = others - sealed_n;
        Ok(Candidates {
            near: out,
            all,
            held,
            sealed: (sealed_n, (t2 - t1).as_nanos() as u64),
            delta: (delta, (t3 - t2).as_nanos() as u64),
        })
    }

    /// What the index holds as `txn` sees it.
    pub fn status(&self, txn: &dyn KvRead) -> Result<Status> {
        let root = self.root_in(txn)?;
        let dead = if root.graph.dead == 0 {
            0
        } else {
            let b = txn
                .get(&self.keys.obj(root.graph.dead))?
                .ok_or_else(|| corrupt("the dead set is missing"))?;
            Dead::decode(&b)?.nodes.len()
        };
        let (start, end) = self.keys.range(b'd');
        let mut delta = txn
            .scan(Bound::Included(&start), Bound::Excluded(&end))?
            .count();
        let (start, end) = self.keys.range(b'x');
        delta += txn
            .scan(Bound::Included(&start), Bound::Excluded(&end))?
            .count();
        Ok(Status {
            nodes: root.graph.nodes,
            dead,
            patches: root.graph.patches.len(),
            sealed: root.sealed_rows(),
            delta,
        })
    }

    /// The integrity check: the root and the objects it names readable,
    /// the graph's edges in range; each of `rows` (primary key, stored
    /// vector: the table's rows the index should hold) in the graph live,
    /// in a sealed object or in the delta with that vector.
    pub fn check(
        &self,
        txn: &dyn KvRead,
        name: &str,
        rows: &[(Vec<u8>, Vec<u8>)],
        problems: &mut Vec<String>,
    ) -> Result<()> {
        let root = match self.root_in(txn) {
            Ok(r) => r,
            Err(e) => {
                problems.push(format!("index {name}: its root: {}", e.message));
                return Ok(());
            }
        };
        let mut held: std::collections::HashSet<(Vec<u8>, Vec<u8>)> = Default::default();
        let view = match View::open(self.shape, &self.keys, txn, &root) {
            Ok(v) => v,
            Err(e) => {
                problems.push(format!("index {name}: its graph: {}", e.message));
                return Ok(());
            }
        };
        let nodes = root.graph.nodes;
        if let Some((e, _)) = root.graph.entry
            && e >= nodes
        {
            problems.push(format!("index {name}: its entry point is no node"));
        }
        let mut list = Vec::new();
        let mut bad_edges = 0;
        for n in 0..nodes {
            let level = self.shape.params.level(n);
            for layer in 0..=level {
                view.neighbors(n, layer, &mut list);
                bad_edges += list.iter().filter(|&&x| x >= nodes).count();
            }
            if !view.is_dead(n)
                && let Some(pk) = view.pk(n)
            {
                held.insert((pk.to_vec(), view.at(n).to_vec()));
            }
        }
        if let Err(e) = view.check() {
            problems.push(format!("index {name}: its graph: {}", e.message));
            return Ok(());
        }
        if bad_edges > 0 {
            problems.push(format!("index {name}: {bad_edges} edges to no node"));
        }
        for &(id, _) in &root.sealed {
            match self.sealed(txn, id) {
                Ok(s) => {
                    for (i, v) in s.data.chunks_exact(self.shape.width).enumerate() {
                        held.insert((s.pks.get(i).to_vec(), v.to_vec()));
                    }
                }
                Err(e) => problems.push(format!("index {name}: a sealed object: {}", e.message)),
            }
        }
        let (start, end) = self.keys.range(b'd');
        for e in txn.scan(Bound::Included(&start), Bound::Excluded(&end))? {
            let (k, v) = e?;
            held.insert((self.keys.delta_pk(&k).to_vec(), v));
        }
        let missing = rows.iter().filter(|r| !held.contains(*r)).count();
        if missing > 0 {
            problems.push(format!("index {name}: {missing} rows missing"));
        }
        Ok(())
    }
}

/// What an index holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    /// The graph's nodes (the dead ones included).
    pub nodes: u32,
    /// Its dead nodes.
    pub dead: usize,
    /// The patches over its pages.
    pub patches: usize,
    /// Rows (written or gone) sealed, not yet folded.
    pub sealed: u64,
    /// Keys in the delta (rows written, rows gone), not yet sealed.
    pub delta: usize,
}

/// An `f32` ordered totally (for a heap).
#[derive(Clone, Copy, PartialEq)]
struct OrdF32(f32);

impl Eq for OrdF32 {}

impl PartialOrd for OrdF32 {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}

impl Ord for OrdF32 {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&o.0)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use nucleus_kv::{Database, Kv, Options, TxnOptions};
    use nucleus_platform::sim::SimPlatform;

    use super::*;
    use crate::hnsw::graph::Metric;

    fn vector(i: u32) -> Vec<u8> {
        [i % 37, i % 11, i % 5, i / 37 % 13]
            .iter()
            .flat_map(|&x| (x as f32).to_le_bytes())
            .collect()
    }

    fn pk(i: u32) -> Vec<u8> {
        i.to_be_bytes().to_vec()
    }

    fn commit<T>(kv: &dyn Kv, f: impl FnOnce(&mut dyn KvTxn) -> T) -> T {
        let mut t = kv.begin(TxnOptions::default()).unwrap();
        let v = f(t.as_mut());
        t.commit().unwrap();
        v
    }

    /// Rows `range` written to the delta, sealed and folded.
    fn fold_rows(
        kv: &dyn Kv,
        ix: &Index,
        range: std::ops::Range<u32>,
        rows: &mut HashMap<Vec<u8>, Vec<u8>>,
        by_row: &mut ByRow,
    ) {
        commit(kv, |t| {
            for i in range {
                t.put(&ix.keys.delta(&pk(i), nonce()), &vector(i)).unwrap();
                rows.insert(pk(i), vector(i));
            }
        });
        commit(kv, |t| ix.seal(t).unwrap());
        let rows = &*rows;
        commit(kv, |t| {
            ix.fold(t, by_row, &|_, pk| Ok(rows.get(pk).cloned()))
                .unwrap()
        });
    }

    fn root(kv: &dyn Kv, ix: &Index) -> Root {
        let t = kv.begin(TxnOptions::read_only()).unwrap();
        ix.root(t.shared().unwrap()).unwrap()
    }

    /// The index whole, and its ten nearest of a query the exact ones.
    fn sound(kv: &dyn Kv, ix: &Index, rows: &HashMap<Vec<u8>, Vec<u8>>) {
        let t = kv.begin(TxnOptions::read_only()).unwrap();
        let r = t.shared().unwrap();
        let all: Vec<(Vec<u8>, Vec<u8>)> =
            rows.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut problems = Vec::new();
        ix.check(r, "t", &all, &mut problems).unwrap();
        assert_eq!(problems, Vec::<String>::new());
        let q = vector(5);
        let mut exact: Vec<f32> = rows.values().map(|v| Metric::L2.distance(&q, v)).collect();
        exact.sort_by(f32::total_cmp);
        let found = ix
            .candidates(r, &q, rows.len(), &mut Scratch::default())
            .unwrap();
        let got: Vec<f32> = found.near.iter().take(10).map(|c| c.0).collect();
        assert_eq!(got, exact[..10]);
    }

    #[test]
    fn a_compaction_goes_in_between_folds() {
        let p = Arc::new(SimPlatform::new(3));
        let db = Database::open_on(p, Path::new("/db"), Options::default()).unwrap();
        let kv: &dyn Kv = &db;
        let params = Params {
            m: 8,
            ef_construction: 32,
            metric: Metric::L2,
            slack: 1,
        };
        let ix = Index {
            keys: Keys::new(vec![0x7f, 1]),
            shape: Shape::new(params, 16, 2),
        };
        let mut rows: HashMap<Vec<u8>, Vec<u8>> = (0..1000).map(|i| (pk(i), vector(i))).collect();
        let mut sorted: Vec<(Vec<u8>, Vec<u8>)> =
            rows.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        sorted.sort();
        commit(kv, |t| ix.build(t, &sorted).unwrap());
        let mut by_row = ByRow::default();

        // A fold leaves a patch, past its share: a compaction is due.
        fold_rows(kv, &ix, 1000..1100, &mut rows, &mut by_row);
        assert_eq!(root(kv, &ix).graph.patches.len(), 1);
        {
            let t = kv.begin(TxnOptions::read_only()).unwrap();
            assert!(ix.compaction_due(t.shared().unwrap()).unwrap());
        }

        // Worked out from a snapshot; a fold meanwhile; swapped in: the
        // later fold's patch stays, on the rewritten pages.
        let snapshot = kv.begin(TxnOptions::read_only()).unwrap();
        let c = ix.compaction(snapshot.shared().unwrap()).unwrap().unwrap();
        let c2 = ix.compaction(snapshot.shared().unwrap()).unwrap().unwrap();
        drop(snapshot);
        for b in c.batches(1000) {
            commit(kv, |t| ix.put_compaction(t, &c, b).unwrap());
        }
        fold_rows(kv, &ix, 1100..1200, &mut rows, &mut by_row);
        let before = root(kv, &ix).graph;
        assert_eq!(before.patches.len(), 2);
        assert!(commit(kv, |t| ix.install(t, &c).unwrap()));
        let after = root(kv, &ix).graph;
        assert_eq!(after.patches, before.patches[1..]);
        assert_eq!(after.patch_bytes, before.patch_bytes - c.from.patch_bytes);
        assert_eq!(after.nodes, 1200);
        sound(kv, &ix, &rows);

        // Another worked out from the same graph: refused, its pages gone.
        for b in c2.batches(1000) {
            commit(kv, |t| ix.put_compaction(t, &c2, b).unwrap());
        }
        assert!(!commit(kv, |t| ix.install(t, &c2).unwrap()));
        {
            let t = kv.begin(TxnOptions::read_only()).unwrap();
            let live: BTreeSet<u64> = c.pages.iter().map(|x| x.2).collect();
            for &(_, _, id) in &c2.pages {
                let there = t.get(&ix.keys.obj(id)).unwrap().is_some();
                assert_eq!(there, live.contains(&id), "page object {id:x}");
            }
        }
        sound(kv, &ix, &rows);
        let t = kv.begin(TxnOptions::read_only()).unwrap();
        ix.warm(t.shared().unwrap()).unwrap();
    }
}
