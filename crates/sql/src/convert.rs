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
    /// Rows, each sealed under its subjects' key (PLAN.md §M14.30 K4a).
    Sealed,
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
        (["sealed"], None) => Target::Sealed,
        (["column"], None) => Target::Column,
        (["column"], Some(c)) | (["column", "row"], Some(c)) => Target::Both(cols(c)?),
        (["column", "row"], None) => Target::Both(Vec::new()),
        _ => {
            return Err(Error::invalid_parameter(format!(
                "SET STORAGE takes row, column, row, column (columns) or sealed, not {}",
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
        crate::sealed::SEALED => Target::Sealed,
        _ => Target::Row,
    }
}

/// Runs `f` in a transaction, again while it conflicts.
pub(crate) fn attempt<T>(
    kv: &Arc<dyn Kv>,
    mut f: impl FnMut(&mut dyn KvTxn) -> Result<T>,
) -> Result<T> {
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
            CopyState::Sealing => {
                reseal(kv, name, true)?;
                continue;
            }
            CopyState::Unsealing => {
                reseal(kv, name, false)?;
                continue;
            }
            CopyState::Ready => {}
        }
        let now = current(&t);
        if now == to {
            return Ok(());
        }
        match (&now, &to) {
            (Target::Row, Target::Sealed) => {
                switch(kv, name, crate::sealed::SEALED, CopyState::Sealing, None)?;
                reseal(kv, name, true)?;
            }
            (Target::Sealed, _) => {
                switch(kv, name, crate::sealed::SEALED, CopyState::Unsealing, None)?;
                reseal(kv, name, false)?;
            }
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
                dicts: Vec::new(),
            })? {
                rows.push(r?);
                if rows.len() == SEGMENT_ROWS {
                    break;
                }
            }
            if rows.is_empty() {
                return Ok(false);
            }
            crate::column_store::append_segment(txn, &t, rows)?;
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

/// Sealing (`seal`) or opening a `sealed` table's rows, a part at a time,
/// each part a transaction of its own: the rows stored the other way are
/// written again ([`crate::sealed::rewrite`]); then the table is `sealed`
/// (or `row`), ready.  Writes meanwhile store rows the new way already.
/// Opening them, a row whose key is destroyed is removed, and its index
/// entries after (always looked for: a conversion cut short and run again
/// does not know whether it removed any).
fn reseal(kv: &Arc<dyn Kv>, name: &str, seal: bool) -> Result<()> {
    let mut after: Option<Vec<u8>> = None;
    loop {
        let (next, _) = attempt(kv, |txn| {
            let t = fresh(txn, name)?;
            crate::sealed::rewrite(txn, &t, after.as_deref(), COPY_ROWS, seal)
        })?;
        match next {
            Some(last) => after = Some(last),
            None => break,
        }
    }
    if !seal {
        let mut after: Option<Vec<u8>> = None;
        while let Some(last) = attempt(kv, |txn| {
            let t = fresh(txn, name)?;
            crate::sealed::drop_dangling(txn, &t, after.as_deref(), CLEAN_KEYS)
        })? {
            after = Some(last);
        }
    }
    let to = if seal { crate::sealed::SEALED } else { ROW };
    switch(kv, name, to, CopyState::Ready, None)
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
    // Where each part goes on: past the keys the parts before deleted,
    // not over their tombstones again (no write puts a key behind it:
    // the table's writes go to its delta store meanwhile).
    let (mut rows_at, mut changed_at) = (None::<Vec<u8>>, None::<Vec<u8>>);
    loop {
        let removed = attempt(kv, |txn| {
            let t = fresh(txn, name)?;
            let p = t.index_prefix(0);
            let end = prefix_end(&p).expect("an index prefix");
            let rows: Vec<(Vec<u8>, Vec<u8>)> = txn
                .scan(
                    std::ops::Bound::Included(rows_at.as_ref().unwrap_or(&p)),
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
                        crate::large::remove_chunks(txn, &t, pk, c)?;
                    }
                }
                txn.delete(k)?;
            }
            let p = t.index_prefix(CHANGED);
            let end = prefix_end(&p).expect("an index prefix");
            let changed: Vec<Vec<u8>> = txn
                .scan(
                    std::ops::Bound::Included(changed_at.as_ref().unwrap_or(&p)),
                    std::ops::Bound::Excluded(&end),
                )?
                .take(CLEAN_KEYS)
                .map(|e| e.map(|(k, _)| k))
                .collect::<nucleus_kv::Result<_>>()?;
            for k in &changed {
                txn.delete(k)?;
            }
            let past = |k: &[u8]| [k, &[0]].concat();
            Ok((
                rows.len() + changed.len(),
                rows.last().map(|(k, _)| past(k)),
                changed.last().map(|k| past(k)),
            ))
        })?;
        if removed.0 == 0 {
            break;
        }
        rows_at = removed.1.or(rows_at);
        changed_at = removed.2.or(changed_at);
    }
    switch(kv, name, COLUMN, CopyState::Ready, None)
}

/// The storage a table of storage `name` uses (for the tests).
pub fn storage_of(t: &Table) -> Result<Arc<dyn TableStorage>> {
    crate::storage::of(t)
}

/// Keys a part of a move copies or cleans (PLAN.md §M14.28 L3b), at
/// first: a part that commits at once doubles the next, up to
/// [`PART_MOST`]; one that meets writes of the same keys runs again with
/// half, down to [`PART_LEAST`] — few keys where writers are busy, a
/// commit's sync seldom where they are not.
const PART_FIRST: usize = 1024;
const PART_LEAST: usize = 64;
const PART_MOST: usize = 1 << 16;
/// Bytes a part takes, at most about.
const PART_BYTES: usize = 16 << 20;

/// Moves `name`'s keys into `to` (`ALTER TABLE … SET (engine = …)`,
/// PLAN.md §M14.28 L3b), online: under a new table id whose key range is
/// `to`'s.  A partitioned table moves each partition, and its own
/// engine (its next partitions') changes.
///
/// 1. *The claim*: a transaction takes the new id, records it as the
///    table's move ([`catalog::moving_to`]) and writes the table's record
///    again — so a write that read the record before fails, and runs
///    again under the mirror — while the store mirrors every write under
///    the table's prefixes to the new id's ([`Kv::mirror`]).
/// 2. *The copy*: the keys under the old prefixes go under the new ones,
///    a part a transaction; a part that meets a mirrored write of a key
///    it copies runs again, and a mirrored write committed after a part
///    wins over it.
/// 3. *The switch*: a transaction reads the record validated and writes
///    it with the new id and engine — writes begun before fail, and run
///    again on the new keys.
/// 4. *The clean*: the old keys go, a part a transaction.
///
/// A move cut short (a crash; the mirror is the process's) leaves the
/// table where it was, its copy stale: the next move, or `DROP TABLE`,
/// removes it first.  Snapshots older than the switch read the old keys
/// as they read every deleted key, until the vacuum takes them.
pub fn set_engine(kv: &Arc<dyn Kv>, name: &str, to: catalog::TableEngine) -> Result<()> {
    let t = attempt(kv, |txn| fresh(txn, name))?;
    if t.partition_by.is_some() {
        let parts = attempt(kv, |txn| crate::partition::partitions(txn, &t))?;
        for p in parts {
            set_engine(kv, &p.name, to)?;
        }
        return attempt(kv, |txn| {
            let mut t = fresh(txn, name)?;
            t.engine = to;
            catalog::update_table(txn, &t)
        });
    }
    // One move of a table at a time in this process; one recorded but
    // not under way here was cut short, and its copy is stale.
    let _moving = Moving::begin(kv, t.id)?;
    if let Some(stale) = attempt(kv, |txn| catalog::moving_to(txn, t.id))? {
        clean_prefixes(kv, &catalog::id_prefixes(stale))?;
        attempt(kv, |txn| catalog::set_moving(txn, t.id, None))?;
    }
    if t.engine == to {
        return Ok(());
    }
    // A column copy's directory made before keys its segments by whole
    // row keys, which hold the table's id: rewritten first.
    if !t.relative_directory {
        attempt(kv, |txn| {
            let t = fresh(txn, name)?;
            movable(&t)?;
            crate::column_store::relativize(txn, &t).map(drop)
        })?;
    }
    let mut mirror = None;
    let mut claimed = Vec::new();
    let claim = attempt(kv, |txn| {
        let t = fresh(txn, name)?;
        movable(&t)?;
        mirror = None; // a claim run again mirrors anew
        let id = catalog::claim_id(txn, to)?;
        claimed.push(id);
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = catalog::id_prefixes(t.id)
            .into_iter()
            .zip(catalog::id_prefixes(id))
            .collect();
        mirror = Some(kv.mirror(&pairs)?);
        catalog::set_moving(txn, t.id, Some(id))?;
        catalog::update_table(txn, &t)?;
        Ok((t.id, id))
    });
    // What the claims that did not commit mirrored into their ids goes.
    let kept = claim.as_ref().ok().map(|&(_, id)| id);
    for id in claimed.into_iter().filter(|&id| Some(id) != kept) {
        let _ = clean_prefixes(kv, &catalog::id_prefixes(id));
    }
    let (from_id, new_id) = match claim {
        Ok(ids) => ids,
        Err(e) => {
            drop(mirror);
            return Err(e);
        }
    };
    let moved = (|| {
        for (from, into) in catalog::id_prefixes(from_id)
            .into_iter()
            .zip(catalog::id_prefixes(new_id))
        {
            copy_prefix(kv, &from, &into)?;
        }
        attempt(kv, |txn| {
            catalog::validate_table(txn, name)?;
            let mut t = fresh(txn, name)?;
            if t.id != from_id || catalog::moving_to(txn, from_id)? != Some(new_id) {
                return Err(Error::object_in_use(format!(
                    "table \"{name}\" was dropped or replaced while its engine changed"
                )));
            }
            movable(&t)?;
            t.id = new_id;
            t.engine = to;
            catalog::set_moving(txn, from_id, None)?;
            catalog::update_table(txn, &t)
        })
    })();
    drop(mirror);
    match moved {
        Ok(()) => clean_prefixes(kv, &catalog::id_prefixes(from_id)),
        Err(e) => {
            // The copy goes; the table stays where it was.
            let _ = clean_prefixes(kv, &catalog::id_prefixes(new_id)).and_then(|()| {
                attempt(kv, |txn| {
                    if catalog::moving_to(txn, from_id)? == Some(new_id) {
                        catalog::set_moving(txn, from_id, None)?;
                    }
                    Ok(())
                })
            });
            Err(e)
        }
    }
}

/// The tables whose engine this process is changing: (the store's
/// instance, its branch, the table's id).
static MOVES: std::sync::Mutex<Vec<(u64, String, u32)>> = std::sync::Mutex::new(Vec::new());

/// A table's move under way in this process, while it lives.
struct Moving((u64, String, u32));

impl Moving {
    fn begin(kv: &Arc<dyn Kv>, id: u32) -> Result<Self> {
        let key = (kv.instance(), kv.branch_name(), id);
        let mut moves = MOVES.lock().unwrap_or_else(|e| e.into_inner());
        if moves.contains(&key) {
            return Err(Error::object_in_use(
                "the table's engine is being changed by another statement".to_string(),
            ));
        }
        moves.push(key.clone());
        Ok(Moving(key))
    }
}

impl Drop for Moving {
    fn drop(&mut self) {
        MOVES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|k| k != &self.0);
    }
}

/// Refuses a move of `t` its keys cannot make as they are.
fn movable(t: &Table) -> Result<()> {
    if t.storage == crate::sealed::SEALED {
        // Each sealed row is bound to its key (the associated data).
        return Err(Error::feature(format!(
            "table \"{}\" is sealed: its rows are bound to their keys; SET STORAGE row first",
            t.name
        )));
    }
    if t.copy != CopyState::Ready {
        return Err(Error::object_in_use(format!(
            "table \"{}\" is changing its storage: ALTER TABLE … SET STORAGE again finishes it",
            t.name
        )));
    }
    if let Some(cv) = &t.converting {
        return Err(Error::object_in_use(format!(
            "the type of column \"{}\" of \"{}\" is being changed; its engine can change when that is done",
            t.columns[cv.column].name, t.name
        )));
    }
    if let Some(ix) = t.building.first() {
        return Err(Error::object_in_use(format!(
            "index \"{}\" of \"{}\" is being built; its engine can change when that is done",
            ix.name, t.name
        )));
    }
    Ok(())
}

/// Copies every key under `from` to the same under `into`, a part a
/// transaction.
fn copy_prefix(kv: &Arc<dyn Kv>, from: &[u8], into: &[u8]) -> Result<()> {
    parts(kv, from, |txn, k, v| {
        txn.put(&[into, &k[from.len()..]].concat(), v)
    })
}

/// Deletes every key under `prefixes`, a part a transaction.
fn clean_prefixes(kv: &Arc<dyn Kv>, prefixes: &[Vec<u8>]) -> Result<()> {
    for p in prefixes {
        parts(kv, p, |txn, k, _| txn.delete(k))?;
    }
    Ok(())
}

/// Runs `f` on every entry under `prefix`, a part a transaction, each
/// part read from where the last one ended (not from the prefix again,
/// past what the parts before deleted), its size adapting
/// ([`PART_FIRST`]).
fn parts(
    kv: &Arc<dyn Kv>,
    prefix: &[u8],
    mut f: impl FnMut(&mut dyn KvTxn, &[u8], &[u8]) -> nucleus_kv::Result<()>,
) -> Result<()> {
    let end = prefix_end(prefix).expect("a table's prefix below the last");
    let mut at = prefix.to_vec();
    let mut size = PART_FIRST;
    loop {
        let mut tries = 0;
        let (full, next) = attempt(kv, |txn| {
            tries += 1;
            if tries > 1 {
                size = (size / 2).max(PART_LEAST);
            }
            let mut taken = Vec::new();
            let mut bytes = 0;
            for e in txn.scan(
                std::ops::Bound::Included(&at),
                std::ops::Bound::Excluded(&end),
            )? {
                let (k, v) = e?;
                bytes += k.len() + v.len();
                taken.push((k, v));
                if taken.len() == size || bytes >= PART_BYTES {
                    break;
                }
            }
            for (k, v) in &taken {
                f(txn, k, v)?;
            }
            let full = taken.len() == size || bytes >= PART_BYTES;
            // Past the last key taken: it and a zero byte.
            let next = taken.last().map(|(k, _)| [k.as_slice(), &[0]].concat());
            Ok((full, next))
        })?;
        match next {
            Some(k) if full => at = k,
            _ => return Ok(()),
        }
        if tries == 1 {
            size = (size * 2).min(PART_MOST);
        }
    }
}
