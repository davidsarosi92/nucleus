//! The HNSW core (Malkov and Yashunin 2018), over any storage of nodes and
//! edges ([`Graph`], [`GraphMut`]): the layered greedy search with `ef`,
//! inserts with the neighbour-selection heuristic (the paper's algorithm
//! 4, as hnswlib keeps it), and a node's level from a hash of its key — so
//! the same rows inserted in the same order build the same graph
//! (principle 4).  The storage models (PLAN.md §M14.12 V4) differ only
//! below these traits.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

use nucleus_kernels::{dot, l2_squared, mix};

/// A node: a row's position in the index (dense, from 0).
pub type Node = u32;

/// How two vectors' distance is taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metric {
    /// The squared Euclidean distance.
    L2,
    /// One minus the inner product, over vectors stored normalized: the
    /// cosine distance.
    Cosine,
}

impl Metric {
    /// The distance of two vectors stored as little-endian `f32`s.
    #[inline]
    pub fn distance(self, a: &[u8], b: &[u8]) -> f32 {
        match self {
            Metric::L2 => l2_squared(a, b) as f32,
            Metric::Cosine => (1.0 - dot(a, b)) as f32,
        }
    }
}

/// The graph's shape, as pgvector's `WITH (m, ef_construction)`.
#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// Out-edges chosen for a new node per layer (`M`); layer 0 keeps up to
    /// twice as many.
    pub m: usize,
    /// The candidates an insert looks at per layer.
    pub ef_construction: usize,
    /// The distance.
    pub metric: Metric,
    /// How far over its maximum a node's out-edges may grow by backlinks
    /// before they are pruned again (1: at once, hnswlib's; more: the
    /// storage rewrites a node's list less often).
    pub slack: usize,
}

impl Params {
    /// The most out-edges a node keeps on `layer`.
    pub fn m_max(&self, layer: usize) -> usize {
        if layer == 0 { 2 * self.m } else { self.m }
    }

    /// A node's top layer: `⌊−ln(u) / ln M⌋` with `u` from a hash of the
    /// node (the paper's level distribution, deterministic).
    pub fn level(&self, node: Node) -> usize {
        let h = mix(u64::from(node) ^ 0x6a09_e667_f3bc_c909);
        let u = ((h >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
        let l = -u.ln() / (self.m as f64).ln();
        (l.floor() as usize).min(15)
    }
}

/// What a search reads.
pub trait Graph {
    /// The distance from `q` to node `n`'s vector.
    fn distance(&self, q: &[u8], n: Node) -> f32;
    /// Node `n`'s vector, into `out` (replacing its contents).
    fn vector(&self, n: Node, out: &mut Vec<u8>);
    /// Node `n`'s out-edges on `layer`, into `out` (replacing its contents).
    fn neighbors(&self, n: Node, layer: usize, out: &mut Vec<Node>);
    /// The entry point and its top layer (`None`: the graph is empty).
    fn entry(&self) -> Option<(Node, usize)>;
}

/// What an insert writes as well.
pub trait GraphMut: Graph {
    /// Adds node `n`, its vector, its top layer and its out-edges on each
    /// layer up to it (`edges[layer]`).
    fn add_node(&mut self, n: Node, level: usize, vector: &[u8], edges: &[Vec<Node>]);
    /// Adds the edge `n → to` on `layer`; returns `n`'s out-degree there
    /// now.
    fn add_edge(&mut self, n: Node, layer: usize, to: Node) -> usize;
    /// Replaces `n`'s out-edges on `layer`.
    fn set_neighbors(&mut self, n: Node, layer: usize, edges: &[Node]);
    /// Makes `n` (whose top layer is `level`) the entry point.
    fn set_entry(&mut self, n: Node, level: usize);
}

/// A node and its distance, ordered by distance (then node: the same
/// order every time).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Near {
    /// The distance.
    pub d: f32,
    /// The node.
    pub n: Node,
}

impl Eq for Near {}

impl Ord for Near {
    fn cmp(&self, o: &Self) -> Ordering {
        self.d.total_cmp(&o.d).then(self.n.cmp(&o.n))
    }
}

impl PartialOrd for Near {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

/// A search's working memory, reused across searches (one per thread),
/// and what it counted.
#[derive(Default)]
pub struct Scratch {
    visited: Vec<u32>,
    stamp: u32,
    edges: Vec<Node>,
    vector: Vec<u8>,
    /// Out-edge lists read (hops).
    pub hops: u64,
    /// Distances taken.
    pub distances: u64,
}

impl Scratch {
    fn next_stamp(&mut self) {
        self.stamp = self.stamp.wrapping_add(1);
        if self.stamp == 0 {
            self.visited.iter_mut().for_each(|v| *v = 0);
            self.stamp = 1;
        }
    }

    /// Marks `n` visited; whether it was not yet.
    #[inline]
    fn visit(&mut self, n: Node) -> bool {
        let i = n as usize;
        if i >= self.visited.len() {
            self.visited.resize((i + 1).next_power_of_two(), 0);
        }
        let first = self.visited[i] != self.stamp;
        self.visited[i] = self.stamp;
        first
    }
}

/// The `ef` nodes nearest `q` on `layer` reachable greedily from
/// `entries`, nearest first (the paper's algorithm 2).
pub fn search_layer<G: Graph + ?Sized>(
    g: &G,
    q: &[u8],
    entries: &[Near],
    ef: usize,
    layer: usize,
    s: &mut Scratch,
) -> Vec<Near> {
    s.next_stamp();
    let mut candidates: BinaryHeap<Reverse<Near>> = BinaryHeap::with_capacity(ef * 2);
    let mut found: BinaryHeap<Near> = BinaryHeap::with_capacity(ef + 1);
    for &e in entries {
        if s.visit(e.n) {
            candidates.push(Reverse(e));
            found.push(e);
        }
    }
    while found.len() > ef {
        found.pop();
    }
    let mut edges = std::mem::take(&mut s.edges);
    while let Some(Reverse(c)) = candidates.pop() {
        if found.len() >= ef && found.peek().is_some_and(|f| c.d > f.d) {
            break;
        }
        g.neighbors(c.n, layer, &mut edges);
        s.hops += 1;
        for &e in &edges {
            if !s.visit(e) {
                continue;
            }
            let d = g.distance(q, e);
            s.distances += 1;
            if found.len() < ef || found.peek().is_some_and(|f| d < f.d) {
                let near = Near { d, n: e };
                candidates.push(Reverse(near));
                found.push(near);
                if found.len() > ef {
                    found.pop();
                }
            }
        }
    }
    s.edges = edges;
    found.into_sorted_vec()
}

/// The `k` nodes nearest `q`, nearest first, looking at `ef` candidates on
/// layer 0 (the paper's algorithm 5).
pub fn search<G: Graph + ?Sized>(
    g: &G,
    q: &[u8],
    k: usize,
    ef: usize,
    s: &mut Scratch,
) -> Vec<Near> {
    let Some((ep, top)) = g.entry() else {
        return Vec::new();
    };
    s.distances += 1;
    let mut entries = vec![Near {
        d: g.distance(q, ep),
        n: ep,
    }];
    for layer in (1..=top).rev() {
        entries = search_layer(g, q, &entries, 1, layer, s);
    }
    let mut found = search_layer(g, q, &entries, ef.max(k), 0, s);
    found.truncate(k);
    found
}

/// Up to `m` of `candidates` (nearest first, their distances to the node
/// being linked) by the heuristic: a candidate is kept when it is nearer
/// the node than every one kept before it — so the edges point in
/// different directions (the paper's algorithm 4, hnswlib's
/// `getNeighborsByHeuristic2`).
pub fn select<G: Graph + ?Sized>(
    g: &G,
    candidates: &[Near],
    m: usize,
    s: &mut Scratch,
) -> Vec<Node> {
    if candidates.len() <= m {
        return candidates.iter().map(|c| c.n).collect();
    }
    let mut kept: Vec<Node> = Vec::with_capacity(m);
    let mut v = std::mem::take(&mut s.vector);
    for c in candidates {
        if kept.len() >= m {
            break;
        }
        g.vector(c.n, &mut v);
        let good = kept.iter().all(|&r| {
            s.distances += 1;
            g.distance(&v, r) >= c.d
        });
        if good {
            kept.push(c.n);
        }
    }
    s.vector = v;
    kept
}

/// The `ef_construction` nodes nearest `vector` on each layer up to
/// `level` (nearest first; reads only).
pub fn found<G: Graph + ?Sized>(
    g: &G,
    p: &Params,
    vector: &[u8],
    level: usize,
    s: &mut Scratch,
) -> Vec<Vec<Near>> {
    let mut out: Vec<Vec<Near>> = vec![Vec::new(); level + 1];
    let Some((ep, top)) = g.entry() else {
        return out;
    };
    s.distances += 1;
    let mut entries = vec![Near {
        d: g.distance(vector, ep),
        n: ep,
    }];
    for layer in (level + 1..=top).rev() {
        entries = search_layer(g, vector, &entries, 1, layer, s);
    }
    for layer in (0..=level.min(top)).rev() {
        out[layer] = search_layer(g, vector, &entries, p.ef_construction, layer, s);
        entries = out[layer].clone();
    }
    out
}

/// Node `n`'s out-edges on each of its layers (up to its `level`): the
/// heuristic's choice from the `ef_construction` nearest on each (the
/// first half of the paper's algorithm 1; reads only).
pub fn plan<G: Graph + ?Sized>(
    g: &G,
    p: &Params,
    vector: &[u8],
    level: usize,
    s: &mut Scratch,
) -> Vec<Vec<Node>> {
    found(g, p, vector, level, s)
        .iter()
        .map(|f| select(g, f, p.m, s))
        .collect()
}

/// Node `e`'s list on `layer` once `list` (its old edges and those added)
/// has grown past `m_max · slack`: the heuristic's choice of `m_max` from
/// it (reads only); `None` while it has not.
pub fn relink<G: Graph + ?Sized>(
    g: &G,
    p: &Params,
    e: Node,
    layer: usize,
    list: &[Node],
    s: &mut Scratch,
) -> Option<Vec<Node>> {
    let most = p.m_max(layer);
    if list.len() <= most * p.slack {
        return None;
    }
    let mut v = Vec::new();
    g.vector(e, &mut v);
    let mut near: Vec<Near> = list
        .iter()
        .map(|&x| Near {
            d: g.distance(&v, x),
            n: x,
        })
        .collect();
    s.distances += near.len() as u64;
    near.sort();
    near.dedup_by_key(|x| x.n);
    Some(select(g, &near, most, s))
}

/// Node `n`'s list on `layer` with the dead nodes cut out: each dead one
/// replaced by its own live out-edges, the heuristic's choice of `m_max`
/// from them all (FreshDiskANN's delete consolidation, Singh et al. 2021,
/// its algorithm 4; reads only); `None` if `list` has no dead node.
pub fn consolidate<G: Graph + ?Sized>(
    g: &G,
    p: &Params,
    n: Node,
    layer: usize,
    list: &[Node],
    dead: &dyn Fn(Node) -> bool,
    s: &mut Scratch,
) -> Option<Vec<Node>> {
    if !list.iter().any(|&x| dead(x)) {
        return None;
    }
    let mut cands: Vec<Node> = Vec::new();
    let mut theirs = Vec::new();
    for &x in list {
        if dead(x) {
            g.neighbors(x, layer, &mut theirs);
            cands.extend(theirs.iter().copied().filter(|&y| y != n && !dead(y)));
        } else {
            cands.push(x);
        }
    }
    let mut v = Vec::new();
    g.vector(n, &mut v);
    let mut near: Vec<Near> = cands
        .iter()
        .map(|&x| Near {
            d: g.distance(&v, x),
            n: x,
        })
        .collect();
    s.distances += near.len() as u64;
    near.sort();
    near.dedup_by_key(|x| x.n);
    Some(select(g, &near, p.m_max(layer), s))
}

/// Inserts node `n` with `vector` (the paper's algorithm 1): its edges
/// ([`plan`]), then a backlink from each, a list pruned when it grows past
/// `m_max · slack` ([`relink`]).
pub fn insert<G: GraphMut + ?Sized>(
    g: &mut G,
    p: &Params,
    n: Node,
    vector: &[u8],
    s: &mut Scratch,
) {
    let level = p.level(n);
    let top = g.entry().map(|e| e.1);
    let edges = plan(g, p, vector, level, s);
    g.add_node(n, level, vector, &edges);
    let mut list = Vec::new();
    for (layer, out) in edges.iter().enumerate() {
        for &e in out {
            if g.add_edge(e, layer, n) <= p.m_max(layer) * p.slack {
                continue;
            }
            g.neighbors(e, layer, &mut list);
            if let Some(kept) = relink(g, p, e, layer, &list, s) {
                g.set_neighbors(e, layer, &kept);
            }
        }
    }
    if top.is_none_or(|t| level > t) {
        g.set_entry(n, level);
    }
}

/// `f` over `items` on `threads` threads (each with a scratch of its
/// own, its counts added to `s`), the results in `items`' order — the
/// same whatever the threads.
pub fn par_map<T: Sync, R: Send>(
    items: &[T],
    threads: usize,
    s: &mut Scratch,
    f: impl Fn(&T, &mut Scratch) -> R + Sync,
) -> Vec<R> {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    let next = AtomicUsize::new(0);
    // Each thread's results (by position) and its counts.
    type Part<R> = (Vec<(usize, R)>, u64, u64);
    let parts: Vec<Part<R>> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads.max(1))
            .map(|_| {
                sc.spawn(|| {
                    let mut s = Scratch::default();
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(16, Relaxed);
                        if i >= items.len() {
                            break;
                        }
                        for (j, it) in items.iter().enumerate().skip(i).take(16) {
                            out.push((j, f(it, &mut s)));
                        }
                    }
                    (out, s.hops, s.distances)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut slots: Vec<Option<R>> = (0..items.len()).map(|_| None).collect();
    for (out, hops, distances) in parts {
        s.hops += hops;
        s.distances += distances;
        for (j, r) in out {
            slots[j] = Some(r);
        }
    }
    slots.into_iter().map(Option::unwrap).collect()
}

/// How [`insert_batch`] batches.
#[derive(Clone, Copy, Debug)]
pub struct Batching {
    /// A batch's most nodes, as a share of the graph (0: none — a node at
    /// a time).
    pub ratio: f64,
    /// A batch's most nodes.
    pub cap: usize,
    /// Each node's candidates include its batch's other nodes.
    pub join: bool,
    /// Threads.
    pub threads: usize,
}

/// Inserts `nodes` (ascending, with their vectors) as batches (ParlayANN's
/// batch insertion, Manohar et al. 2024): a node whose level is above the
/// graph's top one at a time ([`insert`]); the others added unlinked,
/// their edges chosen on `threads` threads from what the graph as it was
/// before their batch finds for them and — with `join` — every other node
/// of the batch, its distance taken (NN-descent's local join, Dong et al.
/// 2011: rows written together are often near each other, and the graph
/// before the batch has none of them); then the backlinks grouped by the
/// node they go to, each group's added and pruned as [`insert`] does, the
/// groups in parallel.  A batch is at most `ratio` of the graph (prefix
/// doubling: from an empty graph the batches grow with it) and `cap`
/// nodes.  The same graph whatever the threads.
pub fn insert_batch<G: GraphMut + Sync + ?Sized>(
    g: &mut G,
    p: &Params,
    nodes: &[(Node, &[u8])],
    b: &Batching,
    s: &mut Scratch,
) {
    let Batching {
        ratio,
        cap,
        join,
        threads,
    } = *b;
    let mut done = 0;
    let mut size = g.entry().map_or(0, |_| 1usize);
    // The graph's size: the nodes before the first plus those inserted.
    if let Some(&(first, _)) = nodes.first() {
        size = size.max(first as usize);
    }
    while done < nodes.len() {
        let take = ((size as f64 * ratio) as usize)
            .clamp(1, cap.max(1))
            .min(nodes.len() - done);
        let batch = &nodes[done..done + take];
        let top = g.entry().map(|e| e.1);
        type Picked<'n, 'v> = Vec<&'n (Node, &'v [u8])>;
        let (high, low): (Picked, Picked) = batch
            .iter()
            .partition(|(n, _)| top.is_none_or(|t| p.level(*n) > t));
        for &&(n, v) in &high {
            insert(g, p, n, v, s);
        }
        for &&(n, v) in &low {
            let level = p.level(n);
            g.add_node(n, level, v, &vec![Vec::new(); level + 1]);
        }
        let planned = par_map(&low, threads, s, |&&(n, v), s| {
            let level = p.level(n);
            let mut cands = found(&*g, p, v, level, s);
            if join {
                for &&(y, w) in &low {
                    if y == n {
                        continue;
                    }
                    let ly = p.level(y);
                    let d = p.metric.distance(v, w);
                    s.distances += 1;
                    for c in cands.iter_mut().take(ly.min(level) + 1) {
                        c.push(Near { d, n: y });
                    }
                }
                for c in &mut cands {
                    c.sort();
                    c.dedup_by_key(|x| x.n);
                    c.truncate(p.ef_construction.max(p.m));
                }
            }
            cands
                .iter()
                .map(|f| select(&*g, f, p.m, s))
                .collect::<Vec<Vec<Node>>>()
        });
        let mut back: Vec<(Node, usize, Node)> = Vec::new();
        for (&&(n, _), edges) in low.iter().zip(&planned) {
            for (layer, out) in edges.iter().enumerate() {
                g.set_neighbors(n, layer, out);
                back.extend(out.iter().map(|&e| (e, layer, n)));
            }
        }
        back.sort_unstable();
        let mut groups: Vec<(Node, usize, Vec<Node>)> = Vec::new();
        for (e, layer, n) in back {
            match groups.last_mut() {
                Some((ge, gl, l)) if *ge == e && *gl == layer => l.push(n),
                _ => groups.push((e, layer, vec![n])),
            }
        }
        // Each edge added as [`insert`] adds it: pruned when the list
        // grows past its bound, in the nodes' order.
        let lists = par_map(&groups, threads, s, |(e, layer, added), s| {
            let mut list = Vec::new();
            g.neighbors(*e, *layer, &mut list);
            for &n in added {
                list.push(n);
                if let Some(kept) = relink(&*g, p, *e, *layer, &list, s) {
                    list = kept;
                }
            }
            list
        });
        for ((e, layer, _), list) in groups.iter().zip(&lists) {
            g.set_neighbors(*e, *layer, list);
        }
        done += take;
        size += take;
    }
}

/// The `k` vectors of `data` (`width` bytes each) nearest `q`, nearest
/// first: the exact answer the graph is held to.
pub fn exact(metric: Metric, data: &[u8], width: usize, q: &[u8], k: usize) -> Vec<Near> {
    let mut found: BinaryHeap<Near> = BinaryHeap::with_capacity(k + 1);
    for (i, v) in data.chunks_exact(width).enumerate() {
        let d = metric.distance(q, v);
        if found.len() < k || found.peek().is_some_and(|f| d < f.d) {
            found.push(Near { d, n: i as Node });
            if found.len() > k {
                found.pop();
            }
        }
    }
    found.into_sorted_vec()
}
