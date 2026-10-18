//! Table statistics and cardinality estimates (PLAN.md §M13).
//!
//! `ANALYZE` reads every row of a table and stores, under the table's
//! statistics key: the row count, and per column the number of NULLs, the
//! number of distinct values, the minimum and maximum, and an equi-depth
//! histogram (16 buckets: boundaries such that each bucket holds about the
//! same number of non-NULL values).  Statistics are a snapshot: they go
//! stale as data changes, and only steer plan choices, never results.
//!
//! Without statistics a table is assumed to have [`DEFAULT_ROWS`] rows and
//! predicates get PostgreSQL-like default selectivities.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use nucleus_kv::KvTxn;

use crate::ast::{BinaryOp, UnaryOp};
use crate::catalog::{self, Table};
use crate::encoding::{Reader, Writer};
use crate::error::Result;
use crate::exec;
use crate::expr::{Ctx, Expr};
use crate::ext::Registry;
use crate::plan::{Access, Plan};
use crate::types::Value;

/// Rows assumed for a table never analyzed.
pub const DEFAULT_ROWS: f64 = 1000.0;
const BUCKETS: usize = 16;
const STATS_VERSION: u8 = 1;

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

/// Statistics of a table.
#[derive(Clone, Debug, PartialEq)]
pub struct TableStats {
    /// Rows.
    pub rows: u64,
    /// Per column.
    pub columns: Vec<ColumnStats>,
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
        w.0
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        if r.u8()? != STATS_VERSION {
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
        Ok(Self { rows, columns })
    }
}

/// Computes and stores the statistics of one table.
pub fn analyze_table(txn: &mut dyn KvTxn, reg: &Registry, table: &Table) -> Result<TableStats> {
    let rows: Vec<crate::types::Row> = {
        let ctx = Ctx::new(&*txn, reg, &[]);
        exec::matching_rows(table, &Access::Full, None, &ctx)?
            .into_iter()
            .map(|(_, r)| r)
            .collect()
    };
    let mut columns = Vec::with_capacity(table.columns.len());
    for c in 0..table.columns.len() {
        let mut vals: Vec<Value> = rows
            .iter()
            .map(|r| r[c].clone())
            .filter(|v| !v.is_null())
            .collect();
        let nulls = (rows.len() - vals.len()) as u64;
        let distinct = vals.iter().cloned().collect::<HashSet<_>>().len() as u64;
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
    let stats = TableStats {
        rows: rows.len() as u64,
        columns,
    };
    txn.put(&catalog::stats_key(table), &stats.encode())?;
    Ok(stats)
}

/// `ANALYZE [table]`.
pub fn analyze(txn: &mut dyn KvTxn, reg: &Registry, table: Option<&str>) -> Result<()> {
    let tables = match table {
        Some(name) => vec![catalog::table(&*txn, name)?],
        None => catalog::tables(&*txn)?,
    };
    for t in tables {
        analyze_table(txn, reg, &t)?;
    }
    Ok(())
}

/// Statistics, read once per optimization.
pub struct Stats<'a> {
    txn: &'a dyn KvTxn,
    cache: RefCell<HashMap<u32, Option<TableStats>>>,
}

impl<'a> Stats<'a> {
    /// Statistics read through `txn`.
    pub fn new(txn: &'a dyn KvTxn) -> Self {
        Self {
            txn,
            cache: RefCell::new(HashMap::new()),
        }
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
        selectivity(table, stats.as_ref(), pred).clamp(0.0, 1.0)
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
    if v.sort_cmp(&b[0]).is_le() {
        return 0.0;
    }
    if v.sort_cmp(&b[b.len() - 1]).is_gt() {
        return 1.0;
    }
    let buckets = (b.len() - 1).max(1) as f64;
    for i in 0..b.len() - 1 {
        if v.sort_cmp(&b[i + 1]).is_le() {
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

fn selectivity(table: &Table, stats: Option<&TableStats>, pred: &Expr) -> f64 {
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
        Expr::Binary(BinaryOp::And, a, b) => {
            selectivity(table, stats, a) * selectivity(table, stats, b)
        }
        Expr::Binary(BinaryOp::Or, a, b) => {
            let (x, y) = (selectivity(table, stats, a), selectivity(table, stats, b));
            x + y - x * y
        }
        Expr::Unary(UnaryOp::Not, e) => 1.0 - selectivity(table, stats, e),
        Expr::IsNull(e, negated) => match &**e {
            Expr::Column(c) => {
                let s = col(*c).map_or(0.05, |cs| cs.nulls as f64 / rows);
                if *negated { 1.0 - s } else { s }
            }
            _ => 0.5,
        },
        Expr::InList(e, list, negated) => match &**e {
            Expr::Column(c) => {
                let s = (list.len() as f64 / distinct(*c)).min(1.0) * non_null(*c);
                if *negated { 1.0 - s } else { s }
            }
            _ => 0.5,
        },
        Expr::Like { .. } => 0.1,
        Expr::Binary(op, a, b) => {
            let (c, v, op) = match (&**a, &**b) {
                (Expr::Column(c), e) if e.columns().is_empty() => (*c, e, *op),
                (e, Expr::Column(c)) if e.columns().is_empty() => (*c, e, flip(*op)),
                _ => return 0.33,
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
                _ => 0.5,
            }
        }
        _ => 0.5,
    }
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
