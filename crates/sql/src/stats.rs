//! Table statistics and cardinality estimates (PLAN.md §M13).
//!
//! `ANALYZE` reads every row of a table and stores, under the table's
//! statistics key: the row count, and per column the number of NULLs, the
//! number of distinct values, the minimum and maximum, and an equi-depth
//! histogram (16 buckets: boundaries such that each bucket holds about the
//! same number of non-NULL values).  Statistics are a snapshot: they go
//! stale as data changes, and only steer plan choices, never results.
//!
//! Columns are also counted together (PLAN.md §M14.20 V12), for every
//! group of them: the columns of the primary key and of each index with
//! two or more, and each `CREATE STATISTICS` — the distinct combinations of
//! every two or more of a group's columns, and per pair the degree of their
//! functional dependency (how often one's value determines the other's: of
//! the sampled rows whose value of it repeats, the fraction whose group
//! agrees on the other's).  Equalities on several columns of a table are
//! estimated from them — a set counted together as one, the others each
//! as likely as its dependencies make it ([`together`]) — and so are joins
//! on several columns and the groups of a `GROUP BY` (`group_distinct`),
//! rather than taking the columns for independent.
//!
//! Without statistics a table is assumed to have [`DEFAULT_ROWS`] rows and
//! predicates get PostgreSQL-like default selectivities.

use std::cell::RefCell;
use std::collections::HashMap;

use nucleus_kv::KvTxn;

use crate::ast::{BinaryOp, UnaryOp};
use crate::catalog::{self, Table};
use crate::encoding::{Reader, Writer};
use crate::error::Result;
use crate::expr::Expr;
use crate::plan::Plan;
use crate::types::Value;
use crate::vector::Vector;

/// Rows assumed for a table never analyzed.
pub const DEFAULT_ROWS: f64 = 1000.0;
const BUCKETS: usize = 16;
const STATS_VERSION: u8 = 2;
/// Columns a group of statistics holds at most (as PostgreSQL's).
pub const GROUP_COLUMNS: usize = 8;

/// Statistics of one column.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnStats {
    /// NULL values.
    pub nulls: u64,
    /// Distinct non-NULL values.
    pub distinct: u64,
    /// Histogram boundaries, ascending (first = minimum, last = maximum).
    pub bounds: Vec<Value>,
}

/// The distinct combinations of some columns' values (NULL counted as a
/// value).
#[derive(Clone, Debug, PartialEq)]
pub struct GroupDistinct {
    /// The columns, ascending.
    pub columns: Vec<usize>,
    /// Distinct combinations.
    pub distinct: u64,
}

/// A functional dependency: `from`'s value determines `to`'s in a
/// `degree` (0 to 1) of the rows.
#[derive(Clone, Debug, PartialEq)]
pub struct Dependency {
    /// The determining column.
    pub from: usize,
    /// The determined one.
    pub to: usize,
    /// How often.
    pub degree: f64,
}

/// Statistics of a table.
#[derive(Clone, Debug, PartialEq)]
pub struct TableStats {
    /// Rows.
    pub rows: u64,
    /// Per column.
    pub columns: Vec<ColumnStats>,
    /// Columns counted together.
    pub groups: Vec<GroupDistinct>,
    /// Dependencies between columns.
    pub dependencies: Vec<Dependency>,
}

impl TableStats {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u8(STATS_VERSION)
            .u64(self.rows)
            .u32(self.columns.len() as u32);
        for c in &self.columns {
            w.u64(c.nulls).u64(c.distinct).u32(c.bounds.len() as u32);
            for b in &c.bounds {
                w.value(b);
            }
        }
        w.u32(self.groups.len() as u32);
        for g in &self.groups {
            w.u32(g.columns.len() as u32);
            for &c in &g.columns {
                w.u32(c as u32);
            }
            w.u64(g.distinct);
        }
        w.u32(self.dependencies.len() as u32);
        for d in &self.dependencies {
            w.u32(d.from as u32)
                .u32(d.to as u32)
                .u64(d.degree.to_bits());
        }
        w.0
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let version = r.u8()?;
        if !(1..=STATS_VERSION).contains(&version) {
            return Err(crate::Error::internal("unknown statistics version"));
        }
        let rows = r.u64()?;
        let n = r.u32()? as usize;
        let mut columns = Vec::with_capacity(n);
        for _ in 0..n {
            let nulls = r.u64()?;
            let distinct = r.u64()?;
            let m = r.u32()? as usize;
            let bounds = (0..m).map(|_| r.value()).collect::<Result<_>>()?;
            columns.push(ColumnStats {
                nulls,
                distinct,
                bounds,
            });
        }
        let (mut groups, mut dependencies) = (Vec::new(), Vec::new());
        if version >= 2 {
            for _ in 0..r.u32()? {
                let columns = (0..r.u32()?)
                    .map(|_| r.u32().map(|c| c as usize))
                    .collect::<Result<_>>()?;
                groups.push(GroupDistinct {
                    columns,
                    distinct: r.u64()?,
                });
            }
            for _ in 0..r.u32()? {
                dependencies.push(Dependency {
                    from: r.u32()? as usize,
                    to: r.u32()? as usize,
                    degree: f64::from_bits(r.u64()?),
                });
            }
        }
        Ok(Self {
            rows,
            columns,
            groups,
            dependencies,
        })
    }
}

/// Rows ANALYZE looks at, at most: a uniform sample of a larger table (as
/// PostgreSQL's 30 000).
const SAMPLE: usize = 30_000;

/// Rows of the sample kept for estimates ([`stats_sample`]), at most, and
/// their bytes at most (a table of 768-dimension vectors keeps about 330).
const KEPT_ROWS: usize = 1000;
const KEPT_BYTES: usize = 1 << 20;
const KEPT_VERSION: u8 = 1;

/// Keeps `KEPT_ROWS` of ANALYZE's sampled rows (a uniform choice of them,
/// by `seed`) under the table's sample key, a column to a key, so a
/// predicate the statistics cannot estimate is estimated on them
/// ([`Stats::selectivity`]) from the columns it reads.
fn keep_sample(
    txn: &mut dyn KvTxn,
    table: &Table,
    rows: &[crate::types::Row],
    seed: u64,
) -> Result<()> {
    catalog::delete_stats_sample(txn, table)?;
    // A uniform choice of the reservoir's slots (its slots are not each
    // a uniform choice of the rows: the first ones keep early rows more
    // often).
    let mut order: Vec<usize> = (0..rows.len()).collect();
    let mut state = seed ^ 0x5eed_5a3f_1e00_0001;
    let take = order.len().min(KEPT_ROWS);
    for i in 0..take {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let r = crate::kernels::mix(state);
        let j = i + (r % (order.len() - i) as u64) as usize;
        order.swap(i, j);
    }
    let width = table.columns.len();
    let mut cols: Vec<Vec<Value>> = (0..width).map(|_| Vec::new()).collect();
    let (mut bytes, mut scratch) = (0usize, Vec::new());
    for &i in &order[..take] {
        if bytes > KEPT_BYTES {
            break;
        }
        for (c, v) in cols.iter_mut().zip(&rows[i]) {
            scratch.clear();
            crate::encoding::encode_value(&mut scratch, v);
            bytes += scratch.len();
            c.push(v.clone());
        }
    }
    let n = cols.first().map_or(0, Vec::len);
    let key = catalog::stats_sample_key(table);
    let mut w = Writer::default();
    w.u8(KEPT_VERSION).u32(width as u32).u32(n as u32);
    txn.put(&key, &w.0)?;
    // Each column as a column table's segment holds it: decoded straight
    // into the vector a batch evaluates.
    for (c, col) in cols.into_iter().enumerate() {
        let mut k = key.clone();
        k.extend_from_slice(&(c as u16).to_be_bytes());
        txn.put(&k, &crate::segment::encode(col).0)?;
    }
    Ok(())
}

/// The kept sample rows' count ([`keep_sample`]), if they are of the
/// table's present columns.
fn kept_rows(txn: &dyn KvTxn, table: &Table) -> Option<usize> {
    let b = txn.get(&catalog::stats_sample_key(table)).ok()??;
    let mut r = Reader::new(&b);
    if r.u8().ok()? != KEPT_VERSION || r.u32().ok()? as usize != table.columns.len() {
        return None;
    }
    Some(r.u32().ok()? as usize)
}

/// Column `c`'s kept sample values (as many as [`kept_rows`] says), as
/// a vector.
fn kept_column(txn: &dyn KvTxn, table: &Table, c: usize) -> Option<Vector> {
    let mut k = catalog::stats_sample_key(table);
    k.extend_from_slice(&(c as u16).to_be_bytes());
    crate::segment::decode(&txn.get(&k).ok()??).ok()
}

/// Computes and stores the statistics of one table: from all its rows, or
/// from a uniform sample of [`SAMPLE`] ([`Sample`], seeded once from the
/// database's random numbers; the table's storage decodes only the rows it
/// takes) — the row count is always exact.
pub fn analyze_table(txn: &mut dyn KvTxn, table: &Table) -> Result<TableStats> {
    let mut seed = [0u8; 8];
    crate::datetime::random_bytes(&mut seed)?;
    let seed = u64::from_le_bytes(seed);
    let (rows, total) = {
        let mut sample = Sample::new(SAMPLE, seed);
        let start = table.index_prefix(0);
        let end = nucleus_kv::prefix_end(&start);
        crate::storage::of(table)?.sample(
            crate::storage::Scan {
                txn: &*txn,
                table,
                start,
                end,
                wanted: None,
                validated: false,
                bounds: Vec::new(),
                dicts: Vec::new(),
            },
            &mut sample,
        )?;
        let total = sample.seen();
        (sample.into_rows(), total)
    };
    let scale = |n: u64| -> u64 {
        if rows.is_empty() {
            0
        } else {
            ((n as f64) * (total as f64) / (rows.len() as f64)).round() as u64
        }
    };
    let mut columns = Vec::with_capacity(table.columns.len());
    for c in 0..table.columns.len() {
        let mut vals: Vec<Value> = rows
            .iter()
            .map(|r| r[c].clone())
            .filter(|v| !v.is_null())
            .collect();
        let nulls = scale((rows.len() - vals.len()) as u64);
        let mut counts: HashMap<Value, u64> = HashMap::new();
        for v in &vals {
            *counts.entry(v.clone()).or_default() += 1;
        }
        let distinct = distinct_of(
            vals.len(),
            counts.values(),
            (rows.len() as u64) < total,
            total - nulls,
        );
        vals.sort_by(Value::sort_cmp);
        let mut bounds = Vec::new();
        if !vals.is_empty() {
            for i in 0..=BUCKETS {
                let at = (i * (vals.len() - 1)) / BUCKETS;
                bounds.push(vals[at].clone());
            }
            bounds.dedup_by(|a, b| a.sort_cmp(b).is_eq());
        }
        columns.push(ColumnStats {
            nulls,
            distinct,
            bounds,
        });
    }
    let (groups, dependencies) = together_stats(table, &rows, (rows.len() as u64) < total, total);
    let stats = TableStats {
        rows: total,
        columns,
        groups,
        dependencies,
    };
    txn.put(&catalog::stats_key(table), &stats.encode())?;
    keep_sample(txn, table, &rows, seed)?;
    Ok(stats)
}

/// A uniform sample of a table's rows as its storage reads them
/// ([`crate::storage::TableStorage::sample`]): each row offered in key
/// order, and made — decoded — only when the
/// [`Reservoir`](crate::sample::Reservoir) takes it, so a
/// large table's `ANALYZE` decodes about `size · (1 + ln(N / size))` rows,
/// not N (PLAN.md §M14.20 W3b).
pub struct Sample {
    reservoir: crate::sample::Reservoir,
    rows: Vec<crate::types::Row>,
    seen: u64,
}

impl Sample {
    /// A sample of at most `size` rows, its draws from `seed`.
    pub fn new(size: usize, seed: u64) -> Self {
        Self {
            reservoir: crate::sample::Reservoir::new(size, seed),
            rows: Vec::new(),
            seen: 0,
        }
    }

    /// Offers the next row, made by `row` only if the sample takes it (each
    /// row counted against the statement's scan budget).
    pub fn offer(&mut self, row: impl FnOnce() -> Result<crate::types::Row>) -> Result<()> {
        crate::budget::charge_scan()?;
        self.seen += 1;
        if self.rows.len() < self.reservoir.size() {
            self.rows.push(row()?);
        } else if let Some(slot) = self.reservoir.offer(self.seen) {
            self.rows[slot] = row()?;
        }
        Ok(())
    }

    /// How many rows were offered.
    pub fn seen(&self) -> u64 {
        self.seen
    }

    /// The rows taken.
    pub fn into_rows(self) -> Vec<crate::types::Row> {
        self.rows
    }
}

/// The distinct values among `n` sampled, from how often each was seen:
/// all of them, or (`sampled`, of `total`) PostgreSQL's estimator (Haas and
/// Stokes' Duj1), from the values seen once.
fn distinct_of<'a>(
    n: usize,
    counts: impl Iterator<Item = &'a u64> + Clone,
    sampled: bool,
    total: u64,
) -> u64 {
    let d = counts.clone().count() as u64;
    if !sampled {
        return d;
    }
    let n = n as f64;
    let f1 = counts.filter(|&&k| k == 1).count() as f64;
    let big = total.max(1) as f64;
    let est = n * d as f64 / (n - f1 + f1 * n / big);
    (est.round() as u64).clamp(d, total.max(d))
}

/// The groups of columns counted together: those of the primary key, of
/// each index and of each `CREATE STATISTICS` with two or more (the first
/// [`GROUP_COLUMNS`] of an index's), each once, ascending.
fn column_groups(table: &Table) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut add = |cols: &[usize]| {
        let mut c = cols[..cols.len().min(GROUP_COLUMNS)].to_vec();
        c.sort_unstable();
        c.dedup();
        if c.len() >= 2 && !out.contains(&c) {
            out.push(c);
        }
    };
    add(&table.primary_key);
    for i in &table.indexes {
        add(&i.columns);
    }
    for s in &table.statistics {
        add(&s.columns);
    }
    out
}

/// The distinct combinations of every two or more columns of each group,
/// and the dependencies between each two of a group's (see the module),
/// from the sampled `rows`.
fn together_stats(
    table: &Table,
    rows: &[crate::types::Row],
    sampled: bool,
    total: u64,
) -> (Vec<GroupDistinct>, Vec<Dependency>) {
    let (mut groups, mut dependencies) = (Vec::<GroupDistinct>::new(), Vec::<Dependency>::new());
    for cols in column_groups(table) {
        for mask in 1u32..(1 << cols.len()) {
            if mask.count_ones() < 2 {
                continue;
            }
            let set: Vec<usize> = (0..cols.len())
                .filter(|i| mask & (1 << i) != 0)
                .map(|i| cols[i])
                .collect();
            if groups.iter().any(|g| g.columns == set) {
                continue;
            }
            let mut counts: HashMap<Vec<&Value>, u64> = HashMap::new();
            for r in rows {
                *counts
                    .entry(set.iter().map(|&c| &r[c]).collect())
                    .or_default() += 1;
            }
            groups.push(GroupDistinct {
                distinct: distinct_of(rows.len(), counts.values(), sampled, total),
                columns: set,
            });
        }
        for &from in &cols {
            for &to in &cols {
                if from == to || dependencies.iter().any(|d| d.from == from && d.to == to) {
                    continue;
                }
                if let Some(degree) = dependency(rows, from, to, sampled) {
                    dependencies.push(Dependency { from, to, degree });
                }
            }
        }
    }
    (groups, dependencies)
}

/// How often `from`'s value determines `to`'s in `rows`: of the rows whose
/// value of `from` repeats, the fraction in a group agreeing on `to` — a
/// value seen once tells nothing of a sample (in the whole table, it
/// determines); `None` if no value repeats in a sample.
fn dependency(rows: &[crate::types::Row], from: usize, to: usize, sampled: bool) -> Option<f64> {
    // Per value of `from`: the first value of `to`, whether all agree, how
    // many rows.
    let mut seen: HashMap<&Value, (&Value, bool, u64)> = HashMap::new();
    for r in rows {
        let e = seen.entry(&r[from]).or_insert((&r[to], true, 0));
        e.1 &= e.0 == &r[to];
        e.2 += 1;
    }
    let (mut agree, mut repeated) = (0u64, 0u64);
    for &(_, all, n) in seen.values() {
        if n > 1 || !sampled {
            repeated += n;
            if all {
                agree += n;
            }
        }
    }
    (repeated > 0).then(|| agree as f64 / repeated as f64)
}

/// `ANALYZE [table]`.
pub fn analyze(txn: &mut dyn KvTxn, table: Option<&str>) -> Result<()> {
    let tables = match table {
        Some(name) => vec![catalog::table(&*txn, name)?],
        None => catalog::tables(&*txn)?,
    };
    for t in tables {
        analyze_table(txn, &t)?;
    }
    Ok(())
}

/// Statistics, read once per optimization.
pub struct Stats<'a> {
    txn: &'a dyn KvTxn,
    cache: RefCell<HashMap<u32, Option<TableStats>>>,
    /// Tables' kept sample row counts (read when an estimate first needs
    /// them).
    samples: RefCell<HashMap<u32, Option<usize>>>,
    /// Predicates estimated on a sample, and their estimates.
    estimated: RefCell<Vec<(u32, Expr, Option<f64>)>>,
}

impl<'a> Stats<'a> {
    /// The transaction it reads through.
    pub fn txn(&self) -> &'a dyn KvTxn {
        self.txn
    }

    /// Statistics read through `txn`.
    pub fn new(txn: &'a dyn KvTxn) -> Self {
        Self {
            txn,
            cache: RefCell::new(HashMap::new()),
            samples: RefCell::new(HashMap::new()),
            estimated: RefCell::new(Vec::new()),
        }
    }

    /// `table`'s kept sample rows of the columns `cols` only (in that
    /// order), as a batch's vectors, and their count — if it was analyzed
    /// since its columns last changed.
    fn sample(&self, table: &Table, cols: &[usize]) -> Option<(Vec<Vector>, usize)> {
        let n = (*self
            .samples
            .borrow_mut()
            .entry(table.id)
            .or_insert_with(|| kept_rows(self.txn, table)))?;
        let vectors = cols
            .iter()
            .map(|&c| kept_column(self.txn, table, c))
            .collect::<Option<_>>()?;
        Some((vectors, n))
    }

    /// The fraction of the kept sample rows for which `pred` is true —
    /// for a predicate the statistics cannot estimate (a function of
    /// columns, a distance below a radius, two columns compared, a
    /// pattern), evaluated as the executor would: `None` without a sample,
    /// or when `pred` cannot be evaluated here (a subquery, a volatile
    /// function, a parameter not yet bound, an error).  No match counts
    /// as half of one.  An index method that knows the share at less cost
    /// says it first ([`crate::index_method::selectivity`]: the text
    /// index from its words' document counts, not a text analyzed a
    /// sample row).
    fn sampled(&self, table: &Table, pred: &Expr) -> Option<f64> {
        if pred.has_subquery() || volatile(pred) {
            return None;
        }
        // A plan's passes ask again of the same predicates.
        if let Some((_, _, s)) = self
            .estimated
            .borrow()
            .iter()
            .find(|(t, p, _)| *t == table.id && p == pred)
        {
            return *s;
        }
        let s = crate::index_method::selectivity(self.txn, table, pred)
            .map(|s| s.max(0.5 / self.rows(table).max(1.0)))
            .or_else(|| self.count_sampled(table, pred));
        self.estimated
            .borrow_mut()
            .push((table.id, pred.clone(), s));
        s
    }

    fn count_sampled(&self, table: &Table, pred: &Expr) -> Option<f64> {
        let mut cols = pred.reads();
        cols.sort_unstable();
        cols.dedup();
        if cols.iter().any(|&c| c >= table.columns.len()) {
            return None;
        }
        let (vectors, n) = self.sample(table, &cols)?;
        if n == 0 {
            return None;
        }
        let pred =
            pred.map_columns(&|c| Expr::Column(cols.binary_search(&c).expect("a column it reads")));
        let reg = crate::ext::Registry::shared_builtin();
        let ctx = crate::expr::Ctx::new(self.txn, &reg, &[]);
        let hits = crate::vector::count_true(&pred, vectors, n, &ctx).ok()?;
        Some((hits as f64).max(0.5) / n as f64)
    }

    /// The statistics of `table`, if it was analyzed.
    pub fn of(&self, table: &Table) -> Option<TableStats> {
        self.cache
            .borrow_mut()
            .entry(table.id)
            .or_insert_with(|| {
                self.txn
                    .get(&catalog::stats_key(table))
                    .ok()
                    .flatten()
                    .and_then(|b| TableStats::decode(&b).ok())
            })
            .clone()
    }

    /// Rows of `table`.
    pub fn rows(&self, table: &Table) -> f64 {
        self.of(table).map_or(DEFAULT_ROWS, |s| s.rows as f64)
    }

    /// The fraction of `table`'s rows satisfying `pred` (over its columns).
    pub fn selectivity(&self, table: &Table, pred: &Expr) -> f64 {
        let stats = self.of(table);
        let sampled = |p: &Expr| self.sampled(table, p);
        selectivity(table, stats.as_ref(), &sampled, pred).clamp(0.0, 1.0)
    }

    /// The distinct combinations of columns `cols` of `table` (estimated),
    /// and whether some of them were counted together: the largest set of
    /// them counted together times the others' own, at most its rows.
    pub fn group_distinct(&self, table: &Table, cols: &[usize]) -> (f64, bool) {
        let mut cols = cols.to_vec();
        cols.sort_unstable();
        cols.dedup();
        let stats = self.of(table);
        let group = stats.as_ref().and_then(|s| {
            s.groups
                .iter()
                .filter(|g| g.columns.iter().all(|c| cols.contains(c)))
                .max_by_key(|g| g.columns.len())
                .cloned()
        });
        let mut n = group.as_ref().map_or(1.0, |g| (g.distinct as f64).max(1.0));
        for &c in &cols {
            if !group.as_ref().is_some_and(|g| g.columns.contains(&c)) {
                n *= self.distinct(table, c);
            }
        }
        (n.min(self.rows(table).max(1.0)), group.is_some())
    }

    /// The distinct values of column `c` of `table` (estimated).
    pub fn distinct(&self, table: &Table, c: usize) -> f64 {
        let unique = (table.primary_key.len() == 1 && table.primary_key[0] == c)
            || table.indexes.iter().any(|i| i.unique && i.columns == [c]);
        match self.of(table) {
            Some(s) => (s.columns.get(c).map_or(1, |cs| cs.distinct) as f64).max(1.0),
            None if unique => DEFAULT_ROWS,
            None => (DEFAULT_ROWS / 10.0).max(1.0),
        }
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Real(f) => Some(f64::from(*f)),
        Value::Float(f) => Some(*f),
        Value::Numeric(d) => Some(d.to_f64()),
        Value::Date(_) | Value::Timestamp(_) | Value::TimestampTz(_) => {
            v.as_timestamp().map(|t| t as f64)
        }
        Value::Interval(i) => Some(i.span() as f64),
        _ => None,
    }
}

/// The fraction of a column's non-NULL values below `v` (from the histogram).
fn fraction_below(cs: &ColumnStats, v: &Value) -> f64 {
    let b = &cs.bounds;
    if b.is_empty() {
        return 0.5;
    }
    // As numbers where both are (a date's bounds and a timestamp, say),
    // else in the values' order.
    let cmp = |x: &Value, y: &Value| match (as_f64(x), as_f64(y)) {
        (Some(a), Some(b)) => a.total_cmp(&b),
        _ => x.sort_cmp(y),
    };
    if cmp(v, &b[0]).is_le() {
        return 0.0;
    }
    if cmp(v, &b[b.len() - 1]).is_gt() {
        return 1.0;
    }
    let buckets = (b.len() - 1).max(1) as f64;
    for i in 0..b.len() - 1 {
        if cmp(v, &b[i + 1]).is_le() {
            // Linear inside the bucket for numbers, half a bucket otherwise.
            let within = match (as_f64(&b[i]), as_f64(&b[i + 1]), as_f64(v)) {
                (Some(lo), Some(hi), Some(x)) if hi > lo => (x - lo) / (hi - lo),
                _ => 0.5,
            };
            return (i as f64 + within) / buckets;
        }
    }
    1.0
}

fn const_value(e: &Expr) -> Option<Value> {
    match e {
        Expr::Literal(v) => Some(v.clone()),
        _ => None,
    }
}

/// Whether `e` calls a volatile function.
fn volatile(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        if let Expr::Call(f, _) = x
            && f.volatility() == crate::ext::Volatility::Volatile
        {
            found = true;
        }
    });
    found
}

/// `pred`'s selectivity: from the statistics where they describe it (a
/// column against a value, `IS NULL`, `IN`, ranges, equalities counted
/// together), else on the kept sample rows (`sampled`), else
/// PostgreSQL-like defaults.
fn selectivity(
    table: &Table,
    stats: Option<&TableStats>,
    sampled: &dyn Fn(&Expr) -> Option<f64>,
    pred: &Expr,
) -> f64 {
    let rows = stats.map_or(DEFAULT_ROWS, |s| s.rows as f64).max(1.0);
    let col = |c: usize| stats.and_then(|s| s.columns.get(c));
    let non_null = |c: usize| col(c).map_or(1.0, |cs| 1.0 - cs.nulls as f64 / rows);
    let distinct = |c: usize| -> f64 {
        let unique = (table.primary_key.len() == 1 && table.primary_key[0] == c)
            || table.indexes.iter().any(|i| i.unique && i.columns == [c]);
        match col(c) {
            Some(cs) => (cs.distinct as f64).max(1.0),
            None if unique => rows,
            None => 200.0_f64.min(rows),
        }
    };
    match pred {
        Expr::Binary(BinaryOp::And, ..) => {
            // Equalities of columns to values together, the rest as
            // independent.
            let mut parts = Vec::new();
            and_parts(pred, &mut parts);
            let mut eqs: Vec<(usize, f64)> = Vec::new();
            let mut rest = 1.0;
            // A column's lower and upper bound together: the histogram
            // between them, not the product of each side's.
            let mut ranges: Vec<(usize, Option<Value>, Option<Value>)> = Vec::new();
            for p in parts {
                if let Some((c, lower, v)) = range(p)
                    && stats.is_some_and(|s| s.columns.get(c).is_some())
                {
                    let r = match ranges.iter_mut().find(|r| r.0 == c) {
                        Some(r) => r,
                        None => {
                            ranges.push((c, None, None));
                            ranges.last_mut().expect("pushed")
                        }
                    };
                    let slot = if lower { &mut r.1 } else { &mut r.2 };
                    if slot.is_none() {
                        *slot = Some(v);
                        continue;
                    }
                }
                let s = selectivity(table, stats, sampled, p);
                match equality(p) {
                    Some(c) if !eqs.iter().any(|e| e.0 == c) => eqs.push((c, s)),
                    _ => rest *= s,
                }
            }
            for (c, lo, hi) in ranges {
                let cs = stats.and_then(|s| s.columns.get(c)).expect("checked");
                let below = |v: &Option<Value>, none: f64| {
                    v.as_ref().map_or(none, |v| fraction_below(cs, v))
                };
                let frac = (below(&hi, 1.0) - below(&lo, 0.0)).max(0.0);
                rest *= (frac * non_null(c)).max(1.0 / rows);
            }
            rest * together(table, stats, &eqs)
        }
        Expr::Binary(BinaryOp::Or, a, b) => {
            let (x, y) = (
                selectivity(table, stats, sampled, a),
                selectivity(table, stats, sampled, b),
            );
            x + y - x * y
        }
        Expr::Unary(UnaryOp::Not, e) => 1.0 - selectivity(table, stats, sampled, e),
        Expr::IsNull(e, negated) => match &**e {
            Expr::Column(c) => {
                let s = col(*c).map_or(0.05, |cs| cs.nulls as f64 / rows);
                if *negated { 1.0 - s } else { s }
            }
            _ => sampled(pred).unwrap_or(0.5),
        },
        Expr::InList(e, list, negated) => match &**e {
            Expr::Column(c) => {
                let s = (list.len() as f64 / distinct(*c)).min(1.0) * non_null(*c);
                if *negated { 1.0 - s } else { s }
            }
            _ => sampled(pred).unwrap_or(0.5),
        },
        // Without a sample, a tenth of the rows match a pattern.
        Expr::Like { negated, .. } => sampled(pred).unwrap_or(if *negated { 0.9 } else { 0.1 }),
        Expr::Binary(op, a, b) => {
            let (c, v, op) = match (&**a, &**b) {
                (Expr::Column(c), e) if e.columns().is_empty() => (*c, e, *op),
                (e, Expr::Column(c)) if e.columns().is_empty() => (*c, e, flip(*op)),
                _ => return sampled(pred).unwrap_or(0.33),
            };
            match op {
                BinaryOp::Eq => non_null(c) / distinct(c),
                BinaryOp::NotEq => non_null(c) * (1.0 - 1.0 / distinct(c)),
                BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => {
                    let (Some(cs), Some(v)) = (col(c), const_value(v)) else {
                        return 0.33;
                    };
                    let below = fraction_below(cs, &v);
                    let frac = if matches!(op, BinaryOp::Lt | BinaryOp::LtEq) {
                        below
                    } else {
                        1.0 - below
                    };
                    (frac * non_null(c)).max(1.0 / rows)
                }
                _ => sampled(pred).unwrap_or(0.5),
            }
        }
        _ => sampled(pred).unwrap_or(0.5),
    }
}

fn and_parts<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match e {
        Expr::Binary(BinaryOp::And, a, b) => {
            and_parts(a, out);
            and_parts(b, out);
        }
        e => out.push(e),
    }
}

/// A range bound `p` puts on a column with a value: (column, whether it
/// is a lower bound, the value).
fn range(p: &Expr) -> Option<(usize, bool, Value)> {
    let Expr::Binary(op, a, b) = p else {
        return None;
    };
    let (c, v, op) = match (&**a, &**b) {
        (Expr::Column(c), Expr::Literal(v)) => (*c, v, *op),
        (Expr::Literal(v), Expr::Column(c)) => (*c, v, flip(*op)),
        _ => return None,
    };
    if v.is_null() {
        return None;
    }
    match op {
        BinaryOp::Gt | BinaryOp::GtEq => Some((c, true, v.clone())),
        BinaryOp::Lt | BinaryOp::LtEq => Some((c, false, v.clone())),
        _ => None,
    }
}

/// The column `p` sets equal to a value (one reading no column of the row).
fn equality(p: &Expr) -> Option<usize> {
    match p {
        Expr::Binary(BinaryOp::Eq, a, b) => match (&**a, &**b) {
            (Expr::Column(c), e) | (e, Expr::Column(c)) if e.columns().is_empty() => Some(*c),
            _ => None,
        },
        _ => None,
    }
}

/// The selectivity of equalities on different columns of a table (each
/// column with its own): one row if they cover a key; else by the chain
/// rule — from a set of them counted together (`1 / its combinations`),
/// each other column in turn given those before it: `combinations(T) /
/// combinations(T and it)` for the most of them T counted together with
/// it, else the strongest dependency on one of them (`degree + (1 -
/// degree) * its own`, PostgreSQL's), else its own; the next always the
/// likeliest.  Of the sets to start from (none, if no set is counted), the
/// likeliest outcome.
fn together(table: &Table, stats: Option<&TableStats>, eqs: &[(usize, f64)]) -> f64 {
    let product: f64 = eqs.iter().map(|e| e.1).product();
    if eqs.len() < 2 {
        return product;
    }
    let cols: Vec<usize> = eqs.iter().map(|e| e.0).collect();
    let rows = stats.map_or(DEFAULT_ROWS, |s| s.rows as f64).max(1.0);
    let covers = |key: &[usize]| !key.is_empty() && key.iter().all(|c| cols.contains(c));
    if covers(&table.primary_key) || table.indexes.iter().any(|i| i.unique && covers(&i.columns)) {
        return 1.0 / rows;
    }
    let Some(s) = stats else { return product };
    // The combinations of a set of columns (ascending), if known.
    let combos = |set: &[usize]| -> Option<f64> {
        match set {
            [c] => s.columns.get(*c).map(|cs| (cs.distinct as f64).max(1.0)),
            _ => s
                .groups
                .iter()
                .find(|g| g.columns == set)
                .map(|g| (g.distinct as f64).max(1.0)),
        }
    };
    let own = |c: usize| eqs.iter().find(|e| e.0 == c).map_or(1.0, |e| e.1);
    // How likely column `c`'s equality is given those of `covered`'s.
    let given = |c: usize, covered: &[usize]| -> f64 {
        let mut best: Option<f64> = None;
        for g in s.groups.iter().filter(|g| g.columns.contains(&c)) {
            let t: Vec<usize> = g
                .columns
                .iter()
                .copied()
                .filter(|x| covered.contains(x))
                .collect();
            if t.is_empty() {
                continue;
            }
            let mut with = t.clone();
            with.push(c);
            with.sort_unstable();
            if let (Some(a), Some(b)) = (combos(&t), combos(&with)) {
                let p = (a / b).min(1.0);
                best = Some(best.map_or(p, |q: f64| q.max(p)));
            }
        }
        if let Some(p) = best {
            return p;
        }
        s.dependencies
            .iter()
            .filter(|d| d.to == c && covered.contains(&d.from))
            .map(|d| d.degree + (1.0 - d.degree) * own(c))
            .fold(own(c), f64::max)
    };
    let mut starts: Vec<Option<&GroupDistinct>> = s
        .groups
        .iter()
        .filter(|g| g.columns.iter().all(|c| cols.contains(c)))
        .map(Some)
        .collect();
    if starts.is_empty() {
        starts.push(None);
    }
    let mut likeliest: f64 = 0.0;
    for start in starts {
        let (mut sel, mut covered) = match start {
            Some(g) => {
                let non_null = g
                    .columns
                    .iter()
                    .map(|&c| {
                        s.columns
                            .get(c)
                            .map_or(1.0, |cs| 1.0 - cs.nulls as f64 / rows)
                    })
                    .fold(1.0, f64::min);
                (non_null / (g.distinct as f64).max(1.0), g.columns.clone())
            }
            None => (1.0, Vec::new()),
        };
        let mut left: Vec<usize> = cols
            .iter()
            .copied()
            .filter(|c| !covered.contains(c))
            .collect();
        while !left.is_empty() {
            let (at, p) = left
                .iter()
                .enumerate()
                .map(|(i, &c)| (i, given(c, &covered)))
                .fold((0, -1.0), |b, x| if x.1 > b.1 { x } else { b });
            sel *= p;
            covered.push(left.swap_remove(at));
        }
        likeliest = likeliest.max(sel);
    }
    likeliest
}

fn flip(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::LtEq => BinaryOp::GtEq,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::GtEq => BinaryOp::LtEq,
        op => op,
    }
}

/// Adds row estimates to a plan's scans for `EXPLAIN` (they are already
/// there when the optimizer had statistics).
pub fn annotate(_txn: &dyn KvTxn, plan: Plan) -> Result<Plan> {
    Ok(plan)
}
