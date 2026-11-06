//! The graph in memory, as hnswlib keeps it: the reference the storage
//! models are measured against (what a hop costs with no storage under
//! it), and the store the core is checked on.

use crate::hnsw::{Graph, GraphMut, Metric, Node};

/// Vectors side by side, each node's out-edges per layer.
pub struct Mem {
    metric: Metric,
    width: usize,
    vectors: Vec<u8>,
    edges: Vec<Vec<Vec<Node>>>,
    entry: Option<(Node, usize)>,
}

impl Mem {
    /// An empty graph of vectors `width` bytes long.
    pub fn new(metric: Metric, width: usize) -> Self {
        Self {
            metric,
            width,
            vectors: Vec::new(),
            edges: Vec::new(),
            entry: None,
        }
    }

    #[inline]
    fn at(&self, n: Node) -> &[u8] {
        let i = n as usize * self.width;
        &self.vectors[i..i + self.width]
    }

    /// The bytes the graph holds (vectors and edge lists).
    pub fn bytes(&self) -> usize {
        self.vectors.len()
            + self
                .edges
                .iter()
                .flatten()
                .map(|l| l.len() * 4)
                .sum::<usize>()
    }
}

impl Graph for Mem {
    #[inline]
    fn distance(&self, q: &[u8], n: Node) -> f32 {
        self.metric.distance(q, self.at(n))
    }

    fn vector(&self, n: Node, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(self.at(n));
    }

    #[inline]
    fn neighbors(&self, n: Node, layer: usize, out: &mut Vec<Node>) {
        out.clear();
        if let Some(l) = self.edges[n as usize].get(layer) {
            out.extend_from_slice(l);
        }
    }

    fn entry(&self) -> Option<(Node, usize)> {
        self.entry
    }
}

impl GraphMut for Mem {
    fn add_node(&mut self, n: Node, _level: usize, vector: &[u8], edges: &[Vec<Node>]) {
        let i = n as usize;
        if self.edges.len() <= i {
            self.edges.resize(i + 1, Vec::new());
            self.vectors.resize((i + 1) * self.width, 0);
        }
        self.vectors[i * self.width..(i + 1) * self.width].copy_from_slice(vector);
        self.edges[i] = edges.to_vec();
    }

    fn add_edge(&mut self, n: Node, layer: usize, to: Node) -> usize {
        let l = &mut self.edges[n as usize][layer];
        l.push(to);
        l.len()
    }

    fn set_neighbors(&mut self, n: Node, layer: usize, edges: &[Node]) {
        self.edges[n as usize][layer] = edges.to_vec();
    }

    fn set_entry(&mut self, n: Node, level: usize) {
        self.entry = Some((n, level));
    }
}
