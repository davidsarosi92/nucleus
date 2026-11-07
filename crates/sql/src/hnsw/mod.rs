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

use graph::{Metric, Params, Scratch};
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

/// An index of `t` as the method sees it.
struct Opened {
    index: Index,
    column: usize,
    sql: SqlMetric,
    state: Arc<State>,
}

fn option(ix: &IndexDef, name: &str) -> Option<usize> {
    ix.options
        .iter()
        .find(|(k, _)| k == name)
        .and_then(|(_, v)| v.parse().ok())
}

fn threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Index `i` of `t` in database `instance` ([`Kv::instance`]).
fn open(instance: u64, t: &Table, i: usize) -> Result<Opened> {
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
    let params = Params {
        m: option(ix, "m").unwrap_or(16),
        ef_construction: option(ix, "ef_construction").unwrap_or(64),
        metric,
        slack: 1,
    };
    Ok(Opened {
        index: Index {
            keys: Keys::new(t.index_prefix(ix.id)),
            shape: Shape::new(params, dims as usize * 4, threads()),
        },
        column,
        sql,
        state: state((instance, t.id, ix.id)),
    })
}

impl Opened {
    /// A value's form in the graph: its components (normalized for the
    /// cosine distance); `None` for what is not indexed.
    fn stored(&self, v: &Value) -> Result<Option<Vec<u8>>> {
        if v.is_null() {
            return Ok(None);
        }
        let Value::Ext(_, b) = v else {
            return Err(Error::internal("not a vector"));
        };
        if self.sql != SqlMetric::Cosine {
            return Ok(Some(b.to_vec()));
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
        Ok(Some(out))
    }

    /// The row of primary key `pk` (only the indexed column read), if the
    /// table has it.
    fn row(&self, txn: &dyn KvTxn, t: &Table, pk: &[u8]) -> Result<Option<Row>> {
        let mut wanted = vec![false; t.columns.len()];
        wanted[self.column] = true;
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
        if ix.columns.len() != 1 {
            return Err(Error::feature("an hnsw index has one column"));
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
        for (k, v) in &ix.options {
            let (lo, hi) = match k.as_str() {
                "m" => (2, 100),
                "ef_construction" => (4, 1000),
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
        let o = open(txn.instance(), t, i)?;
        let old = match old {
            Some((pk, row)) => o.stored(&row[o.column])?.map(|v| (pk, v)),
            None => None,
        };
        let new = match new {
            Some((pk, row)) => o.stored(&row[o.column])?.map(|v| (pk, v)),
            None => None,
        };
        if old.is_some() && old == new {
            return Ok(()); // the vector and the row's key unchanged
        }
        let mut n = 0;
        if let Some((pk, v)) = &old {
            txn.put(
                &o.index.keys.dead(pk, store::vector_hash(v), store::nonce()),
                &[],
            )?;
            n += 1;
        }
        if let Some((pk, v)) = &new {
            txn.put(&o.index.keys.delta(pk, store::nonce()), v)?;
            n += 1;
        }
        if n > 0 {
            o.state.wrote(n);
            pending().note((txn.instance(), t.id, t.indexes[i].id));
        }
        Ok(())
    }

    fn build(&self, txn: &mut dyn KvTxn, _: &Registry, t: &Table, i: usize) -> Result<()> {
        let mut o = open(txn.instance(), t, i)?;
        o.index.shape.batch.threads = crate::budget::workers().max(1);
        let mut wanted = vec![false; t.columns.len()];
        wanted[o.column] = true;
        let mut rows = Vec::new();
        for r in crate::index_method::rows(&*txn, t, Some(&wanted))? {
            let (pk, row) = r?;
            if let Some(v) = o.stored(&row[o.column])? {
                rows.push((pk, v));
            }
        }
        o.index.build(txn, &rows)
    }

    fn check(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        i: usize,
        rows: &[(Vec<u8>, Row)],
        problems: &mut Vec<String>,
    ) -> Result<()> {
        let o = open(txn.instance(), t, i)?;
        let mut want = Vec::with_capacity(rows.len());
        for (pk, row) in rows {
            if let Some(v) = o.stored(&row[o.column])? {
                want.push((pk.clone(), v));
            }
        }
        let r = txn
            .shared()
            .ok_or_else(|| Error::internal("hnsw needs a KV that shares a transaction's reads"))?;
        o.index.check(r, &t.indexes[i].name, &want, problems)
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
    let mut found = search(txn, t, i, q, k, ef, ef_most, &wanted, &mut |_| Ok(true))?;
    found.rows.truncate(k);
    Ok(found.rows)
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
/// indexed one too).  What the graph found is checked against the rows (a
/// row gone, or no longer near, passed over; a row found twice, once);
/// with fewer than `k` passing the graph is asked again with `ef` doubled
/// — pgvector 0.8's iterative scans — up to `ef_most` (raised to `k`) or
/// until the index has no more.  More than `k` may come back; a query the
/// index does not hold (`NULL`; for the cosine distance a zero vector)
/// finds nothing.
#[allow(clippy::too_many_arguments)]
pub fn search(
    txn: &dyn KvTxn,
    t: &Table,
    i: usize,
    q: &Value,
    k: usize,
    ef: usize,
    ef_most: usize,
    wanted: &[bool],
    keep: &mut dyn FnMut(&Row) -> Result<bool>,
) -> Result<Found> {
    let o = open(txn.instance(), t, i)?;
    let Some(qs) = o.stored(q)? else {
        return Ok(Found {
            rows: Vec::new(),
            all: false,
        });
    };
    if qs.len() != o.index.shape.width {
        return Err(Error::data_exception(format!(
            "different vector dimensions {} and {}",
            o.index.shape.width / 4,
            qs.len() / 4
        )));
    }
    let r = txn
        .shared()
        .ok_or_else(|| Error::internal("hnsw needs a KV that shares a transaction's reads"))?;
    let mut wanted = wanted.to_vec();
    wanted[o.column] = true;
    let mut s = Scratch::default();
    let ef_most = ef_most.max(k);
    let mut ef = ef.max(k).clamp(1, ef_most.max(1));
    let started = std::time::Instant::now();
    let metric = o.index.shape.params.metric;
    let mut seen = std::collections::HashSet::new();
    let mut rows: Vec<(Vec<u8>, Row, f64)> = Vec::new();
    loop {
        let found = o.index.candidates(r, &qs, ef, &mut s)?;
        o.state.scanned(found.sealed, found.delta);
        // Nearest first by the graph's distance (the SQL operator's
        // order): the first `k` that pass are the answer, so the rest
        // are not read.  A row whose vector is no longer the one found
        // (its distance differs) is passed over: its vector now is in
        // the index too, under its own distance.
        for (d, pk) in found.near {
            if rows.len() >= k {
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
            if !keep(&row)? {
                continue;
            }
            let d = o.sql.of(&row[o.column], q)?;
            rows.push((pk, row, d));
        }
        if rows.len() >= k || found.all || ef >= ef_most {
            rows.sort_by(|a, b| a.2.total_cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
            o.state.searched(started.elapsed().as_nanos() as u64);
            return Ok(Found {
                rows,
                all: found.all,
            });
        }
        ef = (ef * 2).min(ef_most);
    }
}

/// The index method `ix` is searched by, if it is an HNSW index: the
/// SQL distance its operator class serves, and its `m`.
pub fn serves(ix: &IndexDef) -> Option<(SqlMetric, usize)> {
    if ix.method.as_deref() != Some(NAME) {
        return None;
    }
    let class = ix.opclasses.first()?.as_deref()?;
    let (_, sql, _) = OPCLASSES.iter().find(|o| o.0 == class)?;
    Some((*sql, option(ix, "m").unwrap_or(16)))
}

/// The search behind `ORDER BY col <op> q LIMIT k` ([`Access::Nearest`]
/// (crate::plan::Access::Nearest)): at least `k` rows passing `keep`,
/// nearest first, or `None` where the index cannot tell them — it holds
/// fewer, or finding them would take a search as costly as reading every
/// row: about `ef · 2m` distances against the rows' (each node's
/// neighbours at its bottom layer), so `ef` up to the rows over `2m`.
/// `ef` is the session's `hnsw.ef_search`.
pub fn nearest_rows(
    txn: &dyn KvTxn,
    t: &Table,
    i: usize,
    q: &Value,
    k: usize,
    wanted: Option<&[bool]>,
    keep: &mut dyn FnMut(&Row) -> Result<bool>,
) -> Result<Option<Vec<(Vec<u8>, Row)>>> {
    let ef = ef_search();
    let m = option(&t.indexes[i], "m").unwrap_or(16);
    let ef_most = (rows(txn, t, i)? / (2 * m)).max(ef);
    let all = vec![true; t.columns.len()];
    let found = search(txn, t, i, q, k, ef, ef_most, wanted.unwrap_or(&all), keep)?;
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

/// The rows index `i` of `t` holds as `txn` sees it — its graph's and
/// sealed objects' (not the delta's, at most a seal's size).
pub fn rows(txn: &dyn KvTxn, t: &Table, i: usize) -> Result<usize> {
    let o = open(txn.instance(), t, i)?;
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

/// What index `i` of `t` holds as `txn` sees it.
pub fn status(txn: &dyn KvTxn, t: &Table, i: usize) -> Result<store::Status> {
    let o = open(txn.instance(), t, i)?;
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
    by_row: Mutex<ByRow>,
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

/// An index: its database's [`Kv::instance`], its table's id, its id.
type Id = (u64, u32, u32);

fn state(id: Id) -> Arc<State> {
    static STATES: OnceLock<Mutex<HashMap<Id, Arc<State>>>> = OnceLock::new();
    STATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(id)
        .or_default()
        .clone()
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
    fn done(&self, id: Id, delta: u64, sealed: u64) {
        let mut by = self.by.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(w) = by.get_mut(&id) {
            w.delta = w.delta.saturating_sub(delta);
            w.sealed = sealed;
            if w.delta == 0 && w.sealed == 0 {
                by.remove(&id);
            }
        }
        self.any.store(!by.is_empty(), Relaxed);
    }

    fn note(&self, id: Id) {
        self.by
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(id)
            .or_default()
            .delta += 1;
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
            .filter(|(k, _)| k.0 == kv.instance())
            .map(|(k, v)| (*k, *v))
            .collect()
    };
    for (id, w) in due {
        let st = state(id);
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
            match maintain(kv, &st, id, fold)? {
                Some(sealed) => p.done(id, w.delta, sealed),
                None => {
                    p.done(id, u64::MAX, 0);
                    continue;
                }
            }
        }
        if st.compact_due.load(Relaxed) {
            compact(kv, &st, id)?;
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
            .filter(|(k, _)| k.0 == kv.instance())
            .map(|(k, v)| (*k, *v))
            .collect();
    for (id, w) in due {
        let st = state(id);
        let r = {
            let _busy = st.busy.lock().unwrap_or_else(|e| e.into_inner());
            maintain(kv, &st, id, 0)?
        };
        match r {
            Some(sealed) => p.done(id, w.delta, sealed),
            None => {
                p.done(id, u64::MAX, 0);
                continue;
            }
        }
        if st.compact_due.load(Relaxed) {
            compact(kv, &st, id)?;
        }
    }
    Ok(())
}

/// Index `id`'s table and its position there, as `txn` sees the catalog
/// (`None`: gone).
fn find(txn: &dyn KvTxn, id: Id) -> Result<Option<(Table, usize)>> {
    let (_, table, index) = id;
    Ok(crate::catalog::tables(txn)?.into_iter().find_map(|t| {
        let i = t
            .indexes
            .iter()
            .position(|ix| ix.id == index && ix.method.as_deref() == Some(NAME))?;
        (t.id == table).then_some((t, i))
    }))
}

/// Seals index `id` and folds it once `fold` rows are sealed; the rows
/// left sealed, or `None` where the index is gone.  Notes whether a
/// compaction is due.
fn maintain(kv: &Arc<dyn Kv>, st: &State, id: Id, fold: usize) -> Result<Option<u64>> {
    let instance = id.0;
    let started = std::time::Instant::now();
    let mut sealed_rows = 0u64;
    let mut moved = 0;
    let mut queues = None;
    let found = retrying(kv, |txn| {
        let Some((t, i)) = find(&*txn, id)? else {
            return Ok(false);
        };
        let o = open(instance, &t, i)?;
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
        let o = open(instance, &t, i)?;
        let mut by_row = st.by_row.lock().unwrap_or_else(|e| e.into_inner());
        let current = |ro: &dyn KvTxn, pk: &[u8]| -> Result<Option<Vec<u8>>> {
            match o.row(ro, &t, pk)? {
                Some(row) => o.stored(&row[o.column]),
                None => Ok(None),
            }
        };
        rows = o.index.fold(txn, &mut by_row, &current)?;
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
fn compact(kv: &Arc<dyn Kv>, st: &State, id: Id) -> Result<()> {
    let Ok(_compacting) = st.compacting.try_lock() else {
        return Ok(());
    };
    let instance = id.0;
    let made = {
        let txn = kv.begin(TxnOptions::read_only())?;
        let Some((t, i)) = find(&*txn, id)? else {
            return Ok(());
        };
        let o = open(instance, &t, i)?;
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
