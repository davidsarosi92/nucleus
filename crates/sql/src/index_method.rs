//! Index access methods as modules (PLAN.md §M14.12 V4b, principle 12):
//! an index is the built-in B-tree ([`crate::dml::IndexKeys`]: an entry a
//! row, its key the indexed values, its value the row's primary key) or
//! one of the methods registered here, named by `CREATE INDEX … USING
//! method (col opclass, …) WITH (option = value, …)` — as table storages
//! are ([`crate::storage`]), kept for the process.
//!
//! A method keeps everything under its index's prefix
//! ([`crate::catalog::Table::index_prefix`]): dropping the index, or
//! truncating or dropping its table, removes the prefix whatever the
//! method.  It is told every change to its table's rows in the writing
//! statement's transaction, builds itself over the rows a table has, and
//! checks itself for the integrity check.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use nucleus_kv::KvTxn;

use crate::catalog::{Index, Table};
use crate::error::{Error, Result};
use crate::ext::Registry;
use crate::types::Row;

/// The built-in method's name (`USING btree`, or no `USING`).
pub const BTREE: &str = "btree";

/// A row as an index method sees it: its primary key bytes (the row
/// key's suffix past the table's primary index prefix) and its values.
pub type RowRef<'a> = (&'a [u8], &'a Row);

/// An order a scan's rows are wanted in, for its first `k` (an `ORDER BY
/// … LIMIT k` over it): `function` of the indexed column and `args`,
/// greatest first with `desc`.
#[derive(Clone, Debug, PartialEq)]
pub struct Rank {
    /// The function ranked by.
    pub function: String,
    /// Its arguments but the indexed column (after it; for a function of
    /// two, a distance, either side of it).
    pub args: Vec<crate::types::Value>,
    /// Greatest first.
    pub desc: bool,
    /// The rows wanted.
    pub k: usize,
}

/// Rows' primary keys, one buffer.
#[derive(Default)]
pub struct Pks {
    bytes: Vec<u8>,
    at: Vec<u32>,
}

impl Pks {
    /// Adds `pk`.
    pub fn push(&mut self, pk: &[u8]) {
        if self.at.is_empty() {
            self.at.push(0);
        }
        self.bytes.extend_from_slice(pk);
        self.at.push(self.bytes.len() as u32);
    }

    /// How many.
    pub fn len(&self) -> usize {
        self.at.len().saturating_sub(1)
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The `i`th.
    pub fn get(&self, i: usize) -> &[u8] {
        &self.bytes[self.at[i] as usize..self.at[i + 1] as usize]
    }

    /// Each, in order.
    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        (0..self.len()).map(|i| self.get(i))
    }

    /// The bytes it holds.
    pub fn size(&self) -> usize {
        self.bytes.len() + 4 * self.at.len()
    }
}

/// The rows a ranking is searched among, as the plan gives them (a
/// B-tree's range of the filter, another index's condition): asked with
/// the most the method takes, `None` where there are more — they are then
/// checked on each row ranked instead.
pub type Among<'a> = &'a mut dyn FnMut(usize) -> Result<Option<Pks>>;

/// Whether a row ranked passes the scan's other conditions: told whether
/// it is one of those [`RankAsk::among`] gave.
pub type Keep<'a> = &'a mut dyn FnMut(&Row, bool) -> Result<bool>;

/// What [`IndexMethod::rank`] is asked.
pub struct RankAsk<'a> {
    /// The order and the rows wanted.
    pub rank: &'a Rank,
    /// The condition the index finds the rows of on this scan
    /// ([`IndexMethod::serves`]): its function and its arguments after
    /// the column — only its rows are ranked.  `None`: every row.
    pub served: Option<(&'a str, &'a [crate::types::Value])>,
    /// The share of the rows expected to pass `keep` (the optimizer's
    /// estimate; 1 without one).
    pub share: f64,
    /// The values of the index's columns after its first the scan's
    /// conditions hold equal (a partitioned index's).
    pub part: &'a [crate::types::Value],
    /// The rows to rank among, where the plan has them.
    pub among: Option<Among<'a>>,
    /// Each row given with its value of the function past the table's
    /// columns ([`IndexMethod::values`]).
    pub values: bool,
}

/// What the optimizer asks [`IndexMethod::rank_cost`] about.
pub struct RankCost<'a> {
    /// The function ranked by.
    pub function: &'a str,
    /// The condition the index serves on the scan, if one.
    pub served: Option<&'a str>,
    /// The rows ranked (those of the index's partition the conditions
    /// fix).
    pub rows: f64,
    /// The rows wanted, as far as known.
    pub k: f64,
    /// The share of them expected to pass the conditions checked.
    pub share: f64,
}

/// An index access method.
pub trait IndexMethod: Send + Sync {
    /// Its name, as `CREATE INDEX … USING name` gives it.
    fn name(&self) -> &str;

    /// Defines a new index (or checks one again after its column's type
    /// changed): the columns' types and operator classes, the options —
    /// refused if wrong, defaults filled in.  `UNIQUE` and taught
    /// orderings are refused before it is asked.
    fn define(&self, t: &Table, ix: &mut Index) -> Result<()>;

    /// Keeps index `i` of `t` on a write, in the writing transaction: `old`
    /// the row as it was (`None`: inserted), `new` as it is (`None`:
    /// deleted).
    fn write(
        &self,
        txn: &mut dyn KvTxn,
        t: &Table,
        i: usize,
        old: Option<RowRef>,
        new: Option<RowRef>,
    ) -> Result<()>;

    /// Builds index `i` of `t` over the rows `t` has (its prefix empty).
    fn build(&self, txn: &mut dyn KvTxn, reg: &Registry, t: &Table, i: usize) -> Result<()>;

    /// After a statement in `kv` ended outside a transaction block, or a
    /// block committed (any session's; `wrote`: one that may have
    /// written): the method's upkeep, if it keeps some for then.  By
    /// default nothing.
    fn after_statement(&self, kv: &Arc<dyn nucleus_kv::Kv>, wrote: bool) -> Result<()> {
        let _ = (kv, wrote);
        Ok(())
    }

    /// The value an operator on the index's (first) column takes from the
    /// index, as one more operand, when a statement is bound: what the
    /// operator then means on that column, with or without the index (a
    /// text index's analyzer for `@@`).  By default none.
    fn operand(&self, ix: &Index, op: &str) -> Option<crate::types::Value> {
        let _ = (ix, op);
        None
    }

    /// A function of a condition on the index's (first) column, bound as
    /// the index computes it: an expression in place of `function(arg)`,
    /// and its type — `score(body @@ q)`, BM25 over a text index's
    /// statistics.  `arg` is the bound condition (an operator's call whose
    /// first operand is the column); `t` is the index's table.  By default
    /// none, and the function is bound as any other.
    fn function(
        &self,
        t: &Table,
        i: usize,
        function: &str,
        arg: &crate::expr::Expr,
    ) -> Result<Option<(crate::expr::Expr, crate::types::DataType)>> {
        let _ = (t, i, function, arg);
        Ok(None)
    }

    /// Whether the index finds, exactly, the rows for which `function` of
    /// its (first) column and values fixed for a scan holds — a filter's
    /// condition it then serves ([`IndexMethod::rows_where`]).  By default
    /// none.
    fn serves(&self, ix: &Index, function: &str) -> bool {
        let _ = (ix, function);
        false
    }

    /// Whether the index can rank its rows by `function` of its (first)
    /// column and values fixed for the scan — greatest first with `desc`,
    /// `NULL`s first with `nulls_first` — giving the first `k`
    /// ([`IndexMethod::rank`]).  By default it cannot.
    fn ranks(&self, ix: &Index, function: &str, desc: bool, nulls_first: bool) -> bool {
        let _ = (ix, function, desc, nulls_first);
        false
    }

    /// Whether its rank can give each row's value of `function` with it,
    /// the very value the function computes of the row
    /// ([`RankAsk::values`]): the plan then reads it there and does not
    /// compute it again (a text index's BM25, PLAN.md §M14.21 T5c).  By
    /// default it cannot.
    fn values(&self, ix: &Index, function: &str) -> bool {
        let _ = (ix, function);
        false
    }

    /// What ranking costs, in rows read (a row's read as through an
    /// index), for the optimizer to weigh against the scan's: `None` where
    /// the index cannot rank them, or would read as many as the scan.  By
    /// default `None`.
    fn rank_cost(&self, ix: &Index, c: &RankCost) -> Option<f64> {
        let _ = (ix, c);
        None
    }

    /// The rows index `i` of `t` holds as `txn` sees it, where it knows
    /// them cheaply (for a table not analyzed).  By default `None`.
    fn rows(&self, txn: &dyn KvTxn, t: &Table, i: usize) -> Result<Option<usize>> {
        let _ = (txn, t, i);
        Ok(None)
    }

    /// The rows of `t` first by `ask`'s order ([`ranks`](IndexMethod::ranks))
    /// that `keep` passes, as `txn` sees them, the columns `wanted` marks
    /// read (`None`: all): at least `k` of them where there are, in any
    /// order — an exact method every row ranked as the `k`-th passing
    /// too, so the order and the limit above are exact over them.  `None`
    /// where the index cannot tell them (the rows are then read).  `keep`
    /// is asked about each row once, in the order ranked, told whether
    /// the row is one of those `ask.among` gave (the condition that chose
    /// them is then not checked again); `None`: every row passes.
    fn rank(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        i: usize,
        ask: &mut RankAsk,
        wanted: Option<&[bool]>,
        keep: Option<Keep>,
    ) -> Result<Option<Vec<(Vec<u8>, Row)>>> {
        let _ = (txn, t, i, ask, wanted, keep);
        Ok(None)
    }

    /// The primary keys of the rows of `t`, as `txn` sees them, for which
    /// `function(column, args…)` holds, sorted — `None` where the index
    /// cannot tell for these values (the rows are then read).
    fn rows_where(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        i: usize,
        function: &str,
        args: &[crate::types::Value],
    ) -> Result<Option<Vec<Vec<u8>>>> {
        let _ = (txn, t, i, function, args);
        Ok(None)
    }

    /// The integrity check: index `i` of `t` against `rows` (every row of
    /// the table, primary key bytes and values); what is wrong goes to
    /// `problems`.
    fn check(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        i: usize,
        rows: &[(Vec<u8>, Row)],
        problems: &mut Vec<String>,
    ) -> Result<()>;
}

/// Every row of `t` as a method's build reads them: primary key bytes and
/// values, of the columns `wanted` (the others `NULL`; `None`: all).
pub fn rows<'a>(
    txn: &'a dyn KvTxn,
    t: &'a Table,
    wanted: Option<&'a [bool]>,
) -> Result<crate::storage::RowIter<'a>> {
    let start = t.index_prefix(0);
    let end = nucleus_kv::prefix_end(&start);
    let rows = crate::storage::of(t)?.rows(crate::storage::Scan {
        txn,
        table: t,
        start,
        end,
        wanted,
        validated: false,
        bounds: Vec::new(),
        dicts: Vec::new(),
    })?;
    Ok(Box::new(rows.map(|r| {
        r.map(|(key, row)| (key[crate::catalog::INDEX_PREFIX_LEN..].to_vec(), row))
    })))
}

fn registry() -> &'static RwLock<HashMap<String, Arc<dyn IndexMethod>>> {
    static METHODS: OnceLock<RwLock<HashMap<String, Arc<dyn IndexMethod>>>> = OnceLock::new();
    METHODS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Makes `m` available to indexes by its name (a module's, replacing one
/// of the name registered before — the same module in another registry).
pub fn register(m: Arc<dyn IndexMethod>) -> Result<()> {
    if m.name() == BTREE {
        return Err(Error::invalid_parameter(
            "the btree index method is built in",
        ));
    }
    registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(m.name().to_string(), m);
    Ok(())
}

/// The method called `name`: `None` for the B-tree.
pub fn named(name: &str) -> Result<Option<Arc<dyn IndexMethod>>> {
    if name == BTREE {
        return Ok(None);
    }
    registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .cloned()
        .map(Some)
        .ok_or_else(|| Error::undefined_object(format!("access method \"{name}\" does not exist")))
}

/// The method of `ix`: `None` for a B-tree.
pub fn of(ix: &Index) -> Result<Option<Arc<dyn IndexMethod>>> {
    match &ix.method {
        None => Ok(None),
        Some(m) => named(m),
    }
}

/// After a statement or a commit in `kv` (`wrote`: one that may have
/// written): every registered method's [`IndexMethod::after_statement`].
pub fn after_statement(kv: &Arc<dyn nucleus_kv::Kv>, wrote: bool) -> Result<()> {
    let methods: Vec<Arc<dyn IndexMethod>> = registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .cloned()
        .collect();
    for m in methods {
        m.after_statement(kv, wrote)?;
    }
    Ok(())
}

/// The registered methods' names and the B-tree's, sorted.
pub fn names() -> Vec<String> {
    let mut out: Vec<String> = registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .chain([BTREE.to_string()])
        .collect();
    out.sort();
    out
}
