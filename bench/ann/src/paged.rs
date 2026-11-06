//! (C), PLAN.md §M14.12 V4 — the model chosen: a graph stored as immutable objects in
//! the KV — vector pages (written once: new nodes go to new pages),
//! adjacency pages, and a patch of the edge lists changed since the pages
//! were written — each keyed by a hash of its contents (as the column
//! store's segments), so decoded objects are cached in memory across
//! transactions and a hop is a memory access.  Rows written since go to a
//! delta, a key per row (writers never conflict), searched exactly.
//!
//! One graph; a fold inserts the delta into it in one transaction
//! (FreshDiskANN's merge of its fresh index into the long-term one, Singh
//! et al. 2021), the changed lists kept in the patch until it has grown
//! past a share of the graph's edges, then the pages it touches rewritten
//! (an LSM's compaction).  (B), segments — a graph per seal, merged by size
//! tier — was measured beside it and dropped (PLAN.md): a search visits
//! every graph.  Objects a fold replaces are deleted in its transaction (a snapshot
//! older than it still reads them: MVCC).
//!
//! Keys: [`ROOT`] (the graphs: each one's directory and patch, its nodes
//! and entry point), [`OBJ`] + id (objects), [`DELTA`] + row (vectors not
//! yet in a graph).  A node is a dense position in its graph; a vector
//! page keeps each node's row.

use std::collections::{BTreeSet, HashMap};
use std::ops::Bound;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

use nucleus_kernels::{MixState, mix};
use nucleus_kv::{Kv, KvRead, KvTxn, TxnOptions, prefix_end, transact};

use crate::hnsw::{self, Graph, GraphMut, Near, Node, Params, Scratch};
use crate::model::{Hit, Model};

/// The root: one key.
pub const ROOT: &[u8] = b"c/r";
/// Objects, by id.
pub const OBJ: &[u8] = b"c/o/";
/// The delta, by row (with `epochs`: by epoch and row).
pub const DELTA: &[u8] = b"c/d/";
/// The epoch writers write the delta in (with `epochs`).
pub const EPOCH: &[u8] = b"c/e";
/// The table the index is on (with `table`), by row: each row's vector.
pub const TABLE: &[u8] = b"c/t/";
/// Rows deleted or updated, by epoch, row and a hash of the vector they
/// had (with `table`): what a fold marks dead in the graph.
pub const DEAD: &[u8] = b"c/x/";

/// The model's settings.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// The graph's shape.
    pub params: Params,
    /// A vector's bytes.
    pub width: usize,
    /// Nodes per page (what a write rewrites).
    pub page: usize,
    /// Nodes per chunk: in memory the pages are decoded into chunks of
    /// this many nodes (a multiple of `page`), so a hop finds its vector
    /// and its lists one small table away, whatever the pages' size.
    pub chunk: usize,
    /// (C): the patch's share of the graph's edges past which a fold
    /// rewrites the pages it touches.
    pub compact: f64,
    /// How a fold batches its inserts (`ratio` 0: a row at a time).
    pub batch: hnsw::Batching,
    /// The delta keyed by epoch: a fold first closes the open epoch (a
    /// short transaction; a writer validates the epoch it read, so none
    /// writes into a closed one), then folds the closed epochs and records
    /// the last in the root — a search reads the delta from the next
    /// epoch on, never the keys a fold deleted.  Off: keyed by row, the
    /// keys a fold deleted read past until a VACUUM.
    pub epochs: bool,
    /// With `epochs`: rows per epoch — the open epoch is closed once it
    /// holds this many, folded or not, and a closed epoch, no longer
    /// changing, is searched from memory (an LSM's immutable memtable);
    /// only the open one is read from the KV.  0: an epoch closes when it
    /// is folded.
    pub seal: usize,
    /// With `epochs`: the fold and the seal sizes chosen as the upkeep
    /// runs, each an economic order quantity of costs measured meanwhile
    /// ([`Meter`]); `seal` and the threshold given then cap them.
    pub auto: bool,
    /// The table the index is on kept beside it (a key per row), so rows
    /// can be deleted and updated: a search checks what it found against
    /// the table in its snapshot (a row gone, or with another vector, is
    /// passed over; fewer than `k` left, it asks again with `ef` doubled —
    /// pgvector 0.8's iterative scans); a fold marks the graph's nodes of
    /// rows deleted or updated dead (passed over unread) and, once they
    /// are a share of the graph, cuts them out (FreshDiskANN's delete
    /// consolidation).  With `epochs`.
    pub table: bool,
}

/// The share of a graph's nodes dead past which a fold cuts them out.
const CONSOLIDATE: f64 = 0.02;

/// An economic order quantity (Harris 1913): the batch that makes a
/// fixed cost `k` (ns) a batch, against a cost `h` (ns a second) for
/// each item waiting, cheapest at `rate` items a second — √(2·k·rate/h).
/// Nothing waiting costs (`h` 0): no bound.
fn eoq(k: f64, rate: f64, h: f64) -> f64 {
    if h <= 0.0 {
        f64::INFINITY
    } else {
        (2.0 * k * rate / h).sqrt()
    }
}

/// What the upkeep's sizes weigh, measured as the model runs: how fast
/// rows come and searches ask, what a delta row costs a search from
/// memory and from the KV, what a fold costs beyond its rows (a line
/// fitted to folds' times against their rows) and what closing an epoch
/// costs (its transaction and the writes it sends back).
#[derive(Default)]
struct Meter {
    searches: AtomicU64,
    mem: (AtomicU64, AtomicU64),
    kv: (AtomicU64, AtomicU64),
    inserts: (AtomicU64, AtomicU64),
    retries: AtomicU64,
    rates: Mutex<Rates>,
}

/// The meter's running estimates.
#[derive(Default)]
struct Rates {
    at: Option<std::time::Instant>,
    seen: (u64, u64),
    insert_rate: f64,
    search_rate: f64,
    /// Folds' rows against their ns, as sums decayed (n, Σx, Σy, Σxy, Σx²).
    fit: [f64; 5],
    close_ns: f64,
    retries_at_close: u64,
    /// The last sizes chosen, and which way the fold's is nudged next.
    fold: f64,
    seal: f64,
    nudge: bool,
    /// The fold's fixed cost, as last estimated.
    k: f64,
}

impl Meter {
    fn per_row(c: &(AtomicU64, AtomicU64)) -> f64 {
        let rows = c.0.load(Relaxed);
        if rows == 0 {
            0.0
        } else {
            c.1.load(Relaxed) as f64 / rows as f64
        }
    }

    /// Samples the rates (an exponential average, a sample a tenth of a
    /// second at most).
    fn sample(&self) {
        let mut r = self.rates.lock().unwrap();
        let now = std::time::Instant::now();
        let (ins, sea) = (self.inserts.0.load(Relaxed), self.searches.load(Relaxed));
        let Some(at) = r.at else {
            r.at = Some(now);
            r.seen = (ins, sea);
            return;
        };
        let dt = now.duration_since(at).as_secs_f64();
        if dt < 0.1 {
            return;
        }
        let (i, s) = ((ins - r.seen.0) as f64 / dt, (sea - r.seen.1) as f64 / dt);
        let first = r.insert_rate == 0.0 && r.search_rate == 0.0;
        let a = if first { 1.0 } else { 0.3 };
        r.insert_rate += a * (i - r.insert_rate);
        r.search_rate += a * (s - r.search_rate);
        r.at = Some(now);
        r.seen = (ins, sea);
    }

    fn report(&self) -> String {
        let r = self.rates.lock().unwrap();
        format!(
            "; sizes: fold {:.0} (fixed cost {:.1} ms), seal {:.0} (inserts {:.0}/s, searches {:.0}/s, a delta row {:.0} ns in memory, {:.0} ns in the KV, a close {:.2} ms)",
            r.fold,
            r.k / 1e6,
            r.seal,
            r.insert_rate,
            r.search_rate,
            Self::per_row(&self.mem),
            Self::per_row(&self.kv),
            r.close_ns / 1e6
        )
    }

    /// A fold of `rows` took `ns`.
    fn folded(&self, rows: usize, ns: f64) {
        let mut r = self.rates.lock().unwrap();
        let x = rows as f64;
        for (f, v) in r.fit.iter_mut().zip([1.0, x, ns, x * ns, x * x]) {
            *f = 0.8 * *f + v;
        }
    }

    /// A close took `ns`: with the writes it sent back (the retries since
    /// the last close, at a write's mean cost).
    fn closed(&self, ns: f64) {
        let mut r = self.rates.lock().unwrap();
        let retries = self.retries.load(Relaxed);
        let back = (retries - r.retries_at_close) as f64 * Self::per_row(&self.inserts);
        r.retries_at_close = retries;
        let cost = ns + back;
        r.close_ns = if r.close_ns == 0.0 {
            cost
        } else {
            r.close_ns + 0.3 * (cost - r.close_ns)
        };
    }

    /// The fold and seal sizes now, `fold` and `seal` capping them.
    fn sizes(&self, fold: usize, seal: usize) -> (usize, usize) {
        let mut r = self.rates.lock().unwrap();
        let [n, sx, sy, sxy, sxx] = r.fit;
        // The fit's intercept, once the folds' sizes differ enough to
        // tell it from the slope; until then a tenth of a fold's mean.
        let var = n * sxx - sx * sx;
        let k = if n >= 3.0 && var > 1e-3 * sx * sx {
            let slope = (n * sxy - sx * sy) / var;
            (sy - slope * sx) / n
        } else if n > 0.0 {
            0.1 * sy / n
        } else {
            0.0
        }
        .max(1e6);
        let (mem, kv) = (Self::per_row(&self.mem), Self::per_row(&self.kv));
        let b = eoq(k, r.insert_rate, r.search_rate * mem).clamp(64.0, fold as f64);
        // Nudged a fifth either way in turn, so the folds' sizes differ.
        r.nudge = !r.nudge;
        r.fold = b;
        r.k = k;
        let b = if r.nudge { b * 1.2 } else { b * 0.8 };
        let e = eoq(
            r.close_ns.max(1e4),
            r.insert_rate,
            r.search_rate * (kv - mem),
        )
        .clamp(16.0, seal.min(fold) as f64);
        r.seal = e;
        (b as usize, e as usize)
    }
}

/// A graph's root: its pages, the patches written since they were (the
/// oldest first) and their bytes, its nodes and entry point.
#[derive(Clone, Default, Debug)]
struct GraphRoot {
    dir: u64,
    patches: Vec<u64>,
    patch_bytes: u64,
    nodes: u32,
    entry: Option<(Node, usize)>,
    /// Its dead nodes (an object; 0: none).
    dead: u64,
}

/// The root: the last epoch folded and the graphs.
#[derive(Default)]
struct Root {
    folded: u32,
    graphs: Vec<GraphRoot>,
}

fn encode_root(folded: u32, graphs: &[GraphRoot]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + 26 * graphs.len());
    b.extend_from_slice(&folded.to_le_bytes());
    b.extend_from_slice(&(graphs.len() as u32).to_le_bytes());
    for g in graphs {
        b.extend_from_slice(&g.dir.to_le_bytes());
        b.extend_from_slice(&g.patch_bytes.to_le_bytes());
        b.extend_from_slice(&(g.patches.len() as u16).to_le_bytes());
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
    }
    b
}

fn decode_root(b: &[u8]) -> Root {
    let mut r = Reader(b);
    let folded = r.u32();
    let n = r.u32() as usize;
    let graphs = (0..n)
        .map(|_| {
            let (dir, patch_bytes) = (r.u64(), r.u64());
            let patches = (0..r.u16()).map(|_| r.u64()).collect();
            let nodes = r.u32();
            let entry = (r.u8() == 1).then(|| (r.u32(), r.u8() as usize));
            let dead = r.u64();
            GraphRoot {
                dir,
                patches,
                patch_bytes,
                nodes,
                entry,
                dead,
            }
        })
        .collect();
    Root { folded, graphs }
}

fn read_root(txn: &dyn KvRead) -> Root {
    txn.get(ROOT)
        .expect("root read")
        .map(|b| decode_root(&b))
        .unwrap_or_default()
}

fn read_epoch(txn: &dyn KvRead) -> u32 {
    txn.get(EPOCH)
        .expect("epoch read")
        .map_or(1, |b| u32::from_le_bytes(b[..4].try_into().unwrap()))
}

/// Each row's nodes.
type ByRow = HashMap<u32, Vec<Node>, MixState>;

/// A graph's dead nodes: sorted, and as bits.
struct Dead {
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

    fn decode(b: &[u8]) -> Self {
        let mut r = Reader(b);
        Dead::new((0..r.u32()).map(|_| r.u32()).collect())
    }
}

/// A vector's hash (a deleted row's, matched to its node).
fn vector_hash(v: &[u8]) -> u64 {
    object_id(7, v)
}

fn table_key(row: u32) -> Vec<u8> {
    let mut k = TABLE.to_vec();
    k.extend_from_slice(&row.to_be_bytes());
    k
}

/// A closed epoch's rows and vectors, in memory: it no longer changes.
struct Sealed {
    rows: Vec<u32>,
    data: Vec<u8>,
}

/// The `k` nearest seen, the farthest on top.
struct Nearest {
    k: usize,
    heap: std::collections::BinaryHeap<Near>,
}

impl Nearest {
    fn new(k: usize) -> Self {
        Nearest {
            k,
            heap: std::collections::BinaryHeap::with_capacity(k + 1),
        }
    }

    #[inline]
    fn push(&mut self, d: f32, row: u32) {
        if self.heap.len() < self.k {
            self.heap.push(Near { d, n: row });
        } else if let Some(top) = self.heap.peek()
            && (Near { d, n: row }) < *top
        {
            self.heap.pop();
            self.heap.push(Near { d, n: row });
        }
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> &[u8] {
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        a
    }
    fn u8(&mut self) -> u8 {
        self.take(1)[0]
    }
    fn u16(&mut self) -> u16 {
        u16::from_le_bytes(self.take(2).try_into().unwrap())
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take(4).try_into().unwrap())
    }
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take(8).try_into().unwrap())
    }
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

/// Pages per part of a directory: a fold writes the parts whose pages
/// it changed and the directory's list of parts — not every page's ids
/// (path copying, as a persistent B-tree's).
const PART: usize = 256;

/// A graph's pages: each page's vector page and adjacency page (0: none
/// yet), the parts that list them, and the decoded chunks as they are
/// first read.
struct Dir {
    pages: Vec<(u64, u64)>,
    parts: Vec<u64>,
    /// Each chunk's vectors and lists, decoded on first use.
    vecs: Vec<OnceLock<Arc<VecPage>>>,
    adjs: Vec<OnceLock<Arc<AdjPage>>>,
}

impl Dir {
    /// `per`: pages per chunk.
    fn new(pages: Vec<(u64, u64)>, parts: Vec<u64>, per: usize) -> Self {
        let n = pages.len().div_ceil(per);
        Dir {
            pages,
            parts,
            vecs: (0..n).map(|_| OnceLock::new()).collect(),
            adjs: (0..n).map(|_| OnceLock::new()).collect(),
        }
    }

    /// A directory: its pages' count and its parts.
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

    fn decode_part(b: &[u8]) -> Vec<(u64, u64)> {
        let mut r = Reader(b);
        let n = r.u32() as usize;
        (0..n).map(|_| (r.u64(), r.u64())).collect()
    }

    /// Directory `id`, its parts read through `cache`.
    fn load(txn: &dyn KvRead, id: u64, per: usize, cache: &Objects<Vec<(u64, u64)>>) -> Self {
        let b = read_obj(txn, id);
        let mut r = Reader(&b);
        let n = r.u32() as usize;
        let parts: Vec<u64> = (0..r.u32()).map(|_| r.u64()).collect();
        let mut pages = Vec::with_capacity(n);
        for &p in &parts {
            pages.extend_from_slice(&cache.get(txn, p, Dir::decode_part));
        }
        Dir::new(pages, parts, per)
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

/// A page's nodes' rows and vectors.
/// The vectors start on a cache line (128 bytes on Apple's cores, 64 on
/// most others): a 512-byte vector then spans 4 lines, not 5.
struct VecPage {
    rows: Vec<u32>,
    buf: Vec<u8>,
    off: usize,
    len: usize,
}

/// The alignment of a page's vectors.
const LINE: usize = 128;

impl VecPage {
    fn encode(rows: &[u32], data: &[u8]) -> Vec<u8> {
        let mut b = Vec::with_capacity(4 + 4 * rows.len() + data.len());
        b.extend_from_slice(&(rows.len() as u32).to_le_bytes());
        for r in rows {
            b.extend_from_slice(&r.to_le_bytes());
        }
        b.extend_from_slice(data);
        b
    }

    /// A page's rows and vectors, encoded.
    fn parts(b: &[u8]) -> (Vec<u32>, &[u8]) {
        let mut r = Reader(b);
        let n = r.u32() as usize;
        ((0..n).map(|_| r.u32()).collect(), r.0)
    }

    /// Pages' rows and vectors as one, the vectors on a line.
    fn join(rows: Vec<u32>, data: &[&[u8]]) -> Self {
        let len = data.iter().map(|d| d.len()).sum();
        let mut buf = vec![0u8; len + LINE];
        let off = buf.as_ptr().align_offset(LINE).min(LINE);
        let mut at = off;
        for d in data {
            buf[at..at + d.len()].copy_from_slice(d);
            at += d.len();
        }
        VecPage {
            rows,
            buf,
            off,
            len,
        }
    }

    #[inline]
    fn data(&self) -> &[u8] {
        &self.buf[self.off..self.off + self.len]
    }
}

/// A page's nodes' out-edges, decoded flat (compressed sparse rows): node
/// `i`'s lists are `layers[at[i]..at[i + 1]]`, each list `edges[layers[j]
/// .. layers[j + 1]]` — a list one read away, not three.
struct AdjPage {
    at: Vec<u32>,
    layers: Vec<u32>,
    edges: Vec<Node>,
}

impl AdjPage {
    fn encode(lists: &[Vec<Vec<Node>>]) -> Vec<u8> {
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

    /// Pages' lists as one: each page's `nodes` (`None`: a page with no
    /// lists yet), in order.
    fn join<'b>(pages: impl Iterator<Item = (Option<&'b [u8]>, usize)>) -> Self {
        let (mut at, mut layers, mut edges) = (Vec::new(), Vec::new(), Vec::new());
        for (b, nodes) in pages {
            let mut r = Reader(b.unwrap_or(&[0, 0, 0, 0]));
            let n = r.u32() as usize;
            for i in 0..nodes {
                at.push(layers.len() as u32);
                if i < n {
                    for _ in 0..r.u8() {
                        layers.push(edges.len() as u32);
                        let len = r.u16() as usize;
                        edges.extend((0..len).map(|_| r.u32()));
                    }
                }
            }
        }
        at.push(layers.len() as u32);
        layers.push(edges.len() as u32);
        AdjPage { at, layers, edges }
    }
}

/// A node's list on a layer, as one key.
fn slot(n: Node, layer: usize) -> u64 {
    (u64::from(n) << 8) | layer as u64
}

type Lists = HashMap<u64, Vec<Node>, MixState>;

/// A fold's changes to edge lists: a list replaced (a new node's, one
/// pruned) or edges appended to it.
struct Patch {
    entries: Vec<(u64, bool, Vec<Node>)>,
}

impl Patch {
    /// `changed`: each list as it now is, and how long it was before the
    /// fold if only appended to (`None`: replaced).
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

    fn decode(b: &[u8]) -> Self {
        let mut r = Reader(b);
        let n = r.u32() as usize;
        let entries = (0..n)
            .map(|_| {
                let k = r.u64();
                let append = r.u8() == 1;
                let len = r.u16() as usize;
                (k, append, (0..len).map(|_| r.u32()).collect())
            })
            .collect();
        Patch { entries }
    }
}

/// A patch resolved in memory: the lists it changed as they are after it
/// (appends applied), over the patches before it; which nodes it has
/// lists for (a bit each, so a hop past a node no patch touched does not
/// look in the tables).
struct Level {
    prev: Option<Arc<Level>>,
    lists: Lists,
    dirty: Vec<u64>,
}

impl Level {
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

/// Each changed list as it now is, and its length before the fold if it
/// was only appended to (`None`: replaced).
type Changed = HashMap<u64, (Vec<Node>, Option<usize>), MixState>;

/// Decoded objects of one kind, by id.
struct Objects<T>(Mutex<HashMap<u64, Arc<T>, MixState>>);

impl<T> Objects<T> {
    fn new() -> Self {
        Objects(Mutex::new(HashMap::default()))
    }

    fn get(&self, txn: &dyn KvRead, id: u64, decode: impl FnOnce(&[u8]) -> T) -> Arc<T> {
        self.get_with(id, || decode(&read_obj(txn, id)))
    }

    /// The object keyed `key`, built by `build` if not cached.
    fn get_with(&self, key: u64, build: impl FnOnce() -> T) -> Arc<T> {
        if let Some(o) = self.0.lock().unwrap().get(&key) {
            return o.clone();
        }
        let o = Arc::new(build());
        self.0.lock().unwrap().insert(key, o.clone());
        o
    }
}

fn read_obj(txn: &dyn KvRead, id: u64) -> Vec<u8> {
    txn.get(&obj_key(id))
        .expect("object read")
        .unwrap_or_else(|| panic!("object {id:x} missing"))
}

/// A chunk's cache key: its pages' ids, hashed.
fn chunk_key(ids: impl Iterator<Item = u64>) -> u64 {
    ids.fold(0x9e37_79b9_7f4a_7c15u64, |h, id| mix(h ^ id))
}

fn obj_key(id: u64) -> Vec<u8> {
    let mut k = OBJ.to_vec();
    k.extend_from_slice(&id.to_be_bytes());
    k
}

/// A delta key: the row's, in `epoch` (`None`: no epochs).
fn delta_key(epoch: Option<u32>, row: u32) -> Vec<u8> {
    let mut k = DELTA.to_vec();
    if let Some(e) = epoch {
        k.extend_from_slice(&e.to_be_bytes());
    }
    k.extend_from_slice(&row.to_be_bytes());
    k
}

/// The delta's keys from `epoch` on (or all of them).
fn delta_from(epoch: Option<u32>) -> Vec<u8> {
    let mut k = DELTA.to_vec();
    if let Some(e) = epoch {
        k.extend_from_slice(&e.to_be_bytes());
    }
    k
}

/// The delta's rows and vectors in `[start, end)`.
/// The delta's keys a fold folded: with epochs those after `was` up to
/// `closed`, without them all.
fn delta_scan_keys(txn: &dyn KvTxn, epochs: bool, was: u32, closed: u32) -> Vec<Vec<u8>> {
    let (start, end) = if epochs {
        (delta_from(Some(was + 1)), delta_from(Some(closed + 1)))
    } else {
        (delta_from(None), prefix_end(DELTA).unwrap())
    };
    txn.scan(Bound::Included(&start), Bound::Excluded(&end))
        .expect("delta scan")
        .map(|kv| kv.expect("delta read").0)
        .collect()
}

/// The dead keys of epochs `was + 1 ..= closed`: each key, its row and
/// the hash of the vector the row had.
fn dead_scan(txn: &dyn KvRead, was: u32, closed: u32) -> Vec<(Vec<u8>, u32, u64)> {
    let at = |e: u32| {
        let mut k = DEAD.to_vec();
        k.extend_from_slice(&e.to_be_bytes());
        k
    };
    let (start, end) = (at(was + 1), at(closed + 1));
    txn.scan(Bound::Included(&start), Bound::Excluded(&end))
        .expect("dead scan")
        .map(|kv| {
            let k = kv.expect("dead read").0;
            let n = k.len();
            let row = u32::from_be_bytes(k[n - 12..n - 8].try_into().unwrap());
            let hash = u64::from_be_bytes(k[n - 8..].try_into().unwrap());
            (k, row, hash)
        })
        .collect()
}

fn delta_scan(txn: &dyn KvTxn, start: &[u8], end: &[u8]) -> Vec<(u32, Vec<u8>)> {
    txn.scan(Bound::Included(start), Bound::Excluded(end))
        .expect("delta scan")
        .map(|kv| {
            let (k, v) = kv.expect("delta read");
            (u32::from_be_bytes(k[k.len() - 4..].try_into().unwrap()), v)
        })
        .collect()
}

/// The graphs on a KV.
pub struct Paged {
    kv: Arc<dyn Kv>,
    cfg: Config,
    dirs: Objects<Dir>,
    parts: Objects<Vec<(u64, u64)>>,
    vecs: Objects<VecPage>,
    adjs: Objects<AdjPage>,
    patches: Objects<Patch>,
    levels: Mutex<HashMap<u64, Arc<Level>, MixState>>,
    sealed: Mutex<HashMap<u32, Arc<Sealed>, MixState>>,
    written: AtomicU64,
    unfolded: AtomicU64,
    /// Rows written into the open epoch (about: counted, not read).
    open: AtomicU64,
    seals: AtomicU64,
    meter: Meter,
    folds: AtomicU64,
    compactions: AtomicU64,
    graphs: AtomicU64,
    chain: AtomicU64,
    deads: Objects<Dead>,
    /// (C, with `table`): each row's nodes, for the nodes up to the first
    /// (a node's row never changes; nodes are only added).
    by_row: Mutex<(u32, ByRow)>,
    /// Rows a search read from the table, and the searches widened.
    reads: AtomicU64,
    widened: AtomicU64,
    marked: AtomicU64,
    consolidations: AtomicU64,
}

/// A graph as a snapshot sees it.
struct View<'a> {
    store: &'a Paged,
    txn: &'a dyn KvRead,
    root: GraphRoot,
    dir: Arc<Dir>,
    level: Option<Arc<Level>>,
    dead: Option<Arc<Dead>>,
}

impl View<'_> {
    fn open<'a>(store: &'a Paged, txn: &'a dyn KvRead, root: GraphRoot) -> View<'a> {
        let dir = if root.dir == 0 {
            Arc::new(Dir::new(Vec::new(), Vec::new(), 1))
        } else {
            store.dirs.get_with(root.dir, || {
                Dir::load(
                    txn,
                    root.dir,
                    store.cfg.chunk / store.cfg.page,
                    &store.parts,
                )
            })
        };
        let dead = (root.dead != 0).then(|| store.deads.get(txn, root.dead, Dead::decode));
        let mut view = View {
            store,
            txn,
            root,
            dir,
            level: None,
            dead,
        };
        view.level = view.resolve();
        view
    }

    /// The graph's patches resolved, each level cached by the patches up
    /// to it (the newest built on the one before).
    fn resolve(&self) -> Option<Arc<Level>> {
        let chain = &self.root.patches;
        let keys: Vec<u64> = chain
            .iter()
            .scan(0x9e37_79b9_7f4a_7c15u64, |h, id| {
                *h = mix(*h ^ id);
                Some(*h)
            })
            .collect();
        let mut level = None;
        let mut from = 0;
        {
            let cache = self.store.levels.lock().unwrap();
            for i in (0..keys.len()).rev() {
                if let Some(l) = cache.get(&keys[i]) {
                    level = Some(l.clone());
                    from = i + 1;
                    break;
                }
            }
        }
        for i in from..chain.len() {
            let patch = self.store.patches.get(self.txn, chain[i], Patch::decode);
            let mut lists =
                Lists::with_capacity_and_hasher(patch.entries.len(), MixState::default());
            let mut dirty = vec![0u64; (self.root.nodes as usize).div_ceil(64)];
            for (k, append, edges) in &patch.entries {
                let (n, layer) = ((k >> 8) as Node, (k & 0xff) as usize);
                let list = if *append {
                    let mut l = match level.as_deref().and_then(|l: &Level| l.get(n, layer)) {
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
            let l = Arc::new(Level {
                prev: level,
                lists,
                dirty,
            });
            self.store.levels.lock().unwrap().insert(keys[i], l.clone());
            level = Some(l);
        }
        level
    }

    /// Chunk `c`'s pages.
    fn chunk_pages(&self, c: usize) -> &[(u64, u64)] {
        let per = self.store.cfg.chunk / self.store.cfg.page;
        &self.dir.pages[c * per..((c + 1) * per).min(self.dir.pages.len())]
    }

    /// Chunk `c`'s rows and vectors.
    #[inline]
    fn vec_chunk(&self, c: usize) -> &VecPage {
        self.dir.vecs[c].get_or_init(|| {
            let pages = self.chunk_pages(c);
            self.store
                .vecs
                .get_with(chunk_key(pages.iter().map(|p| p.0)), || {
                    let bytes: Vec<Vec<u8>> =
                        pages.iter().map(|p| read_obj(self.txn, p.0)).collect();
                    let mut rows = Vec::new();
                    let mut data = Vec::new();
                    for b in &bytes {
                        let (r, d) = VecPage::parts(b);
                        rows.extend(r);
                        data.push(d);
                    }
                    VecPage::join(rows, &data)
                })
        })
    }

    #[inline]
    fn at(&self, n: Node) -> &[u8] {
        let (chunk, w) = (self.store.cfg.chunk, self.store.cfg.width);
        let i = n as usize % chunk * w;
        &self.vec_chunk(n as usize / chunk).data()[i..i + w]
    }

    fn row(&self, n: Node) -> u32 {
        let chunk = self.store.cfg.chunk;
        self.vec_chunk(n as usize / chunk).rows[n as usize % chunk]
    }

    fn page_list(&self, n: Node, layer: usize) -> &[Node] {
        let chunk = self.store.cfg.chunk;
        let c = n as usize / chunk;
        let Some(cell) = self.dir.adjs.get(c) else {
            return &[];
        };
        let adj = cell.get_or_init(|| {
            let pages = self.chunk_pages(c);
            self.store
                .adjs
                .get_with(chunk_key(pages.iter().map(|p| p.1)), || {
                    let bytes: Vec<Option<Vec<u8>>> = pages
                        .iter()
                        .map(|p| (p.1 != 0).then(|| read_obj(self.txn, p.1)))
                        .collect();
                    AdjPage::join(bytes.iter().map(|b| (b.as_deref(), self.store.cfg.page)))
                })
        });
        adj.list(n as usize % chunk, layer)
    }
}

impl Graph for View<'_> {
    #[inline]
    fn distance(&self, q: &[u8], n: Node) -> f32 {
        self.store.cfg.params.metric.distance(q, self.at(n))
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
    rows: Vec<u32>,
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
        self.view.store.cfg.params.metric.distance(q, self.at(n))
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
        let (l, _) = self.changed.get_mut(&k).unwrap();
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

/// Where a fold's rows go.
#[derive(Clone, Copy)]
enum Target {
    /// Into graph `i`.
    Into(usize),
    /// Into a new graph.
    New,
}

/// A fold's writes: objects put, objects no longer referenced deleted.
struct Writes {
    put: Vec<(Vec<u8>, Vec<u8>)>,
    dropped: BTreeSet<u64>,
}

impl Writes {
    fn object(&mut self, kind: u64, bytes: Vec<u8>) -> u64 {
        let id = object_id(kind, &bytes);
        self.put.push((obj_key(id), bytes));
        id
    }
}

impl Paged {
    /// The model on `kv`, empty.
    pub fn new(kv: Arc<dyn Kv>, cfg: Config) -> Self {
        Paged {
            kv,
            cfg,
            dirs: Objects::new(),
            parts: Objects::new(),
            vecs: Objects::new(),
            adjs: Objects::new(),
            patches: Objects::new(),
            levels: Mutex::new(HashMap::default()),
            sealed: Mutex::new(HashMap::default()),
            written: AtomicU64::new(0),
            unfolded: AtomicU64::new(0),
            open: AtomicU64::new(0),
            seals: AtomicU64::new(0),
            meter: Meter::default(),
            folds: AtomicU64::new(0),
            compactions: AtomicU64::new(0),
            graphs: AtomicU64::new(0),
            chain: AtomicU64::new(0),
            deads: Objects::new(),
            by_row: Mutex::new((0, HashMap::default())),
            reads: AtomicU64::new(0),
            widened: AtomicU64::new(0),
            marked: AtomicU64::new(0),
            consolidations: AtomicU64::new(0),
        }
    }

    /// Inserts `rows` into `view`'s graph; its new root, with its objects
    /// written to `w` (and those it replaced dropped).  `compact`: write
    /// every changed list into its page (no patch).
    /// `dead`: the graph's dead nodes now (`None`: as they were).
    fn fold_into(
        &self,
        view: &View,
        rows: &[(u32, Vec<u8>)],
        compact: bool,
        dead: Option<Vec<Node>>,
        w: &mut Writes,
    ) -> GraphRoot {
        let cfg = self.cfg;
        let base = view.root.nodes;
        let mut f = Fold {
            view,
            width: cfg.width,
            base,
            rows: Vec::with_capacity(rows.len()),
            vectors: Vec::with_capacity(rows.len() * cfg.width),
            changed: Changed::default(),
            entry: view.root.entry,
        };
        let mut s = Scratch::default();
        for (row, v) in rows {
            f.rows.push(*row);
            f.vectors.extend_from_slice(v);
        }
        if cfg.batch.ratio > 0.0 {
            let nodes: Vec<(Node, &[u8])> = rows
                .iter()
                .enumerate()
                .map(|(i, (_, v))| (base + i as Node, v.as_slice()))
                .collect();
            hnsw::insert_batch(&mut f, &cfg.params, &nodes, &cfg.batch, &mut s);
        } else {
            for (i, (_, v)) in rows.iter().enumerate() {
                hnsw::insert(&mut f, &cfg.params, base + i as Node, v, &mut s);
            }
        }
        let nodes = base + rows.len() as u32;

        // The dead nodes cut out once they are a share of the graph (or a
        // compaction is due anyway): each list through one rewritten, the
        // entry point moved off one; the graph then has none.
        let dead =
            dead.unwrap_or_else(|| view.dead.as_ref().map_or(Vec::new(), |d| d.nodes.clone()));
        let due = compact
            || view.root.patch_bytes as f64
                > cfg.compact * nodes as f64 * (cfg.params.m * 2 * 4) as f64;
        let consolidate =
            !dead.is_empty() && (due || dead.len() as f64 > CONSOLIDATE * f64::from(nodes));
        let dead = if consolidate {
            self.consolidations.fetch_add(1, Relaxed);
            let set = Dead::new(dead);
            let is = |x: Node| set.has(x);
            let p = &cfg.params;
            let all: Vec<Node> = (0..nodes).collect();
            let fixes = hnsw::par_map(&all, cfg.batch.threads, &mut s, |&n, s| {
                let mut out = Vec::new();
                let mut l = Vec::new();
                for layer in 0..=p.level(n) {
                    if set.has(n) {
                        out.push((layer, Vec::new()));
                        continue;
                    }
                    f.neighbors(n, layer, &mut l);
                    if let Some(new) = hnsw::consolidate(&f, p, n, layer, &l, &is, s) {
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
        let compact = compact || consolidate;
        let (changed, entry, new_rows, new_vectors) = (f.changed, f.entry, f.rows, f.vectors);
        let mut pages = view.dir.pages.clone();
        pages.resize((nodes as usize).div_ceil(cfg.page), (0, 0));

        // Vector pages: the last partial one rewritten, new ones after.
        for (p, page) in pages.iter_mut().enumerate().skip(base as usize / cfg.page) {
            let lo = (p * cfg.page).max(base as usize);
            let hi = ((p + 1) * cfg.page).min(nodes as usize);
            let (mut r, mut d) = if lo > p * cfg.page {
                let (old, at) = (
                    view.vec_chunk(p * cfg.page / cfg.chunk),
                    p * cfg.page % cfg.chunk,
                );
                let len = base as usize - p * cfg.page;
                (
                    old.rows[at..at + len].to_vec(),
                    old.data()[at * cfg.width..(at + len) * cfg.width].to_vec(),
                )
            } else {
                (Vec::new(), Vec::new())
            };
            let (a, b) = (lo - base as usize, hi - base as usize);
            r.extend_from_slice(&new_rows[a..b]);
            d.extend_from_slice(&new_vectors[a * cfg.width..b * cfg.width]);
            let old = page.0;
            page.0 = w.object(1, VecPage::encode(&r, &d));
            if old != 0 && old != page.0 {
                w.dropped.insert(old);
            }
        }

        // A patch of the fold's changes, or — once the patches outgrow
        // `compact` of the adjacency (or for a segment) — the pages they
        // touch rewritten.
        let patch = Patch::encode(&changed);
        let adjacency = nodes as f64 * (cfg.params.m * 2 * 4) as f64;
        let bytes = view.root.patch_bytes + patch.len() as u64;
        let (patches, patch_bytes) = if compact || bytes as f64 > cfg.compact * adjacency {
            self.compactions.fetch_add(1, Relaxed);
            let mut touched: BTreeSet<usize> = changed
                .keys()
                .map(|k| (k >> 8) as usize / cfg.page)
                .collect();
            let mut at = view.level.as_deref();
            while let Some(l) = at {
                touched.extend(l.lists.keys().map(|k| (k >> 8) as usize / cfg.page));
                at = l.prev.as_deref();
            }
            for p in touched {
                let lo = p * cfg.page;
                let hi = ((p + 1) * cfg.page).min(nodes as usize);
                let lists: Vec<Vec<Vec<Node>>> = (lo..hi)
                    .map(|n| {
                        let mut node = Vec::new();
                        for layer in 0..16 {
                            let l = match changed.get(&slot(n as Node, layer)) {
                                Some((l, _)) => l.clone(),
                                None if (n as u32) < base => {
                                    let mut l = Vec::new();
                                    view.neighbors(n as Node, layer, &mut l);
                                    l
                                }
                                None => Vec::new(),
                            };
                            node.push(l);
                        }
                        while node.last().is_some_and(Vec::is_empty) {
                            node.pop();
                        }
                        node
                    })
                    .collect();
                let old = pages[p].1;
                pages[p].1 = w.object(2, AdjPage::encode(&lists));
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
            chain.push(w.object(3, patch));
            (chain, bytes)
        };
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
        GraphRoot {
            dir,
            patches,
            patch_bytes,
            nodes,
            entry,
            dead,
        }
    }

    /// One fold in a transaction into `target`: `bulk`'s rows (a new index's), or
    /// without them the delta's — with `epochs` those of the epochs
    /// closed (up to `closed`) — deleted from it; the rows folded.
    fn fold(&self, bulk: Option<&[(u32, Vec<u8>)]>, closed: u32, target: Target) -> usize {
        let mut folded = 0;
        let mut written = 0;
        let mut count = 0;
        let epochs = self.cfg.epochs;
        let t = std::time::Instant::now();
        transact(self.kv.as_ref(), TxnOptions::default(), |txn| {
            written = 0;
            let was = read_root(txn.shared().expect("shared reads")).folded;
            let delta = match bulk {
                Some(rows) => rows.to_vec(),
                None if epochs => delta_scan(
                    &*txn,
                    &delta_from(Some(was + 1)),
                    &delta_from(Some(closed + 1)),
                ),
                None => delta_scan(&*txn, &delta_from(None), &prefix_end(DELTA).unwrap()),
            };
            folded = delta.len();
            // With the table: only rows as it still has them; the rows
            // deleted or updated since, to be marked dead.
            let table = self.cfg.table && epochs && bulk.is_none();
            let (delta, gone) = if table {
                let r = txn.shared().expect("shared reads");
                let delta: Vec<(u32, Vec<u8>)> = delta
                    .into_iter()
                    .filter(|(row, v)| {
                        r.get(&table_key(*row)).expect("table read").as_deref()
                            == Some(v.as_slice())
                    })
                    .collect();
                (delta, dead_scan(r, was, closed))
            } else {
                (delta, Vec::new())
            };
            folded += gone.len();
            if delta.is_empty() && gone.is_empty() && !(epochs && bulk.is_none() && closed > was) {
                return Ok(());
            }
            let mut w = Writes {
                put: Vec::new(),
                dropped: BTreeSet::new(),
            };
            let keep = {
                let r = txn.shared().expect("the KV shares a transaction's reads");
                let mut graphs = read_root(r).graphs;
                let target = match target {
                    Target::Into(i) if i >= graphs.len() => Target::New,
                    t => t,
                };
                let all = delta;
                // The nodes of the rows gone marked dead.
                let marks = match target {
                    Target::Into(i) if !gone.is_empty() => {
                        let v = View::open(self, r, graphs[i].clone());
                        Some(self.mark(&v, r, &gone))
                    }
                    _ => None,
                };
                let root = if all.is_empty() && marks.is_none() {
                    None
                } else {
                    Some(match target {
                        Target::Into(i) => {
                            let v = View::open(self, r, graphs[i].clone());
                            self.fold_into(&v, &all, false, marks, &mut w)
                        }
                        Target::New => {
                            let v = View::open(self, r, GraphRoot::default());
                            self.fold_into(&v, &all, true, None, &mut w)
                        }
                    })
                };
                match (target, root) {
                    (Target::Into(i), Some(root)) => graphs[i] = root,
                    (Target::New, Some(root)) => graphs.push(root),
                    (_, None) => {}
                }
                graphs
            };
            for (k, v) in &w.put {
                txn.put(k, v)?;
                written += (k.len() + v.len()) as u64;
            }
            let live: BTreeSet<u64> = w
                .put
                .iter()
                .map(|(k, _)| u64::from_be_bytes(k[OBJ.len()..].try_into().unwrap()))
                .collect();
            for id in w.dropped.difference(&live) {
                txn.delete(&obj_key(*id))?;
            }
            let now = if epochs && bulk.is_none() {
                closed.max(was)
            } else {
                was
            };
            let rb = encode_root(now, &keep);
            txn.put(ROOT, &rb)?;
            written += (ROOT.len() + rb.len()) as u64;
            if bulk.is_none() {
                // With epochs, the delta keys of the epochs folded are
                // deleted all the same (their space), read by no scan.
                let mut epoch_of = delta_scan_keys(&*txn, epochs, was, closed);
                for k in epoch_of.drain(..) {
                    txn.delete(&k)?;
                }
                for (k, _, _) in &gone {
                    txn.delete(k)?;
                }
            }
            count = keep.len();
            self.chain.store(
                keep.iter()
                    .map(|g| g.patches.len() as u64)
                    .max()
                    .unwrap_or(0),
                Relaxed,
            );
            Ok(())
        })
        .expect("fold");
        self.written.fetch_add(written, Relaxed);
        self.folds.fetch_add(1, Relaxed);
        self.graphs.store(count as u64, Relaxed);
        if std::env::var_os("ANN_TRACE").is_some() {
            eprintln!(
                "fold: {folded} rows, {written} bytes, {:.1} ms",
                t.elapsed().as_secs_f64() * 1e3
            );
        }
        if self.cfg.epochs {
            // A snapshot from before the fold rebuilds what it still reads.
            self.sealed.lock().unwrap().retain(|e, _| *e > closed);
        }
        folded
    }

    /// Closes the open epoch: writers that read it and commit later retry
    /// in the next.  The epoch closed.
    fn close(&self) -> u32 {
        let t = std::time::Instant::now();
        let mut e = 0;
        transact(self.kv.as_ref(), TxnOptions::default(), |txn| {
            e = txn
                .get(EPOCH)?
                .map_or(1, |b| u32::from_le_bytes(b[..4].try_into().unwrap()));
            txn.put(EPOCH, &(e + 1).to_le_bytes())
        })
        .expect("close");
        self.open.store(0, Relaxed);
        self.meter.closed(t.elapsed().as_nanos() as f64);
        e
    }

    /// Closed epoch `e`'s rows, read once.
    fn sealed(&self, txn: &dyn KvRead, e: u32) -> Arc<Sealed> {
        if let Some(s) = self.sealed.lock().unwrap().get(&e) {
            return s.clone();
        }
        let (start, end) = (delta_from(Some(e)), delta_from(Some(e + 1)));
        let (mut rows, mut data) = (Vec::new(), Vec::new());
        for kv in txn
            .scan(Bound::Included(&start), Bound::Excluded(&end))
            .expect("delta scan")
        {
            let (key, v) = kv.expect("delta read");
            rows.push(u32::from_be_bytes(key[key.len() - 4..].try_into().unwrap()));
            data.extend_from_slice(&v);
        }
        let s = Arc::new(Sealed { rows, data });
        self.sealed.lock().unwrap().insert(e, s.clone());
        s
    }

    /// A row written in a transaction of its own: `new` into the delta
    /// (and the table), the vector it had (`old`) to be marked dead — or
    /// with no `new`, the row deleted from the table.  The retries.
    fn write(&self, row: u32, old: Option<&[u8]>, new: Option<&[u8]>) -> u32 {
        let t = std::time::Instant::now();
        let mut attempts = 0;
        let mut len = 0;
        let (epochs, table) = (self.cfg.epochs, self.cfg.table);
        transact(self.kv.as_ref(), TxnOptions::default(), |txn| {
            attempts += 1;
            len = 0;
            let epoch = if epochs {
                Some(
                    txn.get_validated(EPOCH)?
                        .map_or(1, |b| u32::from_le_bytes(b[..4].try_into().unwrap())),
                )
            } else {
                None
            };
            if let Some(v) = new {
                let k = delta_key(epoch, row);
                len += k.len() + v.len();
                txn.put(&k, v)?;
                if table {
                    let k = table_key(row);
                    len += k.len() + v.len();
                    txn.put(&k, v)?;
                }
            } else if table {
                let k = table_key(row);
                len += k.len();
                txn.delete(&k)?;
            }
            if let Some(o) = old {
                let mut k = DEAD.to_vec();
                k.extend_from_slice(&epoch.unwrap_or(0).to_be_bytes());
                k.extend_from_slice(&row.to_be_bytes());
                k.extend_from_slice(&vector_hash(o).to_be_bytes());
                len += k.len();
                txn.put(&k, &[])?;
            }
            Ok(())
        })
        .expect("write");
        let n = u64::from(new.is_some()) + u64::from(old.is_some());
        self.written.fetch_add(len as u64, Relaxed);
        self.unfolded.fetch_add(n, Relaxed);
        self.open.fetch_add(n, Relaxed);
        self.meter.inserts.0.fetch_add(1, Relaxed);
        self.meter
            .inserts
            .1
            .fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
        self.meter.retries.fetch_add(attempts as u64 - 1, Relaxed);
        attempts - 1
    }

    /// `view`'s dead nodes with those of the rows `gone` (each row's node
    /// with the vector it had, unless the table has the row with that
    /// vector again), sorted.
    fn mark(&self, view: &View, r: &dyn KvRead, gone: &[(Vec<u8>, u32, u64)]) -> Vec<Node> {
        let mut by = self.by_row.lock().unwrap();
        let (covered, map) = &mut *by;
        for n in *covered..view.root.nodes {
            map.entry(view.row(n)).or_default().push(n);
        }
        *covered = (*covered).max(view.root.nodes);
        let mut dead: Vec<Node> = view.dead.as_ref().map_or(Vec::new(), |d| d.nodes.clone());
        let before = dead.len();
        for &(_, row, hash) in gone {
            let Some(nodes) = map.get(&row) else { continue };
            let now = r.get(&table_key(row)).expect("table read");
            for &n in nodes {
                let v = view.at(n);
                if n < view.root.nodes && vector_hash(v) == hash && now.as_deref() != Some(v) {
                    dead.push(n);
                }
            }
        }
        dead.sort_unstable();
        dead.dedup();
        self.marked.fetch_add((dead.len() - before) as u64, Relaxed);
        dead
    }
}

impl Model for Paged {
    fn name(&self) -> String {
        format!(
            "C (pages of {} in chunks of {}, compact at {}, sealed every {})",
            self.cfg.page, self.cfg.chunk, self.cfg.compact, self.cfg.seal
        )
    }

    fn bulk(&self, rows: &[(u32, &[u8])]) {
        if self.cfg.table {
            // The table, as it was before the index.
            for part in rows.chunks(10_000) {
                let mut len = 0;
                transact(self.kv.as_ref(), TxnOptions::default(), |txn| {
                    len = 0;
                    for (r, v) in part {
                        let k = table_key(*r);
                        len += k.len() + v.len();
                        txn.put(&k, v)?;
                    }
                    Ok(())
                })
                .expect("table");
                self.written.fetch_add(len as u64, Relaxed);
            }
        }
        let owned: Vec<(u32, Vec<u8>)> = rows.iter().map(|(r, v)| (*r, v.to_vec())).collect();
        self.fold(Some(&owned), 0, Target::New);
    }

    fn insert(&self, row: u32, v: &[u8]) -> u32 {
        self.write(row, None, Some(v))
    }

    fn change(&self, row: u32, old: &[u8], new: Option<&[u8]>) -> u32 {
        assert!(
            self.cfg.table && self.cfg.epochs,
            "deletes need table=1 and epochs=1"
        );
        self.write(row, Some(old), new)
    }

    fn maintain(&self, threshold: usize) -> bool {
        let (threshold, seal) = if self.cfg.auto && self.cfg.epochs && threshold > 1 {
            self.meter.sample();
            self.meter.sizes(
                threshold,
                if self.cfg.seal > 0 {
                    self.cfg.seal
                } else {
                    threshold
                },
            )
        } else {
            (threshold, self.cfg.seal)
        };
        let target = Target::Into(0);
        let (unfolded, open) = (
            self.unfolded.load(Relaxed) as usize,
            self.open.load(Relaxed) as usize,
        );
        let closed = if self.cfg.epochs && seal > 0 && threshold > 1 {
            // Seals close epochs; a fold takes the closed ones (closing
            // one costs the writes it sends back, a fold need not).
            if open >= seal {
                self.close();
                self.seals.fetch_add(1, Relaxed);
            }
            let sealed = self
                .unfolded
                .load(Relaxed)
                .saturating_sub(self.open.load(Relaxed)) as usize;
            if sealed < threshold {
                return false;
            }
            let txn = self.kv.begin(TxnOptions::read_only()).expect("begin");
            read_epoch(txn.shared().expect("shared reads")) - 1
        } else {
            if unfolded < threshold.max(1) {
                if self.cfg.epochs && seal > 0 && open >= seal {
                    self.close();
                    self.seals.fetch_add(1, Relaxed);
                }
                return false;
            }
            if self.cfg.epochs { self.close() } else { 0 }
        };
        let (t, compactions) = (std::time::Instant::now(), self.compactions.load(Relaxed));
        let n = self.fold(None, closed, target);
        // A compaction's cost is the patches' (`compact` amortizes it),
        // not the fold's.
        if self.compactions.load(Relaxed) == compactions {
            self.meter.folded(n, t.elapsed().as_nanos() as f64);
        }
        self.unfolded.fetch_sub(n as u64, Relaxed);
        n > 0
    }

    fn search(&self, q: &[u8], k: usize, ef: usize, s: &mut Scratch) -> Vec<Hit> {
        let txn = self.kv.begin(TxnOptions::read_only()).expect("begin");
        let r = txn.shared().expect("shared reads");
        let root = read_root(r);
        if !self.cfg.table {
            return self.gather(r, &root, q, k, ef, s);
        }
        // What was found, checked against the table in the snapshot: a
        // row gone, or whose vector is no longer the one found (its
        // distance differs), passed over; asked again with `ef` doubled
        // while fewer than `k` are left.
        let metric = self.cfg.params.metric;
        let mut ef = ef.max(k);
        loop {
            let found = self.gather(r, &root, q, ef, ef, s);
            let mut hits: Vec<Hit> = Vec::with_capacity(k);
            for h in &found {
                if hits.len() == k {
                    break;
                }
                if hits.iter().any(|x| x.row == h.row) {
                    continue;
                }
                self.reads.fetch_add(1, Relaxed);
                if let Some(v) = r.get(&table_key(h.row)).expect("table read")
                    && metric.distance(q, &v).to_bits() == h.d.to_bits()
                {
                    hits.push(*h);
                }
            }
            if hits.len() == k || found.len() < ef || ef >= 4096 {
                return hits;
            }
            self.widened.fetch_add(1, Relaxed);
            ef *= 2;
        }
    }

    fn written(&self) -> u64 {
        self.written.load(Relaxed)
    }

    fn report(&self) -> String {
        let deletes = if self.cfg.table {
            format!(
                ", table reads {}, searches widened {}, nodes marked dead {}, consolidations {}",
                self.reads.load(Relaxed),
                self.widened.load(Relaxed),
                self.marked.load(Relaxed),
                self.consolidations.load(Relaxed)
            )
        } else {
            String::new()
        };
        format!(
            "graphs {}, patches {}, folds {}, seals {}, compactions {}, unfolded {}{deletes}{}",
            self.graphs.load(Relaxed),
            self.chain.load(Relaxed),
            self.folds.load(Relaxed),
            self.seals.load(Relaxed),
            self.compactions.load(Relaxed),
            self.unfolded.load(Relaxed),
            if self.cfg.auto {
                self.meter.report()
            } else {
                String::new()
            }
        )
    }
}

impl Paged {
    /// The `k` rows nearest `q` in the graphs (their dead nodes passed
    /// over) and the delta, nearest first, unchecked.
    fn gather(
        &self,
        r: &dyn KvRead,
        root: &Root,
        q: &[u8],
        k: usize,
        ef: usize,
        s: &mut Scratch,
    ) -> Vec<Hit> {
        let mut hits = Vec::new();
        for g in &root.graphs {
            let view = View::open(self, r, g.clone());
            let found = hnsw::search(
                &view,
                q,
                if view.dead.is_some() { ef.max(k) } else { k },
                ef,
                s,
            );
            hits.extend(
                found
                    .into_iter()
                    .filter(|x| view.dead.as_ref().is_none_or(|d| !d.has(x.n)))
                    .take(k)
                    .map(|Near { d, n }| Hit {
                        d,
                        row: view.row(n),
                    }),
            );
        }
        let (metric, w) = (self.cfg.params.metric, self.cfg.width);
        let mut near = Nearest::new(k);
        let meter = &self.meter;
        meter.searches.fetch_add(1, Relaxed);
        // The closed epochs from memory, the open one from the KV.
        let open = if self.cfg.epochs {
            let open = read_epoch(r);
            let (t, mut rows) = (std::time::Instant::now(), 0);
            for e in root.folded + 1..open {
                let s = self.sealed(r, e);
                rows += s.rows.len();
                for (row, v) in s.rows.iter().zip(s.data.chunks_exact(w)) {
                    near.push(metric.distance(q, v), *row);
                }
            }
            if rows > 0 {
                meter.mem.0.fetch_add(rows as u64, Relaxed);
                meter
                    .mem
                    .1
                    .fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            }
            Some(open)
        } else {
            None
        };
        let start = delta_from(open);
        let end = prefix_end(DELTA).unwrap();
        let (t, mut rows) = (std::time::Instant::now(), 0u64);
        for kv in r
            .scan(Bound::Included(&start), Bound::Excluded(&end))
            .expect("delta scan")
        {
            let (key, v) = kv.expect("delta read");
            let row = u32::from_be_bytes(key[key.len() - 4..].try_into().unwrap());
            near.push(metric.distance(q, &v), row);
            rows += 1;
        }
        if rows > 0 {
            meter.kv.0.fetch_add(rows, Relaxed);
            meter.kv.1.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
        }
        hits.extend(near.heap.into_iter().map(|Near { d, n }| Hit { d, row: n }));
        hits.sort_by(|a, b| a.d.total_cmp(&b.d).then(a.row.cmp(&b.row)));
        hits.truncate(k);
        hits
    }
}
