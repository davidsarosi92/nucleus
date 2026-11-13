//! The sparse vector index (PLAN.md §M14.21 T7b): `CREATE INDEX … USING
//! sparse (col)` on a `sparsevec` column — a learned sparse model's term
//! weights (SPLADE's), a bag of words weighted — ranking `ORDER BY col <#>
//! q LIMIT k` (the inner product, greatest first) exactly.
//!
//! The text index's storage ([`super::store`]), its documents *weighted*:
//! a dimension is a term (its number in hexadecimal, so the dictionary's
//! order is the dimensions'), a component's weight the term's frequency
//! in the postings ([`super::segment::weight_bits`]: in the numbers'
//! order), no positions.  The impacts a block keeps give its greatest
//! weight, so block-max MaxScore ([`super::store`]) ranks by the inner
//! product as it ranks by BM25: each leaf's bound its weight in the query
//! times a block's greatest — Elasticsearch's `sparse_vector` and
//! OpenSearch's neural sparse search over Lucene's impacts, Vespa's and
//! Milvus's WAND over the same bounds.
//!
//! Exact: the index sums the products in units (2^-32, each rounded
//! once), the function in `f64` — within `MARGIN` units a leaf of each
//! other.  The index gives every row it scores within that of the `k`-th
//! best; the sort above computes `<#>` of each and keeps the first `k` —
//! those of a scan.  It gives up (the rows are read) where a row it does
//! not find could be among them: a query weight not positive, fewer than
//! `k` rows with a positive product (the rest are ties at 0), a product
//! too large for units, another condition's rows to rank among.

use nucleus_kv::KvTxn;

use super::segment::{Doc, weight_bits};
use super::store;
use crate::catalog::{Index as IndexDef, Table};
use crate::error::{Error, Result};
use crate::ext::Registry;
use crate::index_method::{IndexMethod, Keep, RankAsk, RankCost, RowRef};
use crate::text::query::{Leaf, Node, Query};
use crate::text::score::{Scorer, of_fixed};
use crate::types::{Row, Value};

/// The method's name.
pub const NAME: &str = "sparse";

/// The function it ranks by: `<#>` of two `sparsevec`s.
const RANKS: &str = "negative_inner_product";

/// The index's sum and the function's may differ by half a unit a leaf
/// (each product rounded) and by the `f64` sum's rounding (at most
/// 2^-53 of the sum of a leaf's products, each at most 2^16 —
/// [`Scorer::dot`] refuses more — about 2^10 units a leaf): this many
/// units a leaf, and one more, is margin enough.
const MARGIN: i64 = 1025;

/// The sparse vector index method.
pub struct SparseIndex;

/// Dimension `d` as a term: hexadecimal, eight digits, so terms sort as
/// dimensions do.
fn term(d: u32) -> String {
    format!("{d:08x}")
}

/// A `sparsevec` value as the index keeps it.
fn doc_of(v: &crate::sparse::Sparse) -> Doc {
    Doc {
        len: v.at.len() as u32,
        terms: v.at.iter().map(|&d| (term(d), Vec::new())).collect(),
        weights: v.values.iter().map(|&w| weight_bits(w)).collect(),
    }
}

/// The value of `row` the index holds, its hash and its document (`None`:
/// a `NULL`, not indexed).
fn indexed(t: &Table, i: usize, row: &Row) -> Result<Option<(u64, Doc)>> {
    match &row[t.indexes[i].columns[0]] {
        Value::Null => Ok(None),
        v @ Value::Ext(_, b) => Ok(Some((
            crate::object_cache::object_id(14, b),
            doc_of(&crate::sparse::Sparse::of(v)?),
        ))),
        _ => Err(Error::internal("a sparse index of a column not sparsevec")),
    }
}

impl IndexMethod for SparseIndex {
    fn name(&self) -> &str {
        NAME
    }

    fn scrub(
        &self,
        kv: &std::sync::Arc<dyn nucleus_kv::Kv>,
        _: &Registry,
        t: &Table,
        i: usize,
    ) -> Result<()> {
        super::scrub(kv, t, i)
    }

    fn define(&self, t: &Table, ix: &mut IndexDef) -> Result<()> {
        if ix.columns.len() != 1 {
            return Err(Error::feature("a sparse index has one column"));
        }
        let col = &t.columns[ix.columns[0]];
        if col.ty != crate::sparse::data_type() {
            return Err(Error::feature(format!(
                "a sparse index needs a sparsevec column, not {}",
                col.ty.name()
            )));
        }
        match ix.opclasses[0].as_deref() {
            None | Some("sparsevec_ip_ops") => {}
            Some(o) => {
                return Err(Error::undefined_object(format!(
                    "operator class \"{o}\" does not exist for access method \"sparse\""
                )));
            }
        }
        if let Some((k, _)) = ix.options.first() {
            return Err(Error::invalid_parameter(format!(
                "unrecognized parameter \"{k}\""
            )));
        }
        Ok(())
    }

    fn write(
        &self,
        txn: &mut dyn KvTxn,
        t: &Table,
        i: usize,
        old: Option<RowRef>,
        new: Option<RowRef>,
    ) -> Result<()> {
        let old = match old {
            Some((pk, row)) => indexed(t, i, row)?.map(|d| (pk, d)),
            None => None,
        };
        let new = match new {
            Some((pk, row)) => indexed(t, i, row)?.map(|d| (pk, d)),
            None => None,
        };
        if old.is_some() && old == new {
            return Ok(());
        }
        let s = super::store(t, i);
        if let Some((pk, (hash, doc))) = old {
            let key = s.keys.tombstone(pk, hash, crate::hnsw::store::nonce());
            txn.put(&key, &store::encode_gone(&doc))?;
        }
        if let Some((pk, (hash, doc))) = new {
            let key = s.keys.delta(pk, crate::hnsw::store::nonce());
            txn.put(&key, &store::encode_insert(hash, &doc))?;
        }
        super::pending().note(super::Id {
            instance: txn.instance(),
            table: t.id,
            index: t.indexes[i].id,
        });
        Ok(())
    }

    /// The rows' vectors read, made segments of [`super::BUILD_BATCH`],
    /// merged into one where they fit.
    fn build(&self, txn: &mut dyn KvTxn, _: &Registry, t: &Table, i: usize) -> Result<()> {
        let col = t.indexes[i].columns[0];
        let mut wanted = vec![false; t.columns.len()];
        wanted[col] = true;
        let s = super::store(t, i);
        let mut docs: Vec<(Vec<u8>, u64, Doc)> = Vec::new();
        let mut made = Vec::new();
        for r in crate::index_method::rows(&*txn, t, Some(&wanted))? {
            let (pk, row) = r?;
            if let Some((hash, doc)) = indexed(t, i, &row)? {
                docs.push((pk, hash, doc));
            }
            if docs.len() == super::BUILD_BATCH {
                made.push((super::segment::of_docs(&docs)?, docs.len() as u32));
                docs.clear();
            }
        }
        if !docs.is_empty() {
            made.push((super::segment::of_docs(&docs)?, docs.len() as u32));
        }
        let bytes: usize = made.iter().map(|(b, _)| b.len()).sum();
        s.append_segments(txn, made)?;
        let workers = crate::budget::workers();
        if bytes <= super::ONE_SEGMENT {
            s.merge_all(txn, workers)
        } else {
            s.merge_due(txn, workers)
        }
    }

    fn check(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        i: usize,
        rows: &[(Vec<u8>, Row)],
        problems: &mut Vec<String>,
    ) -> Result<()> {
        let name = &t.indexes[i].name;
        let view = super::store(t, i).view(txn)?;
        let mut want = Vec::new();
        for (pk, row) in rows {
            if let Some((hash, _)) = indexed(t, i, row)? {
                want.push((pk.clone(), hash));
            }
        }
        want.sort();
        let have = view.live();
        if have != want {
            let extra = have
                .iter()
                .filter(|x| want.binary_search(x).is_err())
                .count();
            let missing = want
                .iter()
                .filter(|x| have.binary_search(x).is_err())
                .count();
            problems.push(format!(
                "sparse index \"{name}\": {missing} rows missing, {extra} it should not have"
            ));
        }
        Ok(())
    }

    /// By the inner product, greatest first: `col <#> q` ascending.
    fn ranks(&self, _: &IndexDef, function: &str, desc: bool, nulls_first: bool) -> bool {
        function == RANKS && !desc && !nulls_first
    }

    /// As the text index's: `k` over the share passing, never more than
    /// the rows.
    fn rank_cost(&self, _: &IndexDef, c: &RankCost) -> Option<f64> {
        (c.function == RANKS && c.served.is_none()).then(|| (c.k / c.share).min(c.rows))
    }

    fn rows(&self, txn: &dyn KvTxn, t: &Table, i: usize) -> Result<Option<usize>> {
        Ok(Some(super::store(t, i).view(txn)?.stats(&[]).docs as usize))
    }

    /// The rows by the inner product with the query, greatest first, read
    /// as the text index reads its ranking — `k` over the share expected
    /// to pass `keep`, more where fewer passed — then on through every row
    /// within [`MARGIN`] units a leaf of the `k`-th passing.
    fn rank(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        i: usize,
        ask: &mut RankAsk,
        wanted: Option<&[bool]>,
        mut keep: Option<Keep>,
    ) -> Result<Option<Vec<(Vec<u8>, Row)>>> {
        let r = ask.rank;
        if r.function != RANKS || r.desc || ask.served.is_some() || ask.among.is_some() {
            return Ok(None);
        }
        let [q @ Value::Ext(..)] = r.args.as_slice() else {
            return Ok(None);
        };
        let q = crate::sparse::Sparse::of(q)?;
        if q.at.is_empty() || q.values.iter().any(|&w| w <= 0.0) {
            return Ok(None);
        }
        let k = r.k;
        if k == 0 {
            return Ok(Some(Vec::new()));
        }
        let leaves = q.at.len();
        let query = Query {
            leaves: q.at.iter().map(|&d| Leaf::Exact(term(d))).collect(),
            root: Some(Node::Bool {
                must: Vec::new(),
                should: (0..leaves).map(Node::Leaf).collect(),
                not: Vec::new(),
            }),
        };
        let sc = Scorer::dot(&query, q.values.iter().map(|&w| f64::from(w)).collect());
        let margin = of_fixed(MARGIN * leaves as i64 + 1);
        let view = super::store(t, i).view(txn)?;
        let read = keep.is_some() || wanted.is_none_or(|w| w.contains(&true));
        let storage = crate::storage::of(t)?;
        let mut key = t.index_prefix(0);
        let base = key.len();
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let (mut checked, mut passed) = (0usize, 0usize);
        let mut floor: Option<f64> = None;
        let mut slice = ((k as f64 / ask.share.clamp(1e-9, 1.0)).ceil() as usize).max(k);
        let mut n = if keep.is_some() {
            slice.saturating_mul(4)
        } else {
            k
        };
        for pass in 0.. {
            let mut ranked = view.ranked(&query, &sc, n)?;
            let mut below = false;
            while ranked.left() > 0 && !below {
                for (pk, s) in ranked.next(slice) {
                    if floor.is_some_and(|f| s < f) {
                        below = true;
                        break;
                    }
                    if !seen.insert(pk.to_vec()) {
                        continue;
                    }
                    let row = if read {
                        crate::budget::charge_scan()?;
                        key.truncate(base);
                        key.extend_from_slice(pk);
                        storage
                            .get(txn, t, &key, wanted, &mut crate::enums::Labels::default())?
                            .ok_or_else(|| {
                                Error::internal(
                                    "an index method found a row its table does not have",
                                )
                            })?
                    } else {
                        vec![Value::Null; t.columns.len()]
                    };
                    checked += 1;
                    if let Some(keep) = keep.as_mut()
                        && !keep(&row, false)?
                    {
                        continue;
                    }
                    passed += 1;
                    out.push((pk.to_vec(), row));
                    if passed == k {
                        floor = Some(s - margin);
                    }
                }
                slice = match (floor, passed) {
                    (Some(_), _) => slice,
                    (None, 0) => (2 * checked).max(slice),
                    (None, _) => {
                        let rest = (k - passed) as f64 * checked as f64 / passed as f64;
                        (rest.ceil() as usize).max(slice)
                    }
                };
            }
            if below || ranked.complete() {
                break;
            }
            // The first `n` ran out above the floor, or before `k` passed:
            // more of them, all on the third pass.
            n = if pass >= 1 {
                usize::MAX
            } else {
                n.saturating_mul(4)
            };
        }
        // Rows it did not find tie at 0 with the `k`-th, or may beat it.
        if passed < k
            || floor.is_none_or(|f| f <= 0.0)
            || sc.overflow.load(std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(None);
        }
        Ok(Some(out))
    }
}

/// Registers the method (the built-in module does, with the text index).
pub fn register(r: &mut Registry) -> Result<()> {
    r.add_index_method(std::sync::Arc::new(SparseIndex))
}
