//! The text index (PLAN.md §M14.21 T4): `CREATE INDEX … USING text
//! (col) WITH (analyzer = 'english')` on a text column — an inverted index
//! of the column's terms by the analyzer named ([`crate::text`]; `simple`
//! by default), kept exact in every snapshot ([`store`]).
//!
//! The index decides what `col @@ q` means: the binder gives the operator
//! the index's analyzer ([`crate::index_method::IndexMethod::operand`]),
//! so a row matches alike whether the index finds it or the row is read;
//! and the planner asks the index for the rows of a filter's `col @@ q`
//! with `q` fixed for the scan ([`crate::plan::Access::Method`]).
//!
//! A write puts a delta row or a tombstone in the statement's transaction.
//! The upkeep — a seal of the delta into a segment once it holds
//! [`SEAL`] rows, merges after — runs as the HNSW index's does: after a
//! commit, by the session whose commit finds it due, in transactions of
//! its own.  A `NULL` is not indexed.

pub mod json;
pub(crate) mod segment;
pub mod sparse;
pub(crate) mod store;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

use nucleus_kv::{Kv, KvTxn};

use crate::catalog::{Index as IndexDef, Table};
use crate::error::{Error, Result};
use crate::ext::Registry;
use crate::index_method::{IndexMethod, Keep, RankAsk, RankCost, RowRef};
use crate::types::{DataType, Row, Value};

use segment::Doc;
use store::{Keys, Store};

/// The method's name.
pub const NAME: &str = "text";

/// Delta rows past which a seal is due.  A search reads the delta from
/// the KV and analyzes nothing (its rows hold their terms): about a
/// microsecond a row against the seal's few milliseconds.
pub const SEAL: u64 = 1024;
/// Documents a build puts in a segment at most (merges then join them).
const BUILD_BATCH: usize = 100_000;
/// …and at least, where its workers share fewer rows (each a segment).
const BUILD_MIN: usize = 4096;
/// The share of the matches a ranking's first `n` may be at most for
/// MaxScore to skip by their threshold; past it, every match is scored.
const WHOLE: f64 = 64.0;
/// The bytes a build's segments may hold together for it to merge them
/// into one (a segment's offsets are 32 bits; merged, they shrink).
const ONE_SEGMENT: usize = 3 << 30;

/// The text index method.
pub struct TextIndex;

/// Index `ix`'s analyzer, if it is a text index.
pub fn analyzer_of(ix: &IndexDef) -> Option<&str> {
    if ix.method.as_deref() != Some(NAME) {
        return None;
    }
    Some(
        ix.options
            .iter()
            .find(|(k, _)| k == "analyzer")
            .map_or(crate::text::DEFAULT, |(_, v)| v.as_str()),
    )
}

/// Index `ix`'s BM25 parameters (`WITH (k1 = …, b = …)`; Lucene's
/// 1.2 and 0.75 by default).
pub(crate) fn params_of(ix: &IndexDef) -> crate::text::score::Params {
    let mut p = crate::text::score::DEFAULT;
    for (k, v) in &ix.options {
        match (k.as_str(), v.parse::<f64>()) {
            ("k1", Ok(x)) => p.k1 = x,
            ("b", Ok(x)) => p.b = x,
            _ => {}
        }
    }
    p
}

fn store(t: &Table, i: usize) -> Store {
    Store {
        keys: Keys::new(t.index_prefix(t.indexes[i].id)),
        weighted: matches!(
            t.indexes[i].method.as_deref(),
            Some(sparse::NAME | json::NAME)
        ),
    }
}

/// A text's hash: what a tombstone names the version of a row by.
fn text_hash(s: &str) -> u64 {
    crate::object_cache::object_id(13, s.as_bytes())
}

/// The text of `row` the index holds (`None`: a `NULL`, not indexed).
fn text_of<'r>(t: &Table, i: usize, row: &'r Row) -> Result<Option<&'r str>> {
    match &row[t.indexes[i].columns[0]] {
        Value::Null => Ok(None),
        Value::Text(s) => Ok(Some(s)),
        _ => Err(Error::internal("a text index of a column not text")),
    }
}

/// An index, where it is: its database, table and id.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Id {
    instance: u64,
    table: u32,
    index: u32,
}

/// Delta rows written by index since its upkeep last ran (this process's).
#[derive(Default)]
struct Pending {
    any: AtomicBool,
    by: Mutex<HashMap<Id, u64>>,
    /// Held while an index is sealed or merged: one at a time.
    busy: Mutex<()>,
}

fn pending() -> &'static Pending {
    static P: OnceLock<Pending> = OnceLock::new();
    P.get_or_init(Pending::default)
}

impl Pending {
    fn note(&self, id: Id) {
        *self
            .by
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(id)
            .or_default() += 1;
        self.any.store(true, Relaxed);
    }
}

impl IndexMethod for TextIndex {
    fn name(&self) -> &str {
        NAME
    }

    fn define(&self, t: &Table, ix: &mut IndexDef) -> Result<()> {
        if ix.columns.len() != 1 {
            return Err(Error::feature("a text index has one column"));
        }
        let col = &t.columns[ix.columns[0]];
        if col.ty != DataType::Text {
            return Err(Error::feature(format!(
                "a text index needs a text column, not {}",
                col.ty.name()
            )));
        }
        if let Some(o) = &ix.opclasses[0] {
            return Err(Error::undefined_object(format!(
                "operator class \"{o}\" does not exist for access method \"text\""
            )));
        }
        let mut analyzer = crate::text::DEFAULT.to_string();
        let mut bm25 = Vec::new();
        for (k, v) in &ix.options {
            match k.as_str() {
                "analyzer" => analyzer = v.clone(),
                // BM25's: k1, how fast a term's weight saturates with its
                // frequency, ≥ 0; b, how much a text's length counts, 0…1.
                "k1" | "b" => {
                    let x = v
                        .parse::<f64>()
                        .ok()
                        .filter(|x| x.is_finite() && *x >= 0.0 && (k == "k1" || *x <= 1.0));
                    let Some(x) = x else {
                        return Err(Error::invalid_parameter(format!(
                            "{k} must be a number {}, not {v}",
                            if k == "k1" {
                                "of 0 or more"
                            } else {
                                "from 0 to 1"
                            }
                        )));
                    };
                    bm25.retain(|(n, _): &(String, String)| n != k);
                    bm25.push((k.clone(), x.to_string()));
                }
                _ => {
                    return Err(Error::invalid_parameter(format!(
                        "unrecognized parameter \"{k}\""
                    )));
                }
            }
        }
        crate::text::named(&analyzer)?;
        ix.options = vec![("analyzer".into(), analyzer)];
        ix.options.extend(bm25);
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
            Some((pk, row)) => text_of(t, i, row)?.map(|s| (pk, s)),
            None => None,
        };
        let new = match new {
            Some((pk, row)) => text_of(t, i, row)?.map(|s| (pk, s)),
            None => None,
        };
        if old.is_some() && old == new {
            return Ok(()); // the text and the row's key unchanged
        }
        let a = crate::text::named(analyzer_of(&t.indexes[i]).unwrap_or(crate::text::DEFAULT))?;
        let s = store(t, i);
        if let Some((pk, text)) = old {
            let doc = Doc::analyze(a.as_ref(), text);
            let key = s
                .keys
                .tombstone(pk, text_hash(text), crate::hnsw::store::nonce());
            txn.put(&key, &store::encode_gone(&doc))?;
        }
        if let Some((pk, text)) = new {
            let doc = Doc::analyze(a.as_ref(), text);
            let key = s.keys.delta(pk, crate::hnsw::store::nonce());
            txn.put(&key, &store::encode_insert(text_hash(text), &doc))?;
        }
        pending().note(Id {
            instance: txn.instance(),
            table: t.id,
            index: t.indexes[i].id,
        });
        Ok(())
    }

    /// The rows' texts read, then analyzed and made segments in batches
    /// on the statement's workers (Tantivy's and Lucene's indexing
    /// threads, each its own segments), the merges due then run a round
    /// at a time, a round's on the workers too.
    fn build(&self, txn: &mut dyn KvTxn, _: &Registry, t: &Table, i: usize) -> Result<()> {
        let col = t.indexes[i].columns[0];
        let mut wanted = vec![false; t.columns.len()];
        wanted[col] = true;
        let a = crate::text::named(analyzer_of(&t.indexes[i]).unwrap_or(crate::text::DEFAULT))?;
        let s = store(t, i);
        let mut texts: Vec<(Vec<u8>, String)> = Vec::new();
        for r in crate::index_method::rows(&*txn, t, Some(&wanted))? {
            let (pk, mut row) = r?;
            match std::mem::replace(&mut row[col], Value::Null) {
                Value::Null => {}
                Value::Text(text) => texts.push((pk, text)),
                _ => return Err(Error::internal("a text index of a column not text")),
            }
        }
        let workers = crate::budget::workers();
        let per = texts.len().div_ceil(workers).clamp(BUILD_MIN, BUILD_BATCH);
        let batches: Vec<&[(Vec<u8>, String)]> = texts.chunks(per).collect();
        let make = |batch: &[(Vec<u8>, String)]| {
            let docs: Vec<(Vec<u8>, u64, Doc)> = batch
                .iter()
                .map(|(pk, text)| (pk.clone(), text_hash(text), Doc::analyze(a.as_ref(), text)))
                .collect();
            segment::of_docs(&docs).map(|b| (b, docs.len() as u32))
        };
        let made: Vec<Result<(Vec<u8>, u32)>> = if workers <= 1 || batches.len() <= 1 {
            batches.iter().map(|b| make(b)).collect()
        } else {
            let next = std::sync::atomic::AtomicUsize::new(0);
            let out: Vec<Mutex<Option<Result<(Vec<u8>, u32)>>>> =
                batches.iter().map(|_| Mutex::new(None)).collect();
            std::thread::scope(|sc| {
                for _ in 0..workers.min(batches.len()) {
                    sc.spawn(|| {
                        loop {
                            let b = next.fetch_add(1, Relaxed);
                            let Some(batch) = batches.get(b) else { break };
                            *out[b].lock().unwrap_or_else(|e| e.into_inner()) = Some(make(batch));
                        }
                    });
                }
            });
            out.into_iter()
                .map(|m| {
                    m.into_inner()
                        .unwrap_or_else(|e| e.into_inner())
                        .expect("made")
                })
                .collect()
        };
        drop(texts);
        let made = made.into_iter().collect::<Result<Vec<_>>>()?;
        let bytes: usize = made.iter().map(|(b, _)| b.len()).sum();
        s.append_segments(txn, made)?;
        // One segment where it fits — a search looks each term up once per
        // segment (a long query's hundreds of terms), and an index just
        // built is read before it is written (Lucene's `forceMerge(1)` for
        // an index done with); one merge of them all reads each document
        // once, where merges by tiers read it once a tier.
        if bytes <= ONE_SEGMENT {
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
        let a = crate::text::named(analyzer_of(&t.indexes[i]).unwrap_or(crate::text::DEFAULT))?;
        let view = store(t, i).view(txn)?;
        // The rows' versions, and the statistics their texts give.
        let mut want = Vec::new();
        let (mut docs, mut len) = (0u64, 0u64);
        let mut df: HashMap<String, u64> = HashMap::new();
        for (pk, row) in rows {
            if let Some(text) = text_of(t, i, row)? {
                want.push((pk.clone(), text_hash(text)));
                let d = Doc::analyze(a.as_ref(), text);
                docs += 1;
                len += u64::from(d.len);
                for (term, _) in d.terms {
                    *df.entry(term).or_default() += 1;
                }
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
                "text index \"{name}\": {missing} rows missing, {extra} it should not have"
            ));
        }
        let terms: Vec<&str> = df.keys().map(String::as_str).collect();
        let st = view.stats(&terms);
        if (st.docs, st.len) != (docs, len) {
            problems.push(format!(
                "text index \"{name}\": counts {} documents of {} terms, the rows {docs} of {len}",
                st.docs, st.len
            ));
        }
        let wrong = terms
            .iter()
            .zip(&st.df)
            .filter(|(t, n)| df[**t] != **n)
            .count();
        if wrong > 0 {
            problems.push(format!(
                "text index \"{name}\": {wrong} terms' document counts differ from the rows'"
            ));
        }
        Ok(())
    }

    fn after_statement(&self, kv: &Arc<dyn Kv>, _wrote: bool) -> Result<()> {
        upkeep(kv, false)
    }

    fn scrub(&self, kv: &Arc<dyn Kv>, _: &Registry, t: &Table, i: usize) -> Result<()> {
        scrub(kv, t, i)
    }

    fn operand(&self, ix: &IndexDef, op: &str) -> Option<Value> {
        (op == "@@").then(|| Value::Text(analyzer_of(ix).unwrap_or(crate::text::DEFAULT).into()))
    }

    fn function(
        &self,
        t: &Table,
        i: usize,
        function: &str,
        arg: &crate::expr::Expr,
        parts: Option<&crate::expr::Expr>,
    ) -> Result<Option<(crate::expr::Expr, DataType)>> {
        if function != "score" {
            return Ok(None);
        }
        crate::text::score::bind(t, i, arg, params_of(&t.indexes[i]), parts)
    }

    fn serves(&self, _: &IndexDef, function: &str) -> bool {
        function == "@@"
    }

    /// By BM25, greatest first: `score(col @@ q)` bound
    /// ([`crate::text::score::bind`]) — never `NULL` on a row it finds.
    fn ranks(&self, _: &IndexDef, function: &str, desc: bool, _: bool) -> bool {
        function == crate::text::score::TEXT_SCORE && desc
    }

    /// Its BM25: MaxScore's sums are the row's text's, to the unit
    /// ([`crate::text::score::fixed`]).
    fn values(&self, _: &IndexDef, function: &str) -> bool {
        function == crate::text::score::TEXT_SCORE
    }

    /// The rows the condition finds where it is the index's (`@@`): only
    /// as many as `k` over the share passing need be read, never more
    /// than it finds.
    fn rank_cost(&self, _: &IndexDef, c: &RankCost) -> Option<f64> {
        (c.function == crate::text::score::TEXT_SCORE && c.served == Some("@@"))
            .then(|| (c.k / c.share).min(c.rows))
    }

    /// `@@`'s share: each word's documents over the index's
    /// ([`crate::text::query::share`]) — PostgreSQL estimates them from a
    /// sample's most common words, the index counts them.  A prefix or a
    /// fuzzy word is left to the sample.
    fn selectivity(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        i: usize,
        function: &str,
        args: &[Value],
    ) -> Option<f64> {
        let (Some(Value::Text(text)), "@@") = (args.first(), function) else {
            return None;
        };
        let a =
            crate::text::named(analyzer_of(&t.indexes[i]).unwrap_or(crate::text::DEFAULT)).ok()?;
        let q = crate::text::query::cached(text, &a).ok()?;
        let Some(root) = &q.root else {
            return Some(0.0);
        };
        let terms: Vec<&str> = q
            .leaves
            .iter()
            .map(|l| match l {
                crate::text::query::Leaf::Exact(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Option<_>>()?;
        let st = store(t, i).view(txn).ok()?.stats(&terms);
        if st.docs == 0 {
            return Some(0.0);
        }
        let docs = st.docs as f64;
        Some(crate::text::query::share(root, &|l| st.df[l] as f64 / docs))
    }

    fn rows(&self, txn: &dyn KvTxn, t: &Table, i: usize) -> Result<Option<usize>> {
        Ok(Some(store(t, i).view(txn)?.stats(&[]).docs as usize))
    }

    /// The rows the condition finds, by BM25 of the same query by the same
    /// analyzer, greatest first — the best scored by MaxScore
    /// ([`store::View::ranked`]), read in slices ([`store::Ranked`]): `k`
    /// over the share expected to pass `keep` first, then as many as the
    /// share passing so far needs (at least as many again), as HNSW's
    /// search grows its `ef`; once `k` passed, on through every row tied
    /// with the `k`-th.  Past the last match it has read them all.  Each
    /// row with its score where asked ([`RankAsk::values`]), so that
    /// `score()` does not analyze its text again (§M14.21 T5c).
    fn rank(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        i: usize,
        ask: &mut RankAsk,
        wanted: Option<&[bool]>,
        mut keep: Option<Keep>,
    ) -> Result<Option<Vec<(Vec<u8>, Row)>>> {
        let Some(("@@", args)) = ask.served else {
            return Ok(None);
        };
        let text = match args.first() {
            Some(Value::Text(q)) => q,
            Some(Value::Null) => return Ok(Some(Vec::new())),
            _ => return Ok(None),
        };
        let r = ask.rank;
        let [Value::Text(rq), ra, Value::Float(k1), Value::Float(b), st] = r.args.as_slice() else {
            return Ok(None);
        };
        if r.function != crate::text::score::TEXT_SCORE || rq != text || args.get(1) != Some(ra) {
            return Ok(None);
        }
        let a = crate::text::named(analyzer_of(&t.indexes[i]).unwrap_or(crate::text::DEFAULT))?;
        let q = crate::text::query::cached(text, &a)?;
        let st = crate::text::score::Stats::of(st)?;
        if st.df.len() != q.leaves.len() {
            return Ok(None);
        }
        let k = r.k;
        if k == 0 {
            return Ok(Some(Vec::new()));
        }
        let p = crate::text::score::Params { k1: *k1, b: *b };
        let sc = crate::text::score::Scorer::new(&q, &st, p);
        let view = store(t, i).view(txn)?;
        // No column read, nothing to check (`count(*)`): the rows need not
        // be read.
        let read = keep.is_some() || wanted.is_none_or(|w| w.contains(&true));
        let storage = crate::storage::of(t)?;
        let mut key = t.index_prefix(0);
        let base = key.len();
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let (mut checked, mut passed) = (0usize, 0usize);
        let mut kth: Option<f64> = None;
        let mut passes = 1;
        let mut slice = ((k as f64 / ask.share.clamp(1e-9, 1.0)).ceil() as usize).max(k);
        // The best `n` scored (MaxScore), the rest left below their
        // threshold: `k` where every row passes, else four times the rows
        // the share expected to pass takes (so that a second pass is
        // seldom due).  Every match scored where `n` is a 64th of them or
        // more — about: no more than the documents or the leaves'
        // documents together — as a threshold that low skips less than
        // its bookkeeping costs (measured on BEIR).  Where the first `n`
        // run out before `k` passed, the share expected was wrong: the
        // second pass takes four times the rows the share seen so far
        // needs (one more passing and read, so that none passing is not a
        // share of nothing), by the same rule; a third scores every match.
        // Never more than two passes and a full scoring.
        let matches = st.df.iter().sum::<f64>().min(st.docs);
        let whole = |n: usize| {
            if n as f64 * WHOLE >= matches {
                usize::MAX
            } else {
                n
            }
        };
        let mut n = whole(if keep.is_some() {
            slice.saturating_mul(4)
        } else {
            k
        });
        let mut ranked = view.ranked(&q, &sc, n)?;
        'read: loop {
            if ranked.left() == 0 {
                if ranked.complete() || kth.is_some() {
                    break;
                }
                n = if n == usize::MAX || passes == 2 {
                    usize::MAX
                } else {
                    let rest = (k - passed) as f64 * (checked + 1) as f64 / (passed + 1) as f64;
                    whole(checked.saturating_add((4.0 * rest).ceil() as usize))
                };
                passes += 1;
                ranked = view.ranked(&q, &sc, n)?;
                continue;
            }
            for (pk, s) in ranked.next(slice) {
                if kth.is_some_and(|x| s < x) {
                    break 'read;
                }
                if !seen.insert(pk.to_vec()) {
                    continue;
                }
                let mut row = if read {
                    crate::budget::charge_scan()?;
                    key.truncate(base);
                    key.extend_from_slice(pk);
                    storage
                        .get(txn, t, &key, wanted, &mut crate::enums::Labels::default())?
                        .ok_or_else(|| {
                            Error::internal("an index method found a row its table does not have")
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
                if ask.values {
                    row.push(Value::Float(s));
                }
                out.push((pk.to_vec(), row));
                if passed == k {
                    kth = Some(s);
                }
            }
            // The rows the share passing so far takes for the rest to pass.
            let needed = match (kth, passed) {
                (Some(_), _) => k,
                (None, 0) => 2 * checked,
                (None, _) => ((k - passed) as f64 * checked as f64 / passed as f64).ceil() as usize,
            };
            slice = needed.max(slice);
        }
        Ok(Some(out))
    }

    fn rows_where(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        i: usize,
        function: &str,
        args: &[Value],
    ) -> Result<Option<Vec<Vec<u8>>>> {
        if function != "@@" {
            return Ok(None);
        }
        let text = match args.first() {
            Some(Value::Text(q)) => q,
            Some(Value::Null) => return Ok(Some(Vec::new())),
            _ => return Ok(None),
        };
        let a = crate::text::named(analyzer_of(&t.indexes[i]).unwrap_or(crate::text::DEFAULT))?;
        let q = crate::text::query::cached(text, &a)?;
        Ok(Some(store(t, i).view(txn)?.matching(&q)?))
    }
}

/// The statistics BM25 reads, of index `i` of `t`, for `terms`, as `txn`
/// sees its rows: documents, their total length, each term's documents.
/// A partitioned table's are its partitions' together — each one's index
/// of the same definition (§8.16: made with the table's) —, so a row
/// scores as it would in one table, whichever partition holds it; `only`,
/// those of the partitions named (the ones row security lets a statement
/// read: a tenant's scores do not depend on other tenants' rows).
pub fn stats(
    txn: &dyn KvTxn,
    t: &Table,
    i: usize,
    terms: &[&str],
    only: Option<&[String]>,
) -> Result<(u64, u64, Vec<u64>)> {
    if t.partition_by.is_none() {
        let st = store(t, i).view(txn)?.stats(terms);
        return Ok((st.docs, st.len, st.df));
    }
    let ix = &t.indexes[i];
    let alike = |px: &crate::catalog::Index| {
        px.method == ix.method
            && px.columns == ix.columns
            && px.opclasses == ix.opclasses
            && px.options == ix.options
    };
    let (mut docs, mut len, mut df) = (0, 0, vec![0; terms.len()]);
    for p in crate::partition::partitions(txn, t)? {
        if only.is_some_and(|o| !o.contains(&p.name)) {
            continue;
        }
        // At the table's place, unless the partition has indexes of its own
        // before it.
        let j = match p.indexes.get(i) {
            Some(px) if alike(px) => i,
            _ => p.indexes.iter().position(alike).ok_or_else(|| {
                Error::internal(format!("partition {} without index {}", p.name, ix.name))
            })?,
        };
        let (d, l, f) = stats(txn, &p, j, terms, None)?;
        docs += d;
        len += l;
        for (a, b) in df.iter_mut().zip(f) {
            *a += b;
        }
    }
    Ok((docs, len, df))
}

/// Index `id`'s table and its position there, as `txn` sees the catalog.
fn find(txn: &dyn KvTxn, id: &Id) -> Result<Option<(Table, usize)>> {
    Ok(crate::catalog::tables(txn)?.into_iter().find_map(|t| {
        let i = t.indexes.iter().position(|ix| {
            ix.id == id.index
                && matches!(ix.method.as_deref(), Some(NAME | sparse::NAME | json::NAME))
        })?;
        (t.id == id.table).then_some((t, i))
    }))
}

/// Seals each index whose delta holds [`SEAL`] rows (every index written,
/// with `all`) and merges what is due after.
fn upkeep(kv: &Arc<dyn Kv>, all: bool) -> Result<()> {
    let p = pending();
    if !p.any.load(Relaxed) {
        return Ok(());
    }
    let due: Vec<(Id, u64)> =
        p.by.lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(k, n)| k.instance == kv.instance() && (all || **n >= SEAL))
            .map(|(k, n)| (k.clone(), *n))
            .collect();
    if due.is_empty() {
        return Ok(());
    }
    let _busy = if all {
        p.busy.lock().unwrap_or_else(|e| e.into_inner())
    } else {
        match p.busy.try_lock() {
            Ok(b) => b,
            Err(_) => return Ok(()),
        }
    };
    for (id, n) in due {
        let mut queues = None;
        let found = crate::hnsw::retrying(kv, |txn| {
            let Some((t, i)) = find(&*txn, &id)? else {
                return Ok(false);
            };
            let s = store(&t, i);
            s.seal(txn)?;
            queues = Some([s.keys.range(b'd'), s.keys.range(b'x')]);
            Ok(true)
        })?;
        {
            let mut by = p.by.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(m) = by.get_mut(&id) {
                *m = m.saturating_sub(n);
                if *m == 0 {
                    by.remove(&id);
                }
            }
            if by.is_empty() {
                p.any.store(false, Relaxed);
            }
        }
        if !found {
            continue;
        }
        // The moved keys' tombstones go now, or every search's scan of
        // the delta steps over them (RocksDB's queue pattern).
        for (start, end) in queues.iter().flatten() {
            kv.vacuum_range(start, end)?;
        }
        while crate::hnsw::retrying(kv, |txn| match find(&*txn, &id)? {
            Some((t, i)) => store(&t, i).merge(txn),
            None => Ok(false),
        })? {}
    }
    Ok(())
}

/// Every text index written since its upkeep last ran, sealed and merged
/// now whatever the sizes (tests; `VACUUM`).
pub fn upkeep_now(kv: &Arc<dyn Kv>) -> Result<()> {
    upkeep(kv, true)
}

/// Erasure (PLAN.md §M14.24 R2b): index `i` of `t` (a text or a sparse
/// vector index) sealed and its segments with deleted rows rewritten
/// without them, in a transaction of its own.
pub fn scrub(kv: &Arc<dyn Kv>, t: &Table, i: usize) -> Result<()> {
    let _busy = pending().busy.lock().unwrap_or_else(|e| e.into_inner());
    crate::hnsw::retrying(kv, |txn| store(t, i).scrub(txn))?;
    Ok(())
}

/// Index `i` of `t` merged into one segment, its deleted rows dropped
/// (tests).
pub fn merge_all(kv: &Arc<dyn Kv>, t: &Table, i: usize) -> Result<()> {
    crate::hnsw::retrying(kv, |txn| store(t, i).merge_all(txn, 1))
}

/// Segments of index `i` of `t` and rows waiting in its delta, as `txn`
/// sees them (tests).
pub fn shape(txn: &dyn KvTxn, t: &Table, i: usize) -> Result<(usize, usize)> {
    let v = store(t, i).view(txn)?;
    Ok((v.segs.len(), v.waiting()))
}

/// Registers the method, and the sparse vector index and the JSON
/// containment index on the same storage ([`sparse`], [`json`]; the
/// built-in module does).
pub fn register(r: &mut Registry) -> Result<()> {
    r.add_index_method(Arc::new(TextIndex))?;
    sparse::register(r)?;
    json::register(r)
}
