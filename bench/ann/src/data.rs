//! Datasets as `bench/ann/data.py` writes them — `train.f32`, `test.f32`,
//! `neighbors.i32` and `meta.txt` in a directory — read and written.

use std::fs;
use std::path::Path;

use crate::hnsw::{self, Metric};

/// A dataset: the vectors indexed, the queries, each query's true
/// nearest rows (nearest first).
pub struct Dataset {
    /// Components per vector.
    pub dim: usize,
    /// The distance.
    pub metric: Metric,
    /// The vectors indexed, little-endian `f32`s side by side.
    pub train: Vec<u8>,
    /// The queries, the same way.
    pub test: Vec<u8>,
    /// Each query's true nearest train rows.
    pub truth: Vec<Vec<u32>>,
}

impl Dataset {
    /// A vector's bytes.
    pub fn width(&self) -> usize {
        self.dim * 4
    }

    /// The vectors indexed.
    pub fn rows(&self) -> usize {
        self.train.len() / self.width()
    }

    /// Reads the dataset in `dir`.
    pub fn read(dir: &Path) -> Self {
        let meta = fs::read_to_string(dir.join("meta.txt")).expect("meta.txt");
        let field = |k: &str| -> String {
            meta.split_whitespace()
                .find_map(|kv| kv.strip_prefix(k).and_then(|r| r.strip_prefix('=')))
                .unwrap_or_else(|| panic!("meta.txt: no {k}"))
                .to_string()
        };
        let dim: usize = field("dim").parse().unwrap();
        let queries: usize = field("test").parse().unwrap();
        let metric = match field("metric").as_str() {
            "l2" => Metric::L2,
            "cosine" => Metric::Cosine,
            m => panic!("meta.txt: metric {m}"),
        };
        let train = fs::read(dir.join("train.f32")).expect("train.f32");
        let test = fs::read(dir.join("test.f32")).expect("test.f32");
        let nb = fs::read(dir.join("neighbors.i32")).expect("neighbors.i32");
        let per = nb.len() / 4 / queries;
        let truth = nb
            .as_chunks::<4>()
            .0
            .chunks_exact(per)
            .map(|q| q.iter().map(|b| i32::from_le_bytes(*b) as u32).collect())
            .collect();
        assert_eq!(train.len() % (dim * 4), 0);
        Self {
            dim,
            metric,
            train,
            test,
            truth,
        }
    }

    /// Writes the dataset to `dir`, with the exact 100 nearest of each
    /// query as its truth (computed here).
    pub fn write(dim: usize, metric: Metric, train: Vec<u8>, test: Vec<u8>, dir: &Path) {
        fs::create_dir_all(dir).expect("mkdir");
        let width = dim * 4;
        let mut nb = Vec::new();
        for q in test.chunks_exact(width) {
            for n in hnsw::exact(metric, &train, width, q, 100) {
                nb.extend_from_slice(&(n.n as i32).to_le_bytes());
            }
        }
        let name = match metric {
            Metric::L2 => "l2",
            Metric::Cosine => "cosine",
        };
        fs::write(
            dir.join("meta.txt"),
            format!(
                "dim={dim} train={} test={} metric={name}\n",
                train.len() / width,
                test.len() / width
            ),
        )
        .expect("meta.txt");
        fs::write(dir.join("train.f32"), &train).expect("train.f32");
        fs::write(dir.join("test.f32"), &test).expect("test.f32");
        fs::write(dir.join("neighbors.i32"), &nb).expect("neighbors.i32");
    }
}
