//! What a storage model offers the measurements (one interface for the
//! three measured — PLAN.md §M14.12 V4 — of which (C) is left).

use crate::hnsw::Scratch;

/// A row found and its distance.
#[derive(Clone, Copy, Debug)]
pub struct Hit {
    /// The distance.
    pub d: f32,
    /// The row.
    pub row: u32,
}

/// A storage model of the HNSW graph on the KV.
pub trait Model: Sync + Send {
    /// What it is, with its settings.
    fn name(&self) -> String;
    /// Builds the index over `rows` in one transaction (`CREATE INDEX` on a
    /// table that has them).
    fn bulk(&self, rows: &[(u32, &[u8])]);
    /// Inserts one row in a transaction of its own; returns the retries it
    /// took.
    fn insert(&self, row: u32, v: &[u8]) -> u32;
    /// Deletes `row` — whose vector was `old` — or with `new` updates it,
    /// in a transaction of its own (the table's row changed with it);
    /// returns the retries it took.
    fn change(&self, row: u32, old: &[u8], new: Option<&[u8]>) -> u32 {
        let _ = (row, old, new);
        unimplemented!("{}: no deletes", self.name())
    }
    /// The model's upkeep (a fold, a seal, a merge) if `threshold` rows
    /// wait for it; whether it did any.
    fn maintain(&self, threshold: usize) -> bool;
    /// The `k` rows nearest `q` in a snapshot of its own.
    fn search(&self, q: &[u8], k: usize, ef: usize, s: &mut Scratch) -> Vec<Hit>;
    /// Bytes written to the KV so far (keys and values).
    fn written(&self) -> u64;
    /// Its own counters.
    fn report(&self) -> String;
}
