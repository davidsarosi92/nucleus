//! Table storage as a module (PLAN.md §M14.20 V6, principle 12): how a
//! table's rows are kept is one implementation of [`TableStorage`] among
//! those registered — `row` built in (a row's values encoded together
//! under its key, [`crate::large`]'s values out of line) and `column`
//! ([`crate::column_store`]) built in, others added by modules the same way ([`crate::ext::Registry::add_storage`]).  A table
//! names its storage (`CREATE TABLE … WITH (storage = name)`,
//! [`crate::catalog::Table::storage`]).
//!
//! The SQL layer reads and writes rows only through the trait: by *row
//! key* (the table's primary index prefix and the row's primary key, or
//! hidden row id), and by ranges of row keys — so primary key lookups,
//! scans, parallel splits and secondary indexes (which hold row keys)
//! work for every storage.  A storage keeps everything under the table's
//! data prefix ([`crate::catalog::Table::data_prefix`]), in the index ids
//! it names ([`TableStorage::index_ids`]): dropping or truncating a table
//! removes the prefix whatever its storage.

use std::collections::HashMap;
use std::ops::Bound;
use std::sync::{Arc, OnceLock, RwLock};

use nucleus_kv::KvTxn;

use crate::catalog::Table;
use crate::enums::Labels;
use crate::error::{Error, Result};
use crate::types::Row;
use crate::vector::Batch;

/// The built-in storage's name: rows encoded whole under their keys.
pub const ROW: &str = "row";

/// A range of a table's rows to read.
#[derive(Clone)]
pub struct Scan<'a> {
    /// The transaction.
    pub txn: &'a dyn KvTxn,
    /// The table.
    pub table: &'a Table,
    /// The first row key.
    pub start: Vec<u8>,
    /// The row key the range ends before (`None`: the table's end).
    pub end: Option<Vec<u8>>,
    /// The columns to decode (the others read as `NULL`; `None`: all).
    pub wanted: Option<&'a [bool]>,
    /// Reads checked at commit (foreign key checks).
    pub validated: bool,
    /// What the rows read must satisfy, as far as the plan knows (a
    /// storage may skip what these rule out; the rows are filtered above
    /// it anyway).
    pub bounds: Vec<ColumnBound>,
}

/// A range a column's values must lie in (`NULL` never does).
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnBound {
    /// The column.
    pub column: usize,
    /// The least value.
    pub lower: Bound<crate::types::Value>,
    /// The greatest.
    pub upper: Bound<crate::types::Value>,
}

/// Rows with their row keys, ascending.
pub type RowIter<'a> = Box<dyn Iterator<Item = Result<(Vec<u8>, Row)>> + 'a>;

/// A scan's rows a batch at a time (the vectorized executor's input).
pub trait BatchSource {
    /// The next batch of at most `max` rows, every position in play;
    /// `None` at the end.
    fn next_batch(&mut self, max: usize) -> Result<Option<Batch>>;
}

/// How a table's rows are stored.
pub trait TableStorage: Send + Sync {
    /// Its name, as `CREATE TABLE … WITH (storage = name)` gives it.
    fn name(&self) -> &str;

    /// The index ids under the table's data prefix it keeps `t`'s data in
    /// (the integrity check reports keys under others).
    fn index_ids(&self, t: &Table) -> Vec<u32>;

    /// Stores `row` under row key `key`: a new row, or (`old`) one replacing
    /// the row stored there.
    fn write(
        &self,
        txn: &mut dyn KvTxn,
        t: &Table,
        key: &[u8],
        row: &Row,
        old: Option<&Row>,
    ) -> Result<()>;

    /// Stores a new row under `key` unless one is there (`false`).  By
    /// default [`exists`](Self::exists), then [`write`](Self::write); a
    /// storage may tell both from one look.
    fn insert(&self, txn: &mut dyn KvTxn, t: &Table, key: &[u8], row: &Row) -> Result<bool> {
        if self.exists(&*txn, t, key, false)? {
            return Ok(false);
        }
        self.write(txn, t, key, row, None)?;
        Ok(true)
    }

    /// Removes the row under `key`.
    fn remove(&self, txn: &mut dyn KvTxn, t: &Table, key: &[u8]) -> Result<()>;

    /// Whether a row is stored under `key` (`validated`: the read is
    /// checked at commit).
    fn exists(&self, txn: &dyn KvTxn, t: &Table, key: &[u8], validated: bool) -> Result<bool>;

    /// The row under `key`, the columns `wanted` marks decoded (`labels`:
    /// the enums looked up so far, kept across a scan's lookups).
    fn get(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        key: &[u8],
        wanted: Option<&[bool]>,
        labels: &mut Labels,
    ) -> Result<Option<Row>>;

    /// The rows of a range, ascending by key.
    fn rows<'a>(&self, scan: Scan<'a>) -> Result<RowIter<'a>>;

    /// The rows of a range a batch at a time.  By default from
    /// [`rows`](Self::rows), a value per cell.
    fn batches<'a>(&self, scan: Scan<'a>) -> Result<Box<dyn BatchSource + 'a>> {
        Ok(Box::new(RowsAsBatches(self.rows(scan)?)))
    }

    /// Up to `parts - 1` row keys inside `[start, end)` that cut it into
    /// parts of about equal size, ascending (parallel scans).  By default
    /// the KV's split points of the range.
    fn split_points(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        start: &[u8],
        end: Option<&[u8]>,
        parts: usize,
    ) -> Result<Vec<Vec<u8>>> {
        let _ = t;
        Ok(txn.split_points(start, end, parts)?)
    }

    /// Called after a statement wrote rows of `t` (a storage that keeps
    /// recent writes apart may merge them in).  By default nothing.
    fn after_writes(&self, txn: &mut dyn KvTxn, t: &Table) -> Result<()> {
        let _ = (txn, t);
        Ok(())
    }

    /// Called after a statement that wrote rows of `t` failed (its writes
    /// are rolled back with it).  By default nothing.
    fn after_failure(&self, t: &Table) {
        let _ = t;
    }

    /// Every row, checked as stored, problems noted in `problems`
    /// (`crate::check`).  By default, its rows.
    fn check(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        problems: &mut Vec<String>,
    ) -> Result<Vec<(Vec<u8>, Row)>> {
        let start = t.index_prefix(0);
        let end = nucleus_kv::prefix_end(&start);
        let mut out = Vec::new();
        let scan = Scan {
            txn,
            table: t,
            start,
            end,
            wanted: None,
            validated: false,
            bounds: Vec::new(),
        };
        for r in self.rows(scan)? {
            match r {
                Ok(kr) => out.push(kr),
                Err(e) => problems.push(format!("{}: a row does not read: {e}", t.name)),
            }
        }
        Ok(out)
    }
}

/// Batches over a storage's rows.
struct RowsAsBatches<'a>(RowIter<'a>);

impl BatchSource for RowsAsBatches<'_> {
    fn next_batch(&mut self, max: usize) -> Result<Option<Batch>> {
        let mut rows = Vec::new();
        while rows.len() < max {
            match self.0.next() {
                Some(r) => rows.push(r?.1),
                None => break,
            }
        }
        Ok(crate::vector::batch_of_rows(rows))
    }
}

// ------------------------------------------------------------- statements

thread_local! {
    /// The tables the current statement wrote rows of.
    static WRITTEN: std::cell::RefCell<HashMap<u32, Table>> =
        std::cell::RefCell::new(HashMap::new());
}

/// Notes that the current statement wrote rows of `t`.
pub fn written(t: &Table) {
    WRITTEN.with(|w| {
        w.borrow_mut().entry(t.id).or_insert_with(|| t.clone());
    });
}

/// After a statement: each table it wrote, its storage's
/// [`TableStorage::after_writes`] — when it went through (`ok`).
pub fn statement_done(txn: &mut dyn KvTxn, ok: bool) -> Result<()> {
    let tables: Vec<Table> = WRITTEN.with(|w| w.borrow_mut().drain().map(|(_, t)| t).collect());
    if !ok {
        for t in &tables {
            if let Ok(s) = of(t) {
                s.after_failure(t);
            }
        }
        return Ok(());
    }
    for t in tables {
        of(&t)?.after_writes(txn, &t)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- registry

fn registry() -> &'static RwLock<HashMap<String, Arc<dyn TableStorage>>> {
    static STORAGES: OnceLock<RwLock<HashMap<String, Arc<dyn TableStorage>>>> = OnceLock::new();
    STORAGES.get_or_init(|| {
        let row: Arc<dyn TableStorage> = row_storage();
        let column: Arc<dyn TableStorage> = Arc::new(crate::column_store::ColumnStorage);
        let both: Arc<dyn TableStorage> = Arc::new(crate::column_store::HtapStorage);
        RwLock::new(HashMap::from([
            (ROW.to_string(), row),
            (crate::column_store::COLUMN.to_string(), column),
            (crate::column_store::ROW_COLUMN.to_string(), both),
        ]))
    })
}

fn row_storage() -> Arc<dyn TableStorage> {
    static ROW_STORAGE: OnceLock<Arc<dyn TableStorage>> = OnceLock::new();
    ROW_STORAGE.get_or_init(|| Arc::new(RowStorage)).clone()
}

/// Makes `s` available to tables by its name (a module's, replacing one
/// of the name registered before — the same module in another registry).
pub fn register(s: Arc<dyn TableStorage>) -> Result<()> {
    if [
        ROW,
        crate::column_store::COLUMN,
        crate::column_store::ROW_COLUMN,
    ]
    .contains(&s.name())
    {
        return Err(Error::invalid_parameter(format!(
            "the {} storage is built in",
            s.name()
        )));
    }
    registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(s.name().to_string(), s);
    Ok(())
}

/// The storage called `name`.
pub fn named(name: &str) -> Result<Arc<dyn TableStorage>> {
    if name == ROW {
        return Ok(row_storage());
    }
    registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .cloned()
        .ok_or_else(|| Error::undefined_object(format!("there is no table storage \"{name}\"")))
}

/// The storage of `t`.
pub fn of(t: &Table) -> Result<Arc<dyn TableStorage>> {
    named(&t.storage)
}

/// The registered storages' names, sorted.
pub fn names() -> Vec<String> {
    let mut out: Vec<String> = registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect();
    out.sort();
    out
}

// --------------------------------------------------------------------- row

/// The built-in storage: each row's values encoded together under its key
/// ([`crate::encoding`]), a long value in chunks under the table's
/// [`crate::large::LARGE_INDEX`].
pub struct RowStorage;

impl TableStorage for RowStorage {
    fn name(&self) -> &str {
        ROW
    }

    fn index_ids(&self, t: &Table) -> Vec<u32> {
        let mut ids = vec![0, crate::large::LARGE_INDEX];
        if t.copy == crate::column_store::CopyState::DroppingColumns {
            ids.extend(crate::column_store::COPY_INDEXES); // being removed
        }
        ids
    }

    fn write(
        &self,
        txn: &mut dyn KvTxn,
        t: &Table,
        key: &[u8],
        row: &Row,
        old: Option<&Row>,
    ) -> Result<()> {
        crate::large::write(txn, t, key, row, old)
    }

    fn insert(&self, txn: &mut dyn KvTxn, t: &Table, key: &[u8], row: &Row) -> Result<bool> {
        crate::large::insert(txn, t, key, row)
    }

    fn remove(&self, txn: &mut dyn KvTxn, t: &Table, key: &[u8]) -> Result<()> {
        crate::large::remove(txn, t, key)
    }

    fn exists(&self, txn: &dyn KvTxn, _: &Table, key: &[u8], validated: bool) -> Result<bool> {
        Ok(if validated {
            txn.get_validated(key)?
        } else {
            txn.get(key)?
        }
        .is_some())
    }

    fn get(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        key: &[u8],
        wanted: Option<&[bool]>,
        labels: &mut Labels,
    ) -> Result<Option<Row>> {
        match txn.get(key)? {
            None => Ok(None),
            Some(v) => Ok(Some(crate::large::read_some(
                txn, t, key, &v, labels, wanted,
            )?)),
        }
    }

    fn rows<'a>(&self, scan: Scan<'a>) -> Result<RowIter<'a>> {
        let end = scan
            .end
            .as_deref()
            .map_or(Bound::Unbounded, Bound::Excluded);
        let mut entries = crate::exec::Entries::over(
            scan.txn,
            scan.validated,
            Bound::Included(&scan.start),
            end,
        )?;
        let mut labels = Labels::default();
        let (txn, t, wanted) = (scan.txn, scan.table, scan.wanted);
        Ok(Box::new(std::iter::from_fn(move || {
            let row = (|| {
                let Some((k, v)) = entries.next()? else {
                    return Ok(None);
                };
                let row = crate::large::read_some(txn, t, k, v, &mut labels, wanted)?;
                Ok(Some((k.to_vec(), row)))
            })();
            row.transpose()
        })))
    }

    fn batches<'a>(&self, scan: Scan<'a>) -> Result<Box<dyn BatchSource + 'a>> {
        Ok(Box::new(crate::vector::RowBatches::new(scan)?))
    }

    fn check(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        problems: &mut Vec<String>,
    ) -> Result<Vec<(Vec<u8>, Row)>> {
        crate::check::row_storage(txn, t, problems)
    }
}
