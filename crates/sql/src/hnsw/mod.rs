//! HNSW as an index access method (PLAN.md §M14.12 V4b): `CREATE INDEX …
//! USING hnsw (col opclass) WITH (m = 16, ef_construction = 64)` on a
//! `vector(n)` column — the operator classes pgvector's
//! (`vector_l2_ops`, the default, for `<->`; `vector_cosine_ops` for
//! `<=>`; `vector_ip_ops` for `<#>`; `vector_l1_ops` for `<+>`).  The
//! storage is the measured model (C) ([`store`]); the graph's core is
//! [`graph`].
//!
//! A write puts its row into the index's delta (a delete or an update
//! also a dead key) in the statement's transaction — new keys only, so it
//! never conflicts.  The upkeep runs where PostgreSQL's GIN cleans its
//! pending list (`fastupdate`): after a commit, by the session whose
//! commit finds the index's waiting rows past the seal's size — a seal
//! (the delta moved into a sealed object) and, once enough is sealed, a
//! fold on the machine's threads — each in a transaction of its own; no
//! background thread is needed.  The sizes are economic order quantities
//! of costs measured as the index runs ([`Meter`], as `bench/ann`'s).
//!
//! A `NULL` vector is not indexed, nor (for `vector_cosine_ops`) one of
//! zero length, which has no direction — as pgvector's.

mod cache;
pub mod graph;
pub mod store;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

use nucleus_kv::{Kv, KvTxn, TxnOptions};

use crate::catalog::{Index as IndexDef, Table};
use crate::enums::Labels;
use crate::error::{Error, Result};
use crate::ext::Registry;
use crate::index_method::{IndexMethod, RowRef};
use crate::types::{Row, Value};
use crate::vectors::Metric as SqlMetric;

use graph::{Codec, Metric, Params, Scratch, Space};
pub use store::Pks;
use store::{ByRow, Index, Keys, Shape};

/// The method's name.
pub const NAME: &str = "hnsw";

/// The operator classes: name, the SQL distance it serves, the graph's.
const OPCLASSES: [(&str, SqlMetric, Metric); 4] = [
    ("vector_l2_ops", SqlMetric::L2, Metric::L2),
    ("vector_cosine_ops", SqlMetric::Cosine, Metric::Cosine),
    ("vector_ip_ops", SqlMetric::NegInner, Metric::Ip),
    ("vector_l1_ops", SqlMetric::L1, Metric::L1),
];

/// The most rows a seal and a fold take (the sizes chosen below them).
const SEAL_MOST: f64 = 10_000.0;
const FOLD_MOST: f64 = 10_000.0;
/// A compaction's pages put in transactions of about this many bytes.
const PUT_BYTES: usize = 8 << 20;

/// The HNSW index method.
pub struct Hnsw;

/// An index of `t` (a graph of it, partitioned) as the method sees it.
struct Opened {
    id: Id,
    index: Index,
    /// The vector's column, and the index's (its `PARTITION BY` too).
    column: usize,
    columns: Vec<usize>,
    sql: SqlMetric,
    /// Rows read for each returned ([`rescore_default`]).
    rescore: usize,
    state: Arc<State>,
}

fn option(ix: &IndexDef, name: &str) -> Option<usize> {
    ix.options
        .iter()
        .find(|(k, _)| k == name)
        .and_then(|(_, v)| v.parse().ok())
}

/// How index `ix` holds its vectors (`quantize`).
fn codec_of(ix: &IndexDef) -> Codec {
    ix.options
        .iter()
        .find(|(k, _)| k == "quantize")
        .and_then(|(_, v)| Codec::named(v))
        .unwrap_or(Codec::F32)
}

/// The rows a search reads for each it returns, by default, over vectors
/// held as `codec` holds them — their exact distances, from the rows,
/// make up for the codes' (Qdrant's and Elasticsearch's oversampling).
fn rescore_default(codec: Codec) -> usize {
    match codec {
        Codec::F32 | Codec::Half => 1,
        Codec::Int8 => 2,
        Codec::Binary => 5,
    }
}

fn threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// A graph of index `i` of `t`: empty for an index not partitioned; for
/// one partitioned, `p` and its `PARTITION BY` columns' values (key
/// encoded) — `None` where one is `NULL`: the row is not indexed (no
/// search's `col = value` finds it).
fn part(t: &Table, i: usize, row: &[Value]) -> Option<Vec<u8>> {
    let cols = &t.indexes[i].columns[1..];
    if cols.is_empty() {
        return Some(Vec::new());
    }
    let mut out = vec![b'p'];
    for &c in cols {
        if row[c].is_null() {
            return None;
        }
        crate::encoding::encode_key_value(&mut out, &row[c]);
    }
    Some(out)
}

/// The graph of partition values `values` (each of its column's type), as
/// [`part`] names it — `None` where one is `NULL`.
pub fn part_of(values: &[Value]) -> Option<Vec<u8>> {
    if values.is_empty() {
        return Some(Vec::new());
    }
    let mut out = vec![b'p'];
    for v in values {
        if v.is_null() {
            return None;
        }
        crate::encoding::encode_key_value(&mut out, v);
    }
    Some(out)
}

/// Whether index `ix` has graphs by its `PARTITION BY` columns
/// (`ix.columns[1..]`).
pub fn partitioned(ix: &IndexDef) -> bool {
    ix.columns.len() > 1
}

/// Graph `part` ([`part`]) of index `i` of `t` in database `instance`
/// ([`Kv::instance`]).
fn open(instance: u64, t: &Table, i: usize, part: &[u8]) -> Result<Opened> {
    let ix = &t.indexes[i];
    let column = ix.columns[0];
    let dims = t.columns[column]
        .max_len
        .ok_or_else(|| Error::internal("an hnsw index on a vector without dimensions"))?;
    let (_, sql, metric) = OPCLASSES
        .iter()
        .find(|o| Some(o.0) == ix.opclasses[0].as_deref())
        .copied()
        .ok_or_else(|| Error::internal("an hnsw index without its operator class"))?;
    let id = Id {
        instance,
        table: t.id,
        index: ix.id,
        part: part.to_vec(),
    };
    let codec = codec_of(ix);
    let params = Params {
        m: option(ix, "m").unwrap_or(16),
        ef_construction: option(ix, "ef_construction").unwrap_or(64),
        space: Space { metric, codec },
        slack: 1,
    };
    Ok(Opened {
        index: Index {
            keys: Keys::new([t.index_prefix(ix.id).as_slice(), part].concat()),
            shape: Shape::new(params, codec.width(dims as usize), threads()),
        },
        column,
        columns: ix.columns.clone(),
        sql,
        rescore: option(ix, "rescore").unwrap_or(1).max(1),
        state: state(&id),
        id,
    })
}

impl Opened {
    fn id(&self) -> Id {
        self.id.clone()
    }

    /// A value's form in the graph: its components (normalized for the
    /// cosine distance); `None` for what is not indexed.
    fn stored(&self, v: &Value) -> Result<Option<Vec<u8>>> {
        if v.is_null() {
            return Ok(None);
        }
        let Value::Ext(_, b) = v else {
            return Err(Error::internal("not a vector"));
        };
        let codec = self.index.shape.params.space.codec;
        if self.sql != SqlMetric::Cosine {
            return Ok(Some(codec.encode(b)));
        }
        let norm = nucleus_kernels::dot(b, b).sqrt();
        if norm == 0.0 || !norm.is_finite() {
            return Ok(None);
        }
        let mut out = Vec::with_capacity(b.len());
        for c in b.as_chunks::<4>().0 {
            let x = (f64::from(f32::from_le_bytes(*c)) / norm) as f32;
            out.extend_from_slice(&x.to_le_bytes());
        }
        Ok(Some(codec.encode(&out)))
    }

    /// The row of primary key `pk` (only the index's columns read), if
    /// the table has it.
    fn row(&self, txn: &dyn KvTxn, t: &Table, pk: &[u8]) -> Result<Option<Row>> {
        let mut wanted = vec![false; t.columns.len()];
        for &c in &self.columns {
            wanted[c] = true;
        }
        self.row_with(txn, t, pk, &wanted)
    }

    /// The row of primary key `pk`, the columns `wanted` marks read.
    fn row_with(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        pk: &[u8],
        wanted: &[bool],
    ) -> Result<Option<Row>> {
        let mut key = t.index_prefix(0);
        key.extend_from_slice(pk);
        crate::storage::of(t)?.get(txn, t, &key, Some(wanted), &mut Labels::default())
    }
}

impl IndexMethod for Hnsw {
    fn name(&self) -> &str {
        NAME
    }

    fn define(&self, t: &Table, ix: &mut IndexDef) -> Result<()> {
        // `PARTITION BY (c, …)`: the columns after the vector, a graph per
        // value of them.
        let parts = match ix.options.iter().find(|(k, _)| k == "partition_by") {
            Some((_, v)) => v.parse::<usize>().ok().filter(|&n| n >= 1).ok_or_else(|| {
                Error::invalid_parameter(format!(
                    "invalid value {v} for option \"partition_by\" (a number of columns)"
                ))
            })?,
            None => 0,
        };
        if ix.columns.len() != 1 + parts {
            return Err(Error::feature(
                "an hnsw index has one column (and those of its PARTITION BY)",
            ));
        }
        for (n, &c) in ix.columns.iter().enumerate().skip(1) {
            if ix.columns[..n].contains(&c) {
                return Err(Error::feature(format!(
                    "an hnsw index is partitioned by other columns than its own: \"{}\" twice",
                    t.columns[c].name
                )));
            }
            crate::binder::require(
                Some(t.columns[c].ty),
                &[
                    crate::binder::Need::KeyEncoding,
                    crate::binder::Need::Equality,
                ],
                "an index's PARTITION BY",
            )?;
        }
        if ix.opclasses[1..].iter().any(Option::is_some) {
            return Err(Error::feature(
                "a PARTITION BY column of an hnsw index has no operator class",
            ));
        }
        let col = &t.columns[ix.columns[0]];
        if col.ty != crate::vectors::data_type() {
            return Err(Error::feature(format!(
                "an hnsw index needs a vector column, not {}",
                col.ty.name()
            )));
        }
        if col.max_len.is_none() {
            return Err(Error::invalid_parameter(format!(
                "column \"{}\" does not have dimensions",
                col.name
            )));
        }
        match ix.opclasses[0].as_deref() {
            None => ix.opclasses[0] = Some(OPCLASSES[0].0.to_string()),
            Some(o) if OPCLASSES.iter().any(|c| c.0 == o) => {}
            Some(o) => {
                return Err(Error::undefined_object(format!(
                    "operator class \"{o}\" does not exist for access method \"hnsw\""
                )));
            }
        }
        let mut options = Vec::new();
        let mut codec = Codec::F32;
        for (k, v) in &ix.options {
            let (lo, hi) = match k.as_str() {
                "m" => (2, 100),
                "ef_construction" => (4, 1000),
                "rescore" => (1, 100),
                "partition_by" => continue,
                "quantize" => {
                    codec = Codec::named(&v.to_ascii_lowercase()).ok_or_else(|| {
                        Error::invalid_parameter(format!(
                            "invalid value \"{v}\" for option \"quantize\" (none, half, int8 or binary)"
                        ))
                    })?;
                    continue;
                }
                _ => {
                    return Err(Error::invalid_parameter(format!(
                        "unrecognized parameter \"{k}\""
                    )));
                }
            };
            let n: usize = v
                .parse()
                .ok()
                .filter(|n| (lo..=hi).contains(n))
                .ok_or_else(|| {
                    Error::invalid_parameter(format!(
                        "value {v} out of bounds for option \"{k}\" (from {lo} to {hi})"
                    ))
                })?;
            options.push((k.clone(), n));
        }
        let get = |name: &str, d: usize| options.iter().find(|o| o.0 == name).map_or(d, |o| o.1);
        let (m, efc) = (get("m", 16), get("ef_construction", 64));
        if efc < 2 * m {
            return Err(Error::invalid_parameter(
                "ef_construction must be greater than or equal to 2 * m",
            ));
        }
        ix.options = vec![
            ("m".into(), m.to_string()),
            ("ef_construction".into(), efc.to_string()),
        ];
        if codec != Codec::F32 {
            ix.options.push(("quantize".into(), codec.name().into()));
            let rescore = get("rescore", rescore_default(codec));
            ix.options.push(("rescore".into(), rescore.to_string()));
        } else if options.iter().any(|o| o.0 == "rescore") {
            return Err(Error::invalid_parameter(
                "option \"rescore\" needs a quantized index (\"quantize\")",
            ));
        }
        if parts > 0 {
            ix.options.push(("partition_by".into(), parts.to_string()));
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
        // Each side's graph, the row's key and stored vector.
        let instance = txn.instance();
        let side = |r: Option<RowRef>| -> Result<Option<(Opened, Vec<u8>, Vec<u8>)>> {
            let Some((pk, row)) = r else {
                return Ok(None);
            };
            let Some(p) = part(t, i, row) else {
                return Ok(None);
            };
            let o = open(instance, t, i, &p)?;
            Ok(o.stored(&row[o.column])?.map(|v| (o, pk.to_vec(), v)))
        };
        let (old, new) = (side(old)?, side(new)?);
        if let (Some((a, apk, av)), Some((b, bpk, bv))) = (&old, &new)
            && a.index.keys == b.index.keys
            && (apk, av) == (bpk, bv)
        {
            return Ok(()); // the vector, the row's key and its graph unchanged
        }
        if let Some((o, pk, v)) = &old {
            txn.put(
                &o.index.keys.dead(pk, store::vector_hash(v), store::nonce()),
                &[],
            )?;
            o.state.wrote(1);
            pending().note(o.id());
        }
        if let Some((o, pk, v)) = &new {
            txn.put(&o.index.keys.delta(pk, store::nonce()), v)?;
            o.state.wrote(1);
            pending().note(o.id());
        }
        Ok(())
    }

    fn build(&self, txn: &mut dyn KvTxn, _: &Registry, t: &Table, i: usize) -> Result<()> {
        let mut wanted = vec![false; t.columns.len()];
        for &c in &t.indexes[i].columns {
            wanted[c] = true;
        }
        // The rows by graph.
        let mut parts: std::collections::BTreeMap<Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>> =
            Default::default();
        let mut opened: HashMap<Vec<u8>, Opened> = HashMap::new();
        for r in crate::index_method::rows(&*txn, t, Some(&wanted))? {
            let (pk, row) = r?;
            let Some(p) = part(t, i, &row) else {
                continue;
            };
            if !opened.contains_key(&p) {
                opened.insert(p.clone(), open(txn.instance(), t, i, &p)?);
            }
            let o = &opened[&p];
            if let Some(v) = o.stored(&row[o.column])? {
                parts.entry(p).or_default().push((pk, v));
            }
        }
        if parts.is_empty() && !partitioned(&t.indexes[i]) {
            parts.insert(Vec::new(), Vec::new());
            opened.insert(Vec::new(), open(txn.instance(), t, i, &[])?);
        }
        for (p, rows) in parts {
            let o = opened
                .get_mut(&p)
                .ok_or_else(|| Error::internal("a graph not opened"))?;
            o.index.shape.batch.threads = crate::budget::workers().max(1);
            o.index.build(txn, &rows)?;
        }
        Ok(())
    }

    fn check(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        i: usize,
        rows: &[(Vec<u8>, Row)],
        problems: &mut Vec<String>,
    ) -> Result<()> {
        let r = txn
            .shared()
            .ok_or_else(|| Error::internal("hnsw needs a KV that shares a transaction's reads"))?;
        // Each graph against the rows it should hold.
        let mut parts: std::collections::BTreeMap<Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>> =
            Default::default();
        if !partitioned(&t.indexes[i]) {
            parts.insert(Vec::new(), Vec::new());
        }
        let mut opened: HashMap<Vec<u8>, Opened> = HashMap::new();
        for (pk, row) in rows {
            let Some(p) = part(t, i, row) else {
                continue;
            };
            if !opened.contains_key(&p) {
                opened.insert(p.clone(), open(txn.instance(), t, i, &p)?);
            }
            let o = &opened[&p];
            if let Some(v) = o.stored(&row[o.column])? {
                parts.entry(p).or_default().push((pk.clone(), v));
            }
        }
        for (p, want) in parts {
            let o = match opened.remove(&p) {
                Some(o) => o,
                None => open(txn.instance(), t, i, &p)?,
            };
            o.index.check(r, &t.indexes[i].name, &want, problems)?;
        }
        Ok(())
    }

    fn after_statement(&self, kv: &Arc<dyn Kv>, wrote: bool) -> Result<()> {
        upkeep(kv, wrote)
    }
}

/// `ef_search` when it is not set: pgvector's `hnsw.ef_search`.
pub const EF_SEARCH: usize = 40;

/// The rows of `t` nearest `q` by index `i`'s distance, as `txn` sees
/// them: up to `k`, each the row's primary key bytes, the row (its
/// indexed column read) and its distance from `q`, nearest first — those
/// [`search`] finds with `ef`, at most `ef_most`, every row passing.
pub fn nearest(
    txn: &dyn KvTxn,
    t: &Table,
    i: usize,
    q: &Value,
    k: usize,
    ef: usize,
    ef_most: usize,
) -> Result<Vec<(Vec<u8>, Row, f64)>> {
    let mut wanted = vec![false; t.columns.len()];
    wanted[t.indexes[i].columns[0]] = true;
    let ask = Ask {
        k,
        ef,
        ef_most,
        share: 1.0,
        among: None,
        part: &[],
    };
    let mut found = search(txn, t, i, q, &ask, &wanted, &mut |_| Ok(true))?;
    found.rows.truncate(k);
    Ok(found.rows)
}

/// What a [`search`] asks for.
pub struct Ask<'a> {
    /// The rows wanted (at least, where there are).
    pub k: usize,
    /// The candidates passing the filter the search keeps (`ef_search`:
    /// pgvector's, counted among the rows that pass, as a filtered HNSW
    /// search's result list holds only those — hnswlib's, ACORN's).
    pub ef: usize,
    /// The most candidates the graph is asked for.
    pub ef_most: usize,
    /// The share of the rows expected to pass the filter (the
    /// optimizer's estimate; 1 without one).
    pub share: f64,
    /// Only these rows (primary keys, each once: what a B-tree's range of
    /// the filter holds), searched exactly — or every row the index has.
    pub among: Option<&'a Pks>,
    /// The graph, for an index partitioned ([`part_of`]; empty for one
    /// not).
    pub part: &'a [u8],
}

/// What [`search`] found.
pub struct Found {
    /// The rows that passed: each its primary key bytes, the row and its
    /// distance from the query, nearest first.
    pub rows: Vec<(Vec<u8>, Row, f64)>,
    /// Whether the index was searched through: every row it holds was
    /// looked at (`rows` are then every row that passed, of those the
    /// index holds).
    pub all: bool,
}

/// The rows of `t` nearest `q` by index `i`'s distance that `keep`
/// passes, as `txn` sees them, the columns `wanted` marks read (the
/// indexed one too).  What the index found is checked against the rows
/// (a row gone, or no longer near, passed over; a row found twice, once).
///
/// The graph is asked for `ef` candidates over the share expected to
/// pass, so about `ef` of them pass; where fewer than `k` pass, or the
/// share passing near the query turns out under half the expected (a
/// filter that goes with the vectors), it is asked again for as many as
/// that share needs — pgvector 0.8's iterative scans, sized by what was
/// seen — up to `ef_most` (raised to `k`) or until the index has no
/// more.  Among given rows the search is exact: their vectors from the
/// index, read from memory.  More than `k` may come back; a query the
/// index does not hold (`NULL`; for the cosine distance a zero vector)
/// finds nothing.
pub fn search(
    txn: &dyn KvTxn,
    t: &Table,
    i: usize,
    q: &Value,
    ask: &Ask,
    wanted: &[bool],
    keep: &mut dyn FnMut(&Row) -> Result<bool>,
) -> Result<Found> {
    let o = open(txn.instance(), t, i, ask.part)?;
    let dims = t.columns[o.column].max_len.unwrap_or(0) as usize;
    if let Value::Ext(_, b) = q
        && b.len() != 4 * dims
    {
        return Err(Error::data_exception(format!(
            "different vector dimensions {dims} and {}",
            b.len() / 4
        )));
    }
    let Some(qs) = o.stored(q)? else {
        return Ok(Found {
            rows: Vec::new(),
            all: false,
        });
    };
    let r = txn
        .shared()
        .ok_or_else(|| Error::internal("hnsw needs a KV that shares a transaction's reads"))?;
    let mut wanted = wanted.to_vec();
    wanted[o.column] = true;
    let mut s = Scratch::default();
    let k = ask.k;
    // Rows read (their exact distances) for the `k` returned, over codes.
    let reads = k * o.rescore;
    let wants = ask.ef.max(reads).max(1);
    let ef_most = ask.ef_most.max(reads).max(1);
    let mut ef = match ask.among {
        Some(_) => wants,
        None => ((wants as f64 / ask.share.clamp(1e-9, 1.0)).ceil() as usize).clamp(1, ef_most),
    };
    let started = std::time::Instant::now();
    let metric = o.index.shape.params.space;
    let mut seen = std::collections::HashSet::new();
    // The rows that passed, each with its distance by the graph.
    let mut rows: Vec<(Vec<u8>, Row, f64, f32)> = Vec::new();
    let (mut checked, mut passed) = (0usize, 0usize);
    loop {
        let found = match ask.among {
            None => o.index.candidates(r, &qs, ef, &mut s)?,
            Some(pks) => match o.index.candidates_among(
                r,
                &qs,
                pks,
                ef,
                &o.state.by_row,
                crate::budget::workers().min(pks.len() / AMONG_PART),
            )? {
                Some(found) => found,
                // The map of rows to nodes is another graph's: searched
                // through the graph instead.
                None => {
                    let ask = Ask {
                        among: None,
                        ..*ask
                    };
                    return search(txn, t, i, q, &ask, &wanted, keep);
                }
            },
        };
        o.state.scanned(found.sealed, found.delta);
        // Nearest first by the graph's distance (the SQL operator's
        // order): the first `k` that pass are the answer, so the rest
        // are not read.  A row whose vector is no longer the one found
        // (its distance differs) is passed over: its vector now is in
        // the index too, under its own distance.
        for (d, pk) in found.near {
            if rows.iter().filter(|r| r.3 <= d).count() >= reads {
                break;
            }
            if seen.contains(&pk) {
                continue;
            }
            crate::budget::charge_scan()?;
            let Some(row) = o.row_with(txn, t, &pk, &wanted)? else {
                seen.insert(pk);
                continue;
            };
            let now = o.stored(&row[o.column])?;
            if now.is_none_or(|v| metric.distance(&qs, &v).to_bits() != d.to_bits()) {
                continue;
            }
            seen.insert(pk.clone());
            checked += 1;
            if !keep(&row)? {
                continue;
            }
            passed += 1;
            let sql = o.sql.of(&row[o.column], q)?;
            rows.push((pk, row, sql, d));
        }
        // The candidates the share passing here takes for `wants` of them
        // to pass.
        let needed = if passed == 0 {
            ef_most
        } else {
            (wants as f64 * checked as f64 / passed as f64).ceil() as usize
        };
        let short = rows.len() < k || (ask.among.is_none() && needed > 2 * ef);
        if !short || found.all || ef >= ef_most {
            rows.sort_by(|a, b| a.2.total_cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
            o.state.searched(started.elapsed().as_nanos() as u64);
            return Ok(Found {
                rows: rows
                    .into_iter()
                    .map(|(pk, row, d, _)| (pk, row, d))
                    .collect(),
                all: found.all,
            });
        }
        ef = needed.max(2 * ef).min(ef_most);
    }
}

/// The index method `ix` is searched by, if it is an HNSW index: the
/// SQL distance its operator class serves, its `m`, and the rows a search
/// reads for each it returns (`rescore`).
pub fn serves(ix: &IndexDef) -> Option<(SqlMetric, usize, usize)> {
    if ix.method.as_deref() != Some(NAME) {
        return None;
    }
    let class = ix.opclasses.first()?.as_deref()?;
    let (_, sql, _) = OPCLASSES.iter().find(|o| o.0 == class)?;
    Some((
        *sql,
        option(ix, "m").unwrap_or(16),
        option(ix, "rescore").unwrap_or(1).max(1),
    ))
}

/// The search behind `ORDER BY col <op> q LIMIT k` ([`Access::Nearest`]
/// (crate::plan::Access::Nearest)): at least `k` rows passing `keep`,
/// nearest first, or `None` where the index cannot tell them — it holds
/// fewer, or finding them would take a search as costly as reading every
/// row: about `ef · 2m` distances against the rows' (each node's
/// neighbours at its bottom layer), so `ef` up to the rows over `2m`.
/// `ef` is the session's `hnsw.ef_search`; `share`, `among` and `part`
/// as [`Ask`]'s.
#[allow(clippy::too_many_arguments)]
pub fn nearest_rows(
    txn: &dyn KvTxn,
    t: &Table,
    i: usize,
    q: &Value,
    k: usize,
    share: f64,
    among: Option<&Pks>,
    part: &[u8],
    wanted: Option<&[bool]>,
    keep: &mut dyn FnMut(&Row) -> Result<bool>,
) -> Result<Option<Vec<(Vec<u8>, Row)>>> {
    let ef = ef_search();
    let m = option(&t.indexes[i], "m").unwrap_or(16);
    let ef_most = (rows(txn, t, i, part)? / (2 * m)).max(ef);
    let all = vec![true; t.columns.len()];
    let ask = Ask {
        k,
        ef,
        ef_most,
        share,
        among,
        part,
    };
    let found = search(txn, t, i, q, &ask, wanted.unwrap_or(&all), keep)?;
    if found.rows.len() < k {
        return Ok(None);
    }
    Ok(Some(
        found
            .rows
            .into_iter()
            .map(|(pk, row, _)| (pk, row))
            .collect(),
    ))
}

/// The fewest rows a thread of a search among given rows takes: what
/// starting a thread costs (~10 µs) against finding a row's vector
/// (~100 ns, a few cache misses).
const AMONG_PART: usize = 2048;

/// What reading a row costs, and what the graph's search costs for each
/// candidate of its `ef`, against reading a B-tree entry and finding its
/// row's vector in the index (~3.4 µs and ~3.7 µs against ~0.17):
/// measured on SIFT-1M (`bench/ann/filtered.py`).
const ROW_PER_ENTRY: f64 = 20.0;
const EF_PER_ENTRY: f64 = 22.0;

/// The most rows of a filter's B-tree range searched among, of `rows`
/// the graph holds, for `k` at `ef`: where reading their entries (`p` of
/// them) costs what the graph's search does — it is asked for `ef` over
/// the share passing and reads about `k` over it, the share `p / rows` —
/// so `p` up to √(rows · (ef · a candidate's cost + k · a row's) over an
/// entry's).
pub fn among_most(k: usize, ef: usize, rows: usize) -> usize {
    (rows.max(1) as f64 * (EF_PER_ENTRY * ef.max(k) as f64 + ROW_PER_ENTRY * k.max(1) as f64))
        .sqrt() as usize
}

/// The rows graph `part` ([`part_of`]) of index `i` of `t` holds as `txn`
/// sees it — its graph's and sealed objects' (not the delta's, at most a
/// seal's size).
pub fn rows(txn: &dyn KvTxn, t: &Table, i: usize, part: &[u8]) -> Result<usize> {
    let o = open(txn.instance(), t, i, part)?;
    let r = txn
        .shared()
        .ok_or_else(|| Error::internal("hnsw needs a KV that shares a transaction's reads"))?;
    Ok(o.index.root(r)?.rows())
}

/// The session's `hnsw.ef_search` ([`EF_SEARCH`] when not set).
pub fn ef_search() -> usize {
    crate::expr::setting(EF_SEARCH_SETTING)
        .and_then(|v| v.trim().trim_matches('\'').parse().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(EF_SEARCH)
}

/// The setting's name.
pub const EF_SEARCH_SETTING: &str = "hnsw.ef_search";

/// Checks a value for `hnsw.ef_search`: 1 to 1000, as pgvector's.
pub fn check_ef_search(v: &str) -> Result<()> {
    match v.trim().trim_matches('\'').parse::<usize>() {
        Ok(n) if (1..=1000).contains(&n) => Ok(()),
        _ => Err(Error::invalid_parameter(format!(
            "invalid value for parameter \"{EF_SEARCH_SETTING}\": \"{v}\" (1 to 1000)"
        ))),
    }
}

/// What index `i` of `t` (not partitioned) holds as `txn` sees it.
pub fn status(txn: &dyn KvTxn, t: &Table, i: usize) -> Result<store::Status> {
    let o = open(txn.instance(), t, i, &[])?;
    let r = txn
        .shared()
        .ok_or_else(|| Error::internal("hnsw needs a KV that shares a transaction's reads"))?;
    o.index.status(r)
}

// ---------------------------------------------------------------- upkeep

/// What the upkeep's sizes weigh, measured as an index runs (`bench/ann`'s
/// meter): how fast rows come and searches ask, what a search costs, what
/// a fold costs beyond its rows (a line fitted to folds' times against
/// their rows, decayed), what a seal costs.
#[derive(Default)]
struct State {
    writes: AtomicU64,
    searches: (AtomicU64, AtomicU64),
    rates: Mutex<Rates>,
    /// Held by the session doing the upkeep.
    busy: Mutex<()>,
    /// Each row's nodes in the graph (the folds' marks, searches among
    /// given rows).
    by_row: std::sync::RwLock<ByRow>,
    /// Held by the session compacting (apart from `busy`: the folds go on).
    compacting: Mutex<()>,
    /// Whether the last fold left the patches past their share.
    compact_due: AtomicBool,
    /// What a waiting row costs a search, ns, measured by the searches:
    /// sealed (from memory), in the delta (from the KV).
    row_ns: Mutex<(f64, f64)>,
}

#[derive(Default)]
struct Rates {
    at: Option<std::time::Instant>,
    seen: (u64, u64),
    write_rate: f64,
    search_rate: f64,
    /// Folds' rows against their ns (n, Σx, Σy, Σxy, Σx²), decayed.
    fit: [f64; 5],
    seal_ns: f64,
    nudge: bool,
}

/// An economic order quantity (Harris 1913): the batch that makes a fixed
/// cost `k` a batch, against a cost `h` a second for each item waiting,
/// cheapest at `rate` items a second — √(2·k·rate/h).
fn eoq(k: f64, rate: f64, h: f64) -> f64 {
    if h <= 0.0 {
        f64::INFINITY
    } else {
        (2.0 * k * rate / h).sqrt()
    }
}

impl State {
    /// Whether rows are coming in (a row a second or more, lately): the
    /// writers then keep the index up, the searches do not wait on it.
    fn writing(&self) -> bool {
        self.rates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_rate
            >= 1.0
    }

    fn wrote(&self, n: u64) {
        self.writes.fetch_add(n, Relaxed);
    }

    /// A search scanned `sealed` and `delta` rows (each how many, and the
    /// ns it took): their costs a row, averaged.
    fn scanned(&self, sealed: (usize, u64), delta: (usize, u64)) {
        let mut c = self.row_ns.lock().unwrap_or_else(|e| e.into_inner());
        let (a, b) = &mut *c;
        for ((rows, ns), at) in [(sealed, a), (delta, b)] {
            if rows >= 16 {
                let x = ns as f64 / rows as f64;
                *at = if *at == 0.0 {
                    x
                } else {
                    *at + 0.05 * (x - *at)
                };
            }
        }
    }

    fn searched(&self, ns: u64) {
        self.searches.0.fetch_add(1, Relaxed);
        self.searches.1.fetch_add(ns, Relaxed);
    }

    /// Samples the rates (an exponential average, a sample a tenth of a
    /// second at most).
    fn sample(&self) {
        let mut r = self.rates.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        let (w, s) = (self.writes.load(Relaxed), self.searches.0.load(Relaxed));
        let Some(at) = r.at else {
            r.at = Some(now);
            r.seen = (w, s);
            return;
        };
        let dt = now.duration_since(at).as_secs_f64();
        if dt < 0.1 {
            return;
        }
        let (wr, sr) = ((w - r.seen.0) as f64 / dt, (s - r.seen.1) as f64 / dt);
        let first = r.write_rate == 0.0 && r.search_rate == 0.0;
        let a = if first { 1.0 } else { 0.3 };
        r.write_rate += a * (wr - r.write_rate);
        r.search_rate += a * (sr - r.search_rate);
        r.at = Some(now);
        r.seen = (w, s);
    }

    fn folded(&self, rows: usize, ns: f64) {
        let mut r = self.rates.lock().unwrap_or_else(|e| e.into_inner());
        let x = rows as f64;
        for (f, v) in r.fit.iter_mut().zip([1.0, x, ns, x * ns, x * x]) {
            *f = 0.8 * *f + v;
        }
    }

    fn sealed(&self, ns: f64) {
        let mut r = self.rates.lock().unwrap_or_else(|e| e.into_inner());
        r.seal_ns = if r.seal_ns == 0.0 {
            ns
        } else {
            r.seal_ns + 0.3 * (ns - r.seal_ns)
        };
    }

    /// The seal's and the fold's sizes now: a waiting row costs each
    /// search what reading it does — from the KV before a seal, from
    /// memory after — as the searches measure it (until they have: ~470
    /// and ~160 ns, measured on `bench/ann`).
    fn sizes(&self) -> (usize, usize) {
        let (mem, kv) = *self.row_ns.lock().unwrap_or_else(|e| e.into_inner());
        let mem_ns = if mem > 0.0 { mem } else { 160.0 };
        let kv_ns = if kv > 0.0 { kv } else { 470.0 }.max(mem_ns);
        let mut r = self.rates.lock().unwrap_or_else(|e| e.into_inner());
        let [n, sx, sy, sxy, sxx] = r.fit;
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
        r.nudge = !r.nudge;
        let fold = eoq(k, r.write_rate, r.search_rate * mem_ns).clamp(64.0, FOLD_MOST);
        let fold = if r.nudge { fold * 1.2 } else { fold * 0.8 };
        let seal = eoq(
            r.seal_ns.max(1e5),
            r.write_rate,
            r.search_rate * (kv_ns - mem_ns),
        )
        .clamp(16.0, SEAL_MOST.min(fold));
        (seal as usize, fold as usize)
    }
}

/// A graph of an index: its database's [`Kv::instance`], its table's id,
/// its id, and its partition ([`part`]; empty for an index not
/// partitioned).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Id {
    instance: u64,
    table: u32,
    index: u32,
    part: Vec<u8>,
}

fn state(id: &Id) -> Arc<State> {
    static STATES: OnceLock<Mutex<HashMap<Id, Arc<State>>>> = OnceLock::new();
    let mut states = STATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match states.get(id) {
        Some(s) => s.clone(),
        None => states.entry(id.clone()).or_default().clone(),
    }
}

/// What waits in an index for its upkeep: the keys written since its
/// last seal (about: rolled back writes count too; the upkeep reads the
/// truth), and the rows its sealed objects hold.
#[derive(Clone, Copy, Default)]
struct Waiting {
    delta: u64,
    sealed: u64,
}

/// The indexes with rows waiting.
struct Pending {
    any: AtomicBool,
    by: Mutex<HashMap<Id, Waiting>>,
}

fn pending() -> &'static Pending {
    static P: OnceLock<Pending> = OnceLock::new();
    P.get_or_init(|| Pending {
        any: AtomicBool::new(false),
        by: Mutex::new(HashMap::new()),
    })
}

impl Pending {
    /// Index `id`'s upkeep ran on the `delta` keys seen before it: what
    /// was written since waits, and `sealed` rows are left sealed.
    fn done(&self, id: &Id, delta: u64, sealed: u64) {
        let mut by = self.by.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(w) = by.get_mut(id) {
            w.delta = w.delta.saturating_sub(delta);
            w.sealed = sealed;
            if w.delta == 0 && w.sealed == 0 {
                by.remove(id);
            }
        }
        self.any.store(!by.is_empty(), Relaxed);
    }

    fn note(&self, id: Id) {
        let mut by = self.by.lock().unwrap_or_else(|e| e.into_inner());
        match by.get_mut(&id) {
            Some(w) => w.delta += 1,
            None => {
                by.insert(
                    id,
                    Waiting {
                        delta: 1,
                        sealed: 0,
                    },
                );
            }
        }
        drop(by);
        self.any.store(true, Relaxed);
    }
}

/// After a statement: each index whose delta passed its seal's size
/// sealed, folded once its sealed rows pass the fold's, and compacted once
/// a fold leaves its patches past their share — by this session, unless
/// another is at it, and while rows come in only by a session that wrote
/// (PostgreSQL's GIN pending list is cleaned by its inserters): the
/// searches then never wait on it.  The sizes fall as the writes stop (an
/// economic order quantity of a rate going to nothing), so the rows left
/// waiting are folded by the searches that follow.
fn upkeep(kv: &Arc<dyn Kv>, wrote: bool) -> Result<()> {
    let p = pending();
    if !p.any.load(Relaxed) {
        return Ok(());
    }
    let due: Vec<(Id, Waiting)> = {
        let by = p.by.lock().unwrap_or_else(|e| e.into_inner());
        by.iter()
            .filter(|(k, _)| k.instance == kv.instance())
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    };
    for (id, w) in due {
        let st = state(&id);
        st.sample();
        if !wrote && st.writing() {
            continue;
        }
        let (seal, fold) = st.sizes();
        let fold_due = w.sealed > 0 && w.sealed + w.delta >= fold as u64;
        let compact_due = st.compact_due.load(Relaxed);
        if (w.delta as usize) < seal && !fold_due && !compact_due {
            continue;
        }
        if (w.delta as usize) >= seal || fold_due {
            let Ok(_busy) = st.busy.try_lock() else {
                continue;
            };
            match maintain(kv, &st, &id, fold)? {
                Some(sealed) => p.done(&id, w.delta, sealed),
                None => {
                    p.done(&id, u64::MAX, 0);
                    continue;
                }
            }
        }
        if st.compact_due.load(Relaxed) {
            compact(kv, &st, &id)?;
        }
    }
    Ok(())
}

/// Every index written since its upkeep last ran, sealed, folded and
/// compacted now whatever the sizes (tests; `VACUUM`).
pub fn upkeep_now(kv: &Arc<dyn Kv>) -> Result<()> {
    let p = pending();
    let due: Vec<(Id, Waiting)> =
        p.by.lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(k, _)| k.instance == kv.instance())
            .map(|(k, v)| (k.clone(), *v))
            .collect();
    for (id, w) in due {
        let st = state(&id);
        let r = {
            let _busy = st.busy.lock().unwrap_or_else(|e| e.into_inner());
            maintain(kv, &st, &id, 0)?
        };
        match r {
            Some(sealed) => p.done(&id, w.delta, sealed),
            None => {
                p.done(&id, u64::MAX, 0);
                continue;
            }
        }
        if st.compact_due.load(Relaxed) {
            compact(kv, &st, &id)?;
        }
    }
    Ok(())
}

/// Index `id`'s table and its position there, as `txn` sees the catalog
/// (`None`: gone).
fn find(txn: &dyn KvTxn, id: &Id) -> Result<Option<(Table, usize)>> {
    Ok(crate::catalog::tables(txn)?.into_iter().find_map(|t| {
        let i = t
            .indexes
            .iter()
            .position(|ix| ix.id == id.index && ix.method.as_deref() == Some(NAME))?;
        (t.id == id.table).then_some((t, i))
    }))
}

/// Seals index `id` and folds it once `fold` rows are sealed; the rows
/// left sealed, or `None` where the index is gone.  Notes whether a
/// compaction is due.
fn maintain(kv: &Arc<dyn Kv>, st: &State, id: &Id, fold: usize) -> Result<Option<u64>> {
    let instance = id.instance;
    let started = std::time::Instant::now();
    let mut sealed_rows = 0u64;
    let mut moved = 0;
    let mut queues = None;
    let found = retrying(kv, |txn| {
        let Some((t, i)) = find(&*txn, id)? else {
            return Ok(false);
        };
        let o = open(instance, &t, i, &id.part)?;
        moved = o.index.seal(txn)?;
        let r = txn
            .shared()
            .ok_or_else(|| Error::internal("hnsw needs a KV that shares a transaction's reads"))?;
        sealed_rows = o.index.root(r)?.sealed_rows();
        queues = Some(o.index.keys.queues());
        Ok(true)
    })?;
    if !found {
        return Ok(None);
    }
    // The keys a seal moved are deleted, never written again: their
    // tombstones go now, or every search's scan of the delta steps over
    // all the seals' so far (RocksDB's queue pattern).
    if moved > 0 {
        for (start, end) in queues.iter().flatten() {
            kv.vacuum_range(start, end)?;
        }
    }
    st.sealed(started.elapsed().as_nanos() as f64);
    if sealed_rows == 0 || (sealed_rows as usize) < fold {
        return Ok(Some(sealed_rows));
    }
    let started = std::time::Instant::now();
    let mut rows = 0;
    let mut due = false;
    retrying(kv, |txn| {
        let Some((t, i)) = find(&*txn, id)? else {
            return Ok(());
        };
        let o = open(instance, &t, i, &id.part)?;
        // A row now in another graph (its partition's columns changed)
        // has no vector in this one.
        let current = |ro: &dyn KvTxn, pk: &[u8]| -> Result<Option<Vec<u8>>> {
            match o.row(ro, &t, pk)? {
                Some(row) if part(&t, i, &row).as_deref() == Some(id.part.as_slice()) => {
                    o.stored(&row[o.column])
                }
                _ => Ok(None),
            }
        };
        rows = o.index.fold(txn, &st.by_row, &current)?;
        let r = txn
            .shared()
            .ok_or_else(|| Error::internal("hnsw needs a KV that shares a transaction's reads"))?;
        due = o.index.compaction_due(r)?;
        Ok(())
    })?;
    st.compact_due.store(due, Relaxed);
    if rows > 0 {
        st.folded(rows, started.elapsed().as_nanos() as f64);
    }
    Ok(Some(0))
}

/// Compacts index `id` — its patches rewritten into pages from a snapshot
/// while the folds go on, then swapped in, between two folds — unless
/// another session is at it; the graph read into memory after.
fn compact(kv: &Arc<dyn Kv>, st: &State, id: &Id) -> Result<()> {
    let Ok(_compacting) = st.compacting.try_lock() else {
        return Ok(());
    };
    let instance = id.instance;
    let made = {
        let txn = kv.begin(TxnOptions::read_only())?;
        let Some((t, i)) = find(&*txn, id)? else {
            return Ok(());
        };
        let o = open(instance, &t, i, &id.part)?;
        let r = txn
            .shared()
            .ok_or_else(|| Error::internal("hnsw needs a KV that shares a transaction's reads"))?;
        o.index.compaction(r)?.map(|c| (c, o))
    };
    let Some((mut c, o)) = made else {
        st.compact_due.store(false, Relaxed);
        return Ok(());
    };
    for b in c.batches(PUT_BYTES) {
        retrying(kv, |txn| o.index.put_compaction(txn, &c, b.clone()))?;
    }
    c.put();
    let installed = {
        let _busy = st.busy.lock().unwrap_or_else(|e| e.into_inner());
        retrying(kv, |txn| match find(&*txn, id)? {
            Some(_) => o.index.install(txn, &c),
            None => Ok(false),
        })?
    };
    if installed {
        st.compact_due.store(false, Relaxed);
        let txn = kv.begin(TxnOptions::read_only())?;
        if let Some(r) = txn.shared() {
            o.index.warm(r)?;
        }
    }
    Ok(())
}

/// `f` in a transaction of its own, committed — run again while the
/// commit conflicts (with another upkeep: user transactions never write
/// the keys it writes).
fn retrying<T>(kv: &Arc<dyn Kv>, mut f: impl FnMut(&mut dyn KvTxn) -> Result<T>) -> Result<T> {
    let mut attempt: u32 = 0;
    loop {
        let mut txn = kv.begin(TxnOptions::default())?;
        let v = f(txn.as_mut())?;
        match txn.commit() {
            Ok(_) => return Ok(v),
            Err(e) if e.is_retryable() && attempt < 100 => {
                attempt += 1;
                kv.back_off(attempt);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Registers the method (the built-in module does).
pub fn register(r: &mut Registry) -> Result<()> {
    r.add_index_method(Arc::new(Hnsw))
}
