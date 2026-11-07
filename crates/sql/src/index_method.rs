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
