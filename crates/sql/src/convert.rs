//! Changing how a table is stored, online (PLAN.md §M14.20 V8): `ALTER
//! TABLE t SET STORAGE row | column | row, column [(columns)]`.
//!
//! A conversion runs in transactions of its own, a part at a time, while
//! other sessions read and write the table; each switch of state is a
//! small transaction of its own:
//!
//! - **a column copy** (from `row`): the table becomes `row, column` with
//!   the copy being built — every write from then on notes its key among the
//!   changed ([`crate::column_store::CHANGED`]) — and segments are written
//!   from the rows a part at a time, after what is built; then the copy is
//!   ready (the changed keys go into segments as merges come).
//! - **a row copy** (from `column`): the delta store is merged and the table
//!   becomes `row, column` with the row copy being built — reads go to the
//!   column copy, writes to the row copy — and the segments' rows not written
//!   since are copied a segment at a time.
//! - **dropping the column copy** (to `row`): the table becomes `row` at
//!   once; the copy's keys are removed afterwards, a part at a time.
//! - **dropping the row copy** (to `column`): the changed keys go into
//!   segments, the table becomes `column`, and the row copy's keys are
//!   removed afterwards.
//!
//! Writes meet a conversion only on the keys both touch (markers are by
//! row, so a merge does not conflict with deletes).  Each switch rewrites
//! the table's record, and every write reads it validated
//! ([`crate::catalog::validate_table`]), so a write that began under the
//! old state fails (`40001`) and runs again rather than slip past the
//! switch; a switch that takes a delta in whole reads it validated.  A
//! conversion cut short (a crash) leaves the table in a state that reads
//! and writes correctly; running it again finishes it.

use std::sync::Arc;

use nucleus_kv::{Kv, KvTxn, TxnOptions, prefix_end};

use crate::catalog::{self, Table};
use crate::column_store::{CHANGED, COLUMN, CopyState, ROW_COLUMN, SEGMENT_ROWS};
use crate::error::{Error, Result};
use crate::storage::{ROW, RowStorage, Scan, TableStorage};

/// Where a conversion goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Rows only.
    Row,
    /// Columns only.
    Column,
    /// Both; the column copy of these columns (empty: all).
    Both(Vec<usize>),
}

/// The target of `SET STORAGE kinds (columns)` on `t`.
pub fn target(t: &Table, kinds: &[String], columns: Option<&[String]>) -> Result<Target> {
    let mut k: Vec<&str> = kinds.iter().map(String::as_str).collect();
    k.sort_unstable();
    k.dedup();
    let cols = |names: &[String]| -> Result<Vec<usize>> {
        let mut out: Vec<usize> = names
            .iter()
            .map(|n| {
                t.column(n).ok_or_else(|| {
                    Error::undefined_column(format!(
                        "column \"{n}\" of relation \"{}\" does not exist",
                        t.name
                    ))
                })
            })
            .collect::<Result<_>>()?;
        out.sort_unstable();
        out.dedup();
        Ok(out)
    };
    Ok(match (k.as_slice(), columns) {
        (["row"], None) => Target::Row,
        (["column"], None) => Target::Column,
        (["column"], Some(c)) | (["column", "row"], Some(c)) => Target::Both(cols(c)?),
        (["column", "row"], None) => Target::Both(Vec::new()),
        _ => {
            return Err(Error::invalid_parameter(format!(
                "SET STORAGE takes row, column or row, column (columns), not {}",
                kinds.join(", ")
            )));
        }
    })
}

/// Where `t` is now.
fn current(t: &Table) -> Target {
    match t.storage.as_str() {
        COLUMN => Target::Column,
        ROW_COLUMN => Target::Both(t.columnar.clone()),
        _ => Target::Row,
    }
}

/// Runs `f` in a transaction, again while it conflicts.
fn attempt<T>(kv: &Arc<dyn Kv>, mut f: impl FnMut(&mut dyn KvTxn) -> Result<T>) -> Result<T> {
    let mut tries = 0;
    loop {
        let mut txn = kv.begin(TxnOptions::default())?;
        let r = f(txn.as_mut()).and_then(|v| txn.commit().map(|_| v).map_err(Error::from));
        match r {
            Err(e) if e.code == "40001" && tries < 100 => {
                tries += 1;
                kv.back_off(tries);
            }
            other => return other,
        }
    }
}

fn fresh(txn: &dyn KvTxn, name: &str) -> Result<Table> {
    catalog::table(txn, name)
}

fn with(t: &Table, storage: &str, columnar: Vec<usize>, copy: CopyState) -> Table {
    let mut t = t.clone();
    t.storage = storage.to_string();
    t.columnar = columnar;
    t.copy = copy;
    t
}

/// Keys removed per transaction when a copy is dropped.
const CLEAN_KEYS: usize = 4096;
/// Rows copied per transaction into a row copy being built (few: each part
/// may meet the writes of others to the same rows, and run again).
const COPY_ROWS: usize = 1024;

/// Changes `name`'s storage to `to`, online (see the module).
pub fn set_storage(kv: &Arc<dyn Kv>, name: &str, to: Target) -> Result<()> {
    loop {
        let t = attempt(kv, |txn| fresh(txn, name))?;
        // A conversion cut short: finish it first.
        match t.copy {
            CopyState::BuildingColumns => {
                build_columns(kv, name)?;
                continue;
            }
            CopyState::BuildingRows => {
                build_rows(kv, name)?;
                continue;
            }
            CopyState::DroppingColumns => {
                clean_columns(kv, name)?;
                continue;
            }
            CopyState::DroppingRows => {
                clean_rows(kv, name)?;
                continue;
            }
            CopyState::MovingFromDelta => {
                move_delta(kv, name)?;
                continue;
            }
            CopyState::MovingFromChanged => {
                move_changed(kv, name)?;
                continue;
            }
            CopyState::Ready => {}
        }
        let now = current(&t);
        if now == to {
            return Ok(());
        }
        match (&now, &to) {
            (Target::Row, Target::Column) => start_columns(kv, name, Vec::new())?,
            (Target::Row, Target::Both(cols)) => start_columns(kv, name, cols.clone())?,
            (Target::Column, _) => start_rows(kv, name)?,
            (Target::Both(c), Target::Column) if c.is_empty() => drop_rows(kv, name)?,
            // To rows, to another column copy, or to columns from a
            // partial copy: through rows.
            (Target::Both(_), _) => drop_columns(kv, name)?,
            (Target::Row, Target::Row) => return Ok(()),
        }
    }
}

fn switch(
    kv: &Arc<dyn Kv>,
    name: &str,
    storage: &str,
    copy: CopyState,
    columnar: Option<Vec<usize>>,
) -> Result<()> {
    attempt(kv, |txn| {
        let t = fresh(txn, name)?;
        let cols = columnar.clone().unwrap_or_else(|| t.columnar.clone());
        catalog::update_table(txn, &with(&t, storage, cols, copy))
    })
}

/// Row → both: the column copy to be built.
fn start_columns(kv: &Arc<dyn Kv>, name: &str, cols: Vec<usize>) -> Result<()> {
    switch(kv, name, ROW_COLUMN, CopyState::BuildingColumns, Some(cols))?;
    build_columns(kv, name)
}

/// Builds the column copy from the rows, a segment's worth a transaction,
/// after what is built; then the copy is ready.
fn build_columns(kv: &Arc<dyn Kv>, name: &str) -> Result<()> {
    loop {
        let more = attempt(kv, |txn| {
            let t = fresh(txn, name)?;
            let start = t.index_prefix(0);
            let from = match crate::column_store::last_key(&*txn, &t)? {
                Some(mut k) => {
                    k.push(0);
                    k
                }
                None => start.clone(),
            };
            let mut rows = Vec::new();
            for r in RowStorage.rows(Scan {
                txn: &*txn,
                table: &t,
                start: from,
                end: prefix_end(&start),
                wanted: None,
                validated: false,
                bounds: Vec::new(),
            })? {
                rows.push(r?);
                if rows.len() == SEGMENT_ROWS {
                    break;
                }
            }
            if rows.is_empty() {
                return Ok(false);
            }
            crate::column_store::append_segment(txn, &t, &rows)?;
            Ok(true)
        })?;
        if !more {
            break;
        }
    }
    switch(kv, name, ROW_COLUMN, CopyState::Ready, None)?;
    // The changes so far into segments (a later merge does it otherwise).
    let _ = attempt(kv, |txn| {
        let t = fresh(txn, name)?;
        crate::column_store::take_changes(txn, &t, false)
    });
    Ok(())
}

/// Column → both: the table `row, column` at once — writes to the row copy
/// from then on — its delta store moved over after, then the row copy built.
fn start_rows(kv: &Arc<dyn Kv>, name: &str) -> Result<()> {
    // Most of the delta into segments first (fewer rows to move).
    let _ = attempt(kv, |txn| {
        let t = fresh(txn, name)?;
        crate::column_store::flush_delta(txn, &t, false)
    });
    switch(
        kv,
        name,
        ROW_COLUMN,
        CopyState::MovingFromDelta,
        Some(Vec::new()),
    )?;
    move_delta(kv, name)
}

/// Moves the delta store left from a `column` table into the row copy, a
/// part a transaction (no one writes it any more); then the row copy is
/// built.
fn move_delta(kv: &Arc<dyn Kv>, name: &str) -> Result<()> {
    while attempt(kv, |txn| {
        let t = fresh(txn, name)?;
        crate::column_store::delta_to_changed(txn, &t, COPY_ROWS)
    })? > 0
    {}
    switch(kv, name, ROW_COLUMN, CopyState::BuildingRows, None)?;
    build_rows(kv, name)
}

/// Copies the segments' rows into the row copy, a segment a transaction —
/// those not written since (the changed keys have theirs).
fn build_rows(kv: &Arc<dyn Kv>, name: &str) -> Result<()> {
    let mut after: Option<Vec<u8>> = None;
    loop {
        let next = attempt(kv, |txn| {
            let t = fresh(txn, name)?;
            crate::column_store::copy_segment_rows(txn, &t, after.as_deref(), COPY_ROWS)
        })?;
        match next {
            Some(last) => after = Some(last),
            None => break,
        }
    }
    switch(kv, name, ROW_COLUMN, CopyState::Ready, Some(Vec::new()))
}

/// Both → row: the table a row table at once, the column copy removed
/// after.
fn drop_columns(kv: &Arc<dyn Kv>, name: &str) -> Result<()> {
    switch(kv, name, ROW, CopyState::DroppingColumns, Some(Vec::new()))?;
    clean_columns(kv, name)
}

/// Removes a row table's left-over column copy, a part at a time.
fn clean_columns(kv: &Arc<dyn Kv>, name: &str) -> Result<()> {
    loop {
        let removed = attempt(kv, |txn| {
            let t = fresh(txn, name)?;
            let mut n = 0;
            for index in crate::column_store::COPY_INDEXES {
                let p = t.index_prefix(index);
                let end = prefix_end(&p).expect("an index prefix");
                let keys: Vec<Vec<u8>> = txn
                    .scan(
                        std::ops::Bound::Included(&p),
                        std::ops::Bound::Excluded(&end),
                    )?
                    .take(CLEAN_KEYS - n)
                    .map(|e| e.map(|(k, _)| k))
                    .collect::<nucleus_kv::Result<_>>()?;
                for k in &keys {
                    txn.delete(k)?;
                }
                n += keys.len();
                if n == CLEAN_KEYS {
                    break;
                }
            }
            Ok(n)
        })?;
        if removed < CLEAN_KEYS {
            break;
        }
    }
    switch(kv, name, ROW, CopyState::Ready, None)
}

/// Both (every column) → column: the table `column` at once — writes to its
/// delta store from then on — the changed keys moved over after, then the
/// row copy removed.
fn drop_rows(kv: &Arc<dyn Kv>, name: &str) -> Result<()> {
    // Most of the changed keys into segments first.
    let _ = attempt(kv, |txn| {
        let t = fresh(txn, name)?;
        crate::column_store::take_changes(txn, &t, false)
    });
    switch(
        kv,
        name,
        COLUMN,
        CopyState::MovingFromChanged,
        Some(Vec::new()),
    )?;
    move_changed(kv, name)
}

/// Moves a former `row, column` table's changed keys into the delta store,
/// a part a transaction; then the row copy goes.
fn move_changed(kv: &Arc<dyn Kv>, name: &str) -> Result<()> {
    while attempt(kv, |txn| {
        let t = fresh(txn, name)?;
        crate::column_store::changed_to_delta(txn, &t, COPY_ROWS)
    })? > 0
    {}
    switch(kv, name, COLUMN, CopyState::DroppingRows, None)?;
    clean_rows(kv, name)
}

/// Removes a column table's left-over row copy, a part at a time: each
/// row, and its values stored out of line unless its delta row (same key)
/// has them now.
fn clean_rows(kv: &Arc<dyn Kv>, name: &str) -> Result<()> {
    loop {
        let removed = attempt(kv, |txn| {
            let t = fresh(txn, name)?;
            let p = t.index_prefix(0);
            let end = prefix_end(&p).expect("an index prefix");
            let rows: Vec<(Vec<u8>, Vec<u8>)> = txn
                .scan(
                    std::ops::Bound::Included(&p),
                    std::ops::Bound::Excluded(&end),
                )?
                .take(CLEAN_KEYS)
                .collect::<nucleus_kv::Result<_>>()?;
            for (k, v) in &rows {
                let delta = txn
                    .get(&crate::column_store::delta_key(&t, k))?
                    .map(|d| crate::encoding::out_of_line(&d))
                    .transpose()?
                    .unwrap_or_default();
                let pk = crate::exec::pk_suffix(&t, k);
                for (c, _) in crate::encoding::out_of_line(v)? {
                    if !delta.iter().any(|&(d, _)| d == c) {
                        crate::large::remove_chunks(txn, &t, &pk, c)?;
                    }
                }
                txn.delete(k)?;
            }
            let p = t.index_prefix(CHANGED);
            let end = prefix_end(&p).expect("an index prefix");
            let changed: Vec<Vec<u8>> = txn
                .scan(
                    std::ops::Bound::Included(&p),
                    std::ops::Bound::Excluded(&end),
                )?
                .take(CLEAN_KEYS)
                .map(|e| e.map(|(k, _)| k))
                .collect::<nucleus_kv::Result<_>>()?;
            for k in &changed {
                txn.delete(k)?;
            }
            Ok(rows.len() + changed.len())
        })?;
        if removed == 0 {
            break;
        }
    }
    switch(kv, name, COLUMN, CopyState::Ready, None)
}

/// The storage a table of storage `name` uses (for the tests).
pub fn storage_of(t: &Table) -> Result<Arc<dyn TableStorage>> {
    crate::storage::of(t)
}
