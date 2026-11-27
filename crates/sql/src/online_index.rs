//! Building an index online (issue #18): `CREATE [UNIQUE] INDEX
//! CONCURRENTLY`, and a `CREATE INDEX` run alone whose build in one
//! transaction met a write committed under it.
//!
//! The build runs in transactions of its own while other sessions read and
//! write the table; its state is the table's record.  Every write reads
//! that record validated, so a write begun under one state and committed
//! after the next fails (`40001`) and runs again under it (as
//! [`crate::retype`]'s and [`crate::convert`]'s changes do).
//!
//! - **Begin** (one transaction): the index joins the table's indexes
//!   being built ([`Table::building`]).  Writers keep its entries as any
//!   index's — a unique one refuses a duplicate it sees — while readers and
//!   the planner do not see it.
//! - **Backfill**: every row's entry written, a part of the table at a
//!   time in primary key order.  A part reads its rows validated, so a
//!   write to them committed meanwhile fails the part, which runs again
//!   smaller.  A row's entry may be there already — a writer wrote it — or,
//!   in a unique index, another row's: a duplicate, which fails the build.
//! - **Ready** (one transaction): the index moves to the table's indexes.
//! - **Abandon**, when the build fails (a duplicate, the statement's
//!   timeout or cancel): the index and its entries are removed.
//!
//! A row written after the begin has its entry from its writer; a row
//! written before has it from the part that read it, which committed only
//! if the row stayed as read.  The one entry a writer may meet that is not
//! its row's is a unique index's, of another row with the same value
//! (before the build finds the duplicate): a writer removes a unique entry
//! of an index being built only when it is its row's
//! ([`crate::dml::drop_entry`]).
//!
//! Prior art: PostgreSQL's `CREATE INDEX CONCURRENTLY` (an index first
//! committed as maintained by writers but not read, then built, then
//! made valid) and F1's online schema change (Rae et al., VLDB 2013: a
//! write-only state between absent and public, so that any two adjacent
//! schema versions are compatible, and a backfill under it).  Here the
//! backfill needs no second pass over the rows written meanwhile — each
//! part's rows are a premise of its commit — and no wait for older
//! transactions — the table's validated record turns them away.
//!
//! A build cut short (a crash, the session gone) leaves an index being
//! built, kept by writers and seen by no reader; running the statement
//! again, or the server's maintenance thread ([`resume_all`]), finishes
//! it.

use std::sync::Arc;

use nucleus_kv::{Kv, KvTxn, TxnOptions};

use crate::catalog::{self, Index, Table};
use crate::convert::attempt;
use crate::dml::IndexKeys;
use crate::error::{Error, Result};
use crate::exec::pk_suffix;
use crate::ext::Registry;

/// Why index `ix` of `t` cannot be built online, if it cannot.
pub(crate) fn refusal(t: &Table, ix: &Index) -> Option<&'static str> {
    if t.partition_by.is_some() || t.partition_of.is_some() {
        Some("the table is partitioned or a partition")
    } else if ix.method.is_some() {
        Some("it is an index method's")
    } else if t.memory.is_some() {
        Some("the table is a memory store")
    } else if t.converting.is_some() {
        Some("a column's type is being changed")
    } else if t.copy != crate::column_store::CopyState::Ready {
        Some("the table's storage is being changed")
    } else {
        None
    }
}

/// Whether `stmt` is DDL that, failing in one transaction over a
/// concurrent write, may run online instead ([`instead`]): a `CREATE
/// INDEX`, an `ALTER COLUMN … TYPE`, a `SET NOT NULL`, an `ADD CHECK` or
/// `ADD UNIQUE`.
pub fn may_fall_back(stmt: &crate::ast::Statement) -> bool {
    use crate::ast::{Statement, TableChange, TableConstraint};
    matches!(
        stmt,
        Statement::CreateIndex {
            concurrently: false,
            method: None,
            ..
        } | Statement::AlterColumnType {
            concurrently: false,
            ..
        } | Statement::AlterTable {
            change: TableChange::SetNotNull { not_null: true, .. },
            ..
        } | Statement::AddConstraint {
            constraint: TableConstraint::Check(..) | TableConstraint::Unique(..),
            ..
        }
    )
}

/// `stmt` ([`may_fall_back`]), whose transaction met a write committed
/// under it, run online: an index built online, a type that keeps the
/// values checked online (`crate::validating`), another converted online
/// (`crate::retype`).  Its command tag, or `None` when it cannot be (it
/// runs again as it was).
pub fn instead(
    kv: &Arc<dyn Kv>,
    reg: &Registry,
    stmt: &crate::ast::Statement,
) -> Result<Option<&'static str>> {
    Ok(match stmt {
        crate::ast::Statement::CreateIndex { .. } => {
            create(kv, reg, stmt, true)?.then_some("CREATE INDEX")
        }
        crate::ast::Statement::AlterColumnType { .. } => (crate::validating::alter(kv, reg, stmt)?
            || crate::retype::fall_back(kv, reg, stmt)?)
        .then_some("ALTER TABLE"),
        // A unique constraint is a unique index of its name.
        crate::ast::Statement::AddConstraint {
            table,
            constraint: crate::ast::TableConstraint::Unique(name, columns),
        } => {
            let index = crate::ast::Statement::CreateIndex {
                name: Some(
                    name.clone()
                        .unwrap_or_else(|| format!("{table}_{}_key", columns.join("_"))),
                ),
                table: table.clone(),
                columns: columns.clone(),
                exprs: vec![None; columns.len()],
                orderings: vec![None; columns.len()],
                method: None,
                opclasses: vec![None; columns.len()],
                options: Vec::new(),
                partition_by: Vec::new(),
                unique: true,
                if_not_exists: false,
                concurrently: false,
            };
            create(kv, reg, &index, true)?.then_some("ALTER TABLE")
        }
        _ => crate::validating::constrain(kv, reg, stmt)?.then_some("ALTER TABLE"),
    })
}

/// What [`create`] began.
enum Begun {
    /// The index is there (`IF NOT EXISTS`).
    There,
    /// It cannot be built online (and the statement may build it in one
    /// transaction).
    Refused,
    /// Its build, of the index of the table.
    Build(String, String),
}

/// `CREATE INDEX` (`stmt`) built online: begun, then built ([`build`]).
/// `fallback`: the statement's build in one transaction failed, so an
/// index that cannot be built online is not refused — `Ok(false)`, to
/// run as it was.
pub fn create(
    kv: &Arc<dyn Kv>,
    reg: &Registry,
    stmt: &crate::ast::Statement,
    fallback: bool,
) -> Result<bool> {
    create_watched(kv, reg, stmt, fallback, &mut |_| ())
}

/// [`create`], `after_part` called after each part of the backfill
/// commits, with the last row key it wrote an entry for — what a test
/// runs other sessions' writes between.
pub fn create_watched(
    kv: &Arc<dyn Kv>,
    reg: &Registry,
    stmt: &crate::ast::Statement,
    fallback: bool,
    after_part: &mut dyn FnMut(&[u8]),
) -> Result<bool> {
    let begun = attempt(kv, |txn| {
        let Some((t, ix)) = crate::session::define_index(&*txn, reg, stmt, true)? else {
            return Ok(Begun::There);
        };
        if fallback && refusal(&t, &ix).is_some() {
            return Ok(Begun::Refused);
        }
        let names = (t.name.clone(), ix.name.clone());
        begin(txn, t, ix)?;
        Ok(Begun::Build(names.0, names.1))
    })?;
    match begun {
        Begun::There => Ok(true),
        Begun::Refused => Ok(false),
        Begun::Build(table, name) => {
            build_watched(kv, reg, &table, &name, after_part).map(|()| true)
        }
    }
}

/// Whether `a` and `b` define the same index (their ids aside).
fn same_definition(a: &Index, b: &Index) -> bool {
    Index {
        id: b.id,
        ..a.clone()
    } == *b
}

/// The claim on a build run in this process (the maintenance thread
/// leaves it to its statement): keyed apart from the table names
/// [`crate::retype`]'s claims use.
fn claim(kv: &Arc<dyn Kv>, name: &str) -> Option<crate::retype::Running> {
    crate::retype::Running::claim(kv, &format!("\u{1}index {name}"))
}

/// Begins building index `ix` of table `t` (as read in `txn`): records it
/// among the indexes being built.  `Ok(false)` when the same index is
/// being built already (cut short: to finish).
pub(crate) fn begin(txn: &mut dyn KvTxn, mut t: Table, mut ix: Index) -> Result<bool> {
    if let Some(b) = t.building.iter().find(|b| b.name == ix.name) {
        if same_definition(b, &ix) {
            return Ok(false);
        }
        return Err(Error::duplicate_table(format!(
            "relation \"{}\" already exists",
            ix.name
        )));
    }
    if let Some(why) = refusal(&t, &ix) {
        return Err(Error::feature(format!(
            "CREATE INDEX CONCURRENTLY cannot build index \"{}\": {why}; without CONCURRENTLY it is built in one transaction",
            ix.name
        )));
    }
    ix.id = t.next_index_id;
    t.next_index_id += 1;
    t.building.push(ix);
    catalog::update_table(txn, &t)?;
    Ok(true)
}

/// Builds index `name` of `table`, recorded as being built (see the
/// module); on a failure the index is abandoned and the failure returned.
pub fn build(kv: &Arc<dyn Kv>, reg: &Registry, table: &str, name: &str) -> Result<()> {
    build_watched(kv, reg, table, name, &mut |_| ())
}

/// [`build`], `after_part` called as [`create_watched`]'s.
fn build_watched(
    kv: &Arc<dyn Kv>,
    reg: &Registry,
    table: &str,
    name: &str,
    after_part: &mut dyn FnMut(&[u8]),
) -> Result<()> {
    let Some(_claim) = claim(kv, name) else {
        return Err(Error::object_in_use(format!(
            "index \"{name}\" is being built by another session"
        )));
    };
    let r = backfill(kv, reg, table, name, after_part).and_then(|()| ready(kv, table, name));
    if let Err(e) = r {
        // Left to the maintenance thread (which finishes it) only when it
        // cannot be abandoned either.
        let _ = abandon(kv, table, name);
        return Err(e);
    }
    Ok(())
}

/// The position of index `name` among `t`'s indexes being built.
fn position(t: &Table, name: &str) -> Result<usize> {
    t.building
        .iter()
        .position(|ix| ix.name == name)
        .ok_or_else(|| {
            Error::object_in_use(format!(
                "index \"{name}\" was dropped while it was being built"
            ))
        })
}

/// Writes every row's entry of index `name`, a part a transaction.
fn backfill(
    kv: &Arc<dyn Kv>,
    reg: &Registry,
    table: &str,
    name: &str,
    after_part: &mut dyn FnMut(&[u8]),
) -> Result<()> {
    let part = |txn: &mut dyn KvTxn, after: Option<&[u8]>, size: usize| {
        let t = catalog::table(&*txn, table)?;
        catalog::validate_table(&*txn, table)?;
        let view = t.with_building();
        let i = t.indexes.len() + position(&t, name)?;
        let keys = IndexKeys::new(&*txn, reg, &view)?;
        let mut wanted = vec![false; t.columns.len()];
        for c in view.indexes[i].reads() {
            wanted[c] = true;
        }
        let start = match after {
            // The least key after it.
            Some(k) => [k, &[0]].concat(),
            None => t.index_prefix(0),
        };
        // The part's rows read validated: a write to them committed
        // before the part fails it.
        let mut entries = Vec::with_capacity(size);
        let mut last = None;
        {
            let rows = crate::storage::of(&t)?.rows(crate::storage::Scan {
                txn: &*txn,
                table: &t,
                start,
                end: nucleus_kv::prefix_end(&t.index_prefix(0)),
                wanted: Some(&wanted),
                validated: true,
                bounds: Vec::new(),
                dicts: Vec::new(),
            })?;
            for r in rows.take(size) {
                let (key, row) = r?;
                let pk = pk_suffix(&t, &key);
                for (k, unique) in keys.entry(&*txn, &view, &row, pk, i)? {
                    entries.push((k, pk.to_vec(), unique));
                }
                last = Some(key);
            }
        }
        crate::budget::charge_scans(entries.len() as u64)?;
        let Some(last) = last else {
            return Ok(None);
        };
        entries.sort_unstable();
        for (k, pk, unique) in entries {
            if unique {
                match txn.get_validated(&k)? {
                    Some(own) if own == pk => continue, // its writer's
                    Some(_) => {
                        return Err(Error::unique_violation(format!(
                            "could not create unique index \"{name}\": key is duplicated"
                        )));
                    }
                    None => {}
                }
            }
            txn.put(&k, &pk)?;
        }
        Ok(Some(last))
    };
    crate::retype::in_parts_watched(kv, part, after_part)
}

/// Index `name`, built, moves to `table`'s indexes.
fn ready(kv: &Arc<dyn Kv>, table: &str, name: &str) -> Result<()> {
    attempt(kv, |txn| {
        let mut t = catalog::table(&*txn, table)?;
        let ix = t.building.remove(position(&t, name)?);
        t.indexes.push(ix);
        catalog::update_table(txn, &t)
    })
}

/// Index `name` of `table`, being built, removed with its entries (the
/// entries in parts, after it is gone from the table's record).
fn abandon(kv: &Arc<dyn Kv>, table: &str, name: &str) -> Result<()> {
    let id = attempt(kv, |txn| {
        let Some(mut t) = catalog::get_table(&*txn, table)? else {
            return Ok(None); // dropped, its keys with it
        };
        let Some(b) = t.building.iter().position(|ix| ix.name == name) else {
            return Ok(None);
        };
        let id = t.building.remove(b).id;
        catalog::update_table(txn, &t)?;
        Ok(Some((t, id)))
    })?;
    match id {
        Some((t, id)) => crate::retype::remove_entries(kv, &t, id),
        None => Ok(()),
    }
}

/// `DROP INDEX` of index `b` of `t`'s being built: gone from the record,
/// its entries removed after the drop commits; the build, if it runs,
/// stops.
pub(crate) fn drop_building(txn: &mut dyn KvTxn, mut t: Table, b: usize) -> Result<()> {
    let id = t.building.remove(b).id;
    drop_entries_later(txn, &mut t, id)
}

thread_local! {
    /// Whether a statement on this thread dropped an index since
    /// [`after_statement`] last looked.
    static DROPPED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Records index `id` of `t` (gone from its indexes) as dropped and `t` in
/// `txn`: its entries are removed once the drop commits
/// ([`clean_dropped`]).  Removed in the drop's own transaction, they met
/// the writes of every writer of the index's keys — a drop under writes
/// failed `40001` (issue #18).
pub(crate) fn drop_entries_later(txn: &mut dyn KvTxn, t: &mut Table, id: u32) -> Result<()> {
    t.dropped.push(id);
    catalog::update_table(txn, t)?;
    DROPPED.with(|d| d.set(true));
    Ok(())
}

/// Whether a statement on this thread dropped an index since the last
/// look (cleared): a transaction block keeps it until it commits.
pub(crate) fn take_dropped() -> bool {
    DROPPED.with(|d| d.replace(false))
}

/// Notes an index dropped by a block now committed.
pub(crate) fn mark_dropped() {
    DROPPED.with(|d| d.set(true));
}

/// After a session's statement (or block) committed: the entries of the
/// indexes it dropped removed (a drop rolled back leaves none to remove;
/// one cut short, the maintenance thread's [`clean_dropped`] finishes).
pub(crate) fn after_statement(kv: &Arc<dyn Kv>) -> Result<()> {
    if DROPPED.with(|d| d.replace(false)) {
        clean_dropped(kv)?;
    }
    Ok(())
}

/// Removes the entries of every dropped index, a part a transaction, then
/// its id from its table's record.  Returns how many indexes were done.
pub fn clean_dropped(kv: &Arc<dyn Kv>) -> Result<usize> {
    let tables: Vec<Table> = {
        let txn = kv.begin(TxnOptions::read_only())?;
        catalog::tables(&*txn)?
            .into_iter()
            .filter(|t| !t.dropped.is_empty())
            .collect()
    };
    let mut done = 0;
    for t in tables {
        let Some(_claim) = claim(kv, &format!("\u{2}dropped {}", t.name)) else {
            continue; // another session's
        };
        for &id in &t.dropped {
            crate::retype::remove_entries(kv, &t, id)?;
        }
        attempt(kv, |txn| {
            let Some(mut now) = catalog::get_table(&*txn, &t.name)? else {
                return Ok(()); // dropped, its keys with it
            };
            if now.id != t.id {
                return Ok(());
            }
            now.dropped.retain(|id| !t.dropped.contains(id));
            catalog::update_table(txn, &now)
        })?;
        done += t.dropped.len();
    }
    Ok(done)
}

/// Whether `new` may replace `old` as a table's record while an index of
/// it is being built: its builds' own steps may (an index begun, made
/// ready, dropped), other changes of its definition wait for them.
pub(crate) fn allows_change(old: &Table, new: &Table) -> Result<()> {
    if old.building.is_empty() || new.building != old.building {
        return Ok(());
    }
    let same = old.columns == new.columns
        && old.primary_key == new.primary_key
        && old.indexes == new.indexes
        && old.checks == new.checks
        && old.foreign_keys == new.foreign_keys
        && old.storage == new.storage
        && old.columnar == new.columnar
        && old.copy == new.copy
        && old.partition_by == new.partition_by
        && old.partition_of == new.partition_of
        && old.chunks == new.chunks
        && old.converting == new.converting
        && old.engine == new.engine
        && old.next_index_id == new.next_index_id;
    if same {
        return Ok(());
    }
    Err(Error::object_in_use(format!(
        "index \"{}\" of \"{}\" is being built; its definition can change when that is done",
        old.building[0].name, old.name
    )))
}

/// Finishes every build cut short (the maintenance thread): those not run
/// by a statement in this process.  Returns the indexes finished; a build
/// that failed is reported (abandoned, its statement's error is the
/// table's again at the next try).
pub fn resume_all(kv: &Arc<dyn Kv>, reg: &Registry) -> Result<Vec<String>> {
    let builds: Vec<(String, String)> = {
        let txn = kv.begin(TxnOptions::read_only())?;
        catalog::tables(&*txn)?
            .into_iter()
            .flat_map(|t| {
                let table = t.name.clone();
                t.building
                    .into_iter()
                    .map(move |ix| (table.clone(), ix.name))
            })
            .collect()
    };
    let mut done = Vec::new();
    for (table, name) in builds {
        match build(kv, reg, &table, &name) {
            Ok(()) => done.push(name),
            Err(e) if e.code == "55006" => {} // its statement's, running
            Err(e) if e.code.starts_with("23") => done.push(name), // abandoned
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

/// Settles the builds of a restored database (whose secondary indexes are
/// rebuilt from its rows next): each index being built joins its table's
/// indexes, to be built with them.
pub(crate) fn settle(kv: &Arc<dyn Kv>) -> Result<()> {
    let names: Vec<String> = {
        let txn = kv.begin(TxnOptions::read_only())?;
        catalog::tables(&*txn)?
            .into_iter()
            .filter(|t| !t.building.is_empty())
            .map(|t| t.name)
            .collect()
    };
    for name in names {
        attempt(kv, |txn| {
            let mut t = catalog::table(&*txn, &name)?;
            let building = std::mem::take(&mut t.building);
            t.indexes.extend(building);
            catalog::update_table(txn, &t)
        })?;
    }
    Ok(())
}
