//! Column type changes that keep every stored value, and constraints,
//! checked online (issue #18, steps 2 and 3).
//!
//! A type change that leaves each stored value as it is needs no row
//! rewritten — the values are encoded by what they are, not by the
//! column's type (an `integer` and a `bigint` are the same 64-bit value,
//! a `varchar(n)` a text):
//!
//! - **Same** type: nothing to do.
//! - **Wider** (`smallint` → `integer` → `bigint`, `varchar(n)` → a longer
//!   one or `text`, `numeric(p, s)` → `numeric`): every value fits as it
//!   is; the change is the catalog's alone, and depends on no row — no
//!   write to the table makes it fail.
//! - **Narrower** (the other way): every value must fit.  In a
//!   statement's transaction the rows are read validated, so a write to
//!   the table committed meanwhile fails it (`40001`); an autocommit
//!   statement then checks them online: a transaction records the new
//!   type as [`Validating`] — every writer, reading the table's record
//!   validated, obeys it from then on (a write begun before fails its
//!   commit and runs again) — then one read-only snapshot, taken after,
//!   holds every row written under the old definition and is checked in
//!   full, and a last transaction gives the column its type.  A value
//!   that does not fit abandons it: the record cleared, the statement
//!   fails with what was found.
//!
//! Every other change rewrites the rows (in the statement's transaction,
//! or online by [`crate::retype`]).
//!
//! **Constraints** (step 3) are checked online the same way: `SET NOT
//! NULL` and `ADD CHECK` run alone whose rows met a write
//! ([`constrain`]) are recorded as validated — writers obey them (a
//! check bound with the table's own) — then the rows of a snapshot taken
//! after are checked, then they are made the table's.
//!
//! The online check is PostgreSQL's `NOT VALID` constraint and `VALIDATE
//! CONSTRAINT` made one statement, and F1's schema change through a
//! state that writers obey before the data is known to (Rae et al.,
//! "Online, Asynchronous Schema Change in F1", VLDB 2013); the type
//! changes that need no rewrite are PostgreSQL's binary-coercible ones.

use std::sync::Arc;

use nucleus_kv::{Kv, KvTxn, TxnOptions};

use crate::catalog::{self, Column, Table, Validating};
use crate::error::{Error, Result};
use crate::ext::Registry;
use crate::types::{DataType, Row, Value};

/// How a column's stored values meet a new type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fit {
    /// The type it has.
    Same,
    /// Every value fits as it is.
    Wider,
    /// A value fits as it is, or not at all.
    Narrower,
    /// Values change: the rows are rewritten.
    Rewrite,
}

/// An integer type's width, by rank.
fn int_rank(t: DataType) -> Option<u8> {
    match t {
        DataType::SmallInt => Some(0),
        DataType::Integer => Some(1),
        DataType::BigInt => Some(2),
        _ => None,
    }
}

/// How `col`'s values meet type `to` with `max_len` and `numeric`,
/// without a `USING` conversion.
pub(crate) fn fit(
    col: &Column,
    to: DataType,
    max_len: Option<u32>,
    numeric: Option<(u32, u32)>,
) -> Fit {
    let from = col.ty;
    if from == to && col.max_len == max_len && col.numeric == numeric {
        return Fit::Same;
    }
    let by = |wider: bool| if wider { Fit::Wider } else { Fit::Narrower };
    match (from, to) {
        (f, t) if int_rank(f).is_some() && int_rank(t).is_some() => by(int_rank(f) <= int_rank(t)),
        // `varchar(n)` is text with a longest length (none: `text`).
        (DataType::Text, DataType::Text) => by(match (col.max_len, max_len) {
            (_, None) => true,
            (None, Some(_)) => false,
            (Some(a), Some(b)) => a <= b,
        }),
        // A numeric's precision and scale round its values: only to none
        // are they kept.
        (DataType::Numeric, DataType::Numeric) if numeric.is_none() => Fit::Wider,
        _ => Fit::Rewrite,
    }
}

/// Whether a change of column `c` of `t` that keeps its values may be
/// made in the catalog alone: no index reads the column through anything
/// but its values (an expression, a method, an ordering or operator
/// class may answer otherwise under the new type).
pub(crate) fn in_place(t: &Table, c: usize) -> bool {
    t.columns[c].generated.is_none()
        && t.indexes.iter().filter(|ix| ix.uses(c)).all(|ix| {
            ix.ordered() && ix.opclasses.iter().all(Option::is_none) && ix.options.is_empty()
        })
}

/// Column `col` with the new type.
pub(crate) fn retyped(
    col: &Column,
    to: DataType,
    max_len: Option<u32>,
    numeric: Option<(u32, u32)>,
) -> Column {
    let mut col = col.clone();
    col.ty = to;
    col.max_len = max_len;
    col.numeric = numeric;
    col
}

/// Checks that value `v` fits column `col` (its new definition) as it
/// is.
pub(crate) fn fits(txn: &dyn KvTxn, v: &Value, col: &Column) -> Result<()> {
    if v.is_null() {
        return Ok(());
    }
    let w = crate::dml::assign_to(txn, v.clone(), col)?;
    if w != *v {
        return Err(Error::internal(format!(
            "a value of column \"{}\" changes under its new type",
            col.name
        )));
    }
    Ok(())
}

/// A writer's row checked against what is being validated on its table
/// (a check being validated is bound with the table's own, `dml::Bound`).
pub(crate) fn obeyed(txn: &dyn KvTxn, t: &Table, row: &Row) -> Result<()> {
    for v in &t.validating {
        match v {
            Validating::Type { column, .. } => {
                let mut to = t.clone();
                v.apply(&mut to);
                fits(txn, &row[*column], &to.columns[*column])?
            }
            Validating::NotNull { column } if row[*column].is_null() => {
                return Err(Error::not_null_violation(format!(
                    "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
                    t.columns[*column].name, t.name
                )));
            }
            Validating::NotNull { .. } | Validating::Check { .. } => {}
        }
    }
    Ok(())
}

/// Checks every row of `t` (as `txn` sees it) against column `c`'s new
/// definition `col`.
pub(crate) fn check_rows(
    txn: &dyn KvTxn,
    reg: &Registry,
    t: &Table,
    c: usize,
    col: &Column,
) -> Result<()> {
    let ctx = crate::expr::Ctx::new(txn, reg, &[]);
    for r in crate::exec::matching(t, &crate::plan::Access::Full, None, &ctx)? {
        let (_, row) = r?;
        fits(txn, &row[c], col)?;
    }
    Ok(())
}

/// Checks every row of `t` (as `txn` sees it) against `v`: the error a
/// row that breaks it gives the statement.
pub(crate) fn holds(txn: &dyn KvTxn, reg: &Registry, t: &Table, v: &Validating) -> Result<()> {
    match v {
        Validating::Type { column, .. } => {
            let mut to = t.clone();
            v.apply(&mut to);
            check_rows(txn, reg, t, *column, &to.columns[*column])
        }
        Validating::NotNull { column } => {
            let ctx = crate::expr::Ctx::new(txn, reg, &[]);
            for r in crate::exec::matching(t, &crate::plan::Access::Full, None, &ctx)? {
                if r?.1.get(*column).is_none_or(Value::is_null) {
                    return Err(Error::not_null_violation(format!(
                        "column \"{}\" of relation \"{}\" contains null values",
                        t.columns[*column].name, t.name
                    )));
                }
            }
            Ok(())
        }
        Validating::Check { name, text } => {
            let check = crate::binder::Binder::new(txn, reg).condition(
                &crate::parser::parse_expr(text)?,
                &crate::dml::table_scope(t),
            )?;
            let ctx = crate::expr::Ctx::new(txn, reg, &[]);
            for r in crate::exec::matching(t, &crate::plan::Access::Full, None, &ctx)? {
                let (_, row) = r?;
                let env = crate::expr::Env {
                    row: &row,
                    outer: None,
                    ctx: &ctx,
                };
                if check.eval(&env)?.truth()? == Some(false) {
                    return Err(Error::check_violation(format!(
                        "check constraint \"{name}\" of relation \"{}\" is violated by some row",
                        t.name
                    )));
                }
            }
            Ok(())
        }
    }
}

/// The table with column `c` given its new definition `col`; its
/// statistics (the old type's) removed.
pub(crate) fn give_type(txn: &mut dyn KvTxn, mut t: Table, c: usize, col: Column) -> Result<()> {
    t.columns[c] = col;
    t.validating.clear();
    txn.delete(&catalog::stats_key(&t))?;
    catalog::delete_stats_sample(txn, &t)?;
    catalog::note_stored_types(txn, &t.columns)?;
    catalog::update_table(txn, &t)
}

/// `v` made to hold on `t` (its rows checked): the record cleared.
fn finish(txn: &mut dyn KvTxn, mut t: Table, v: &Validating) -> Result<()> {
    if let Validating::Type { column, .. } = v {
        let mut to = t.clone();
        v.apply(&mut to);
        let col = to.columns[*column].clone();
        return give_type(txn, t, *column, col);
    }
    v.apply(&mut t);
    t.validating.clear();
    catalog::update_table(txn, &t)
}

/// In a statement's transaction, `v` found being validated on `t` is the
/// statement's own (cut short): `Ok(true)`, to finish there; another:
/// `55006`.
pub(crate) fn same_as_pending(t: &Table, v: &Validating) -> Result<bool> {
    match t.validating.first() {
        None => Ok(false),
        Some(w) if w == v => Ok(true),
        Some(w) => Err(Error::object_in_use(format!(
            "{} of \"{}\" is being checked; wait for it to finish",
            w.what(t),
            t.name
        ))),
    }
}

/// The key a validation runs under in this process.
fn claim_key(table: &str) -> String {
    format!("\u{3}type {table}")
}

/// `ALTER TABLE table ALTER COLUMN column TYPE …` (`stmt`, no `USING`)
/// whose rows, read in the statement's transaction, met a write
/// committed meanwhile: the rows checked online (see the module).
/// `Ok(false)`: not a narrowing this does — the statement runs as it was.
pub fn alter(kv: &Arc<dyn Kv>, reg: &Registry, stmt: &crate::ast::Statement) -> Result<bool> {
    alter_watched(kv, reg, stmt, &mut || Ok(()))
}

/// [`alter`], `between` run once the new type is recorded, before the
/// rows are read (an error it returns leaves the change as one cut short
/// would).
pub fn alter_watched(
    kv: &Arc<dyn Kv>,
    reg: &Registry,
    stmt: &crate::ast::Statement,
    between: &mut dyn FnMut() -> Result<()>,
) -> Result<bool> {
    let crate::ast::Statement::AlterColumnType {
        table,
        column,
        ty,
        typmod,
        using: None,
        concurrently: false,
        ..
    } = stmt
    else {
        return Ok(false);
    };
    online(kv, reg, table, between, &mut |txn| {
        begin_type(txn, reg, table, column, ty, typmod)
    })
}

/// `ALTER TABLE … ALTER COLUMN … SET NOT NULL` or `ALTER TABLE … ADD
/// [CONSTRAINT name] CHECK (…)` (`stmt`) whose rows, read in the
/// statement's transaction, met a write committed meanwhile: the rows
/// checked online (see the module).  `Ok(false)`: not one this does.
pub fn constrain(kv: &Arc<dyn Kv>, reg: &Registry, stmt: &crate::ast::Statement) -> Result<bool> {
    constrain_watched(kv, reg, stmt, &mut || Ok(()))
}

/// [`constrain`], `between` run as [`alter_watched`]'s.
pub fn constrain_watched(
    kv: &Arc<dyn Kv>,
    reg: &Registry,
    stmt: &crate::ast::Statement,
    between: &mut dyn FnMut() -> Result<()>,
) -> Result<bool> {
    use crate::ast::{Statement, TableChange, TableConstraint};
    let (table, make): (
        &str,
        Box<dyn Fn(&dyn KvTxn, &Table) -> Result<Option<Validating>>>,
    ) = match stmt {
        Statement::AlterTable {
            table,
            change:
                TableChange::SetNotNull {
                    column,
                    not_null: true,
                },
        } => (
            table,
            Box::new(move |_, t: &Table| {
                let Some(c) = t.column(column) else {
                    return Ok(None);
                };
                Ok(t.columns[c]
                    .nullable
                    .then_some(Validating::NotNull { column: c }))
            }),
        ),
        Statement::AddConstraint {
            table,
            constraint: TableConstraint::Check(name, text),
        } => (
            table,
            Box::new(move |txn, t: &Table| {
                let name = check_name(t, name.as_deref());
                if t.checks.iter().any(|(n, _)| *n == name) {
                    return Ok(None);
                }
                // Bound now: a condition that does not bind is the
                // statement's error.
                crate::binder::Binder::new(txn, reg).condition(
                    &crate::parser::parse_expr(text)?,
                    &crate::dml::table_scope(t),
                )?;
                Ok(Some(Validating::Check {
                    name,
                    text: text.clone(),
                }))
            }),
        ),
        _ => return Ok(false),
    };
    online(kv, reg, table, between, &mut |txn| {
        let mut t = catalog::table(&*txn, table)?;
        if t.partition_by.is_some() || t.partition_of.is_some() {
            return Ok(None);
        }
        let Some(v) = make(&*txn, &t)? else {
            return Ok(None);
        };
        record(txn, &mut t, v).map(Some)
    })
}

/// The name of a check `ADD CHECK` adds to `t`: `name`, else the next
/// free one.
pub(crate) fn check_name(t: &Table, name: Option<&str>) -> String {
    name.map_or_else(
        || format!("{}_check{}", t.name, t.checks.len() + 1),
        str::to_string,
    )
}

/// Records `v` on `t` as being validated, unless it is already (its
/// statement cut short: taken over).
fn record(txn: &mut dyn KvTxn, t: &mut Table, v: Validating) -> Result<Validating> {
    if same_as_pending(t, &v)? {
        return Ok(v);
    }
    if t.converting.is_some() || !t.building.is_empty() {
        return Err(Error::object_in_use(format!(
            "\"{}\" is being changed online; {} can be checked when that is done",
            t.name,
            v.what(t)
        )));
    }
    t.validating.push(v.clone());
    catalog::update_table(txn, t)?;
    Ok(v)
}

/// Runs a validation of `table` online: `begin` records it (`None`: not
/// one to run so), `between` runs, the rows of a snapshot taken after are
/// checked, and it is made to hold — or, a row breaking it, abandoned.
fn online(
    kv: &Arc<dyn Kv>,
    reg: &Registry,
    table: &str,
    between: &mut dyn FnMut() -> Result<()>,
    begin: &mut dyn FnMut(&mut dyn KvTxn) -> Result<Option<Validating>>,
) -> Result<bool> {
    let Some(_running) = crate::retype::Running::claim(kv, &claim_key(table)) else {
        return Err(Error::object_in_use(format!(
            "\"{table}\" is being checked by another session"
        )));
    };
    let Some(v) = crate::convert::attempt(kv, &mut *begin)? else {
        return Ok(false);
    };
    between()?;
    // The snapshot after the record committed: every row written without
    // obeying it.
    let checked = {
        let txn = kv.begin(TxnOptions::read_only())?;
        let t = catalog::table(&*txn, table)?;
        holds(&*txn, reg, &t, &v)
    };
    if let Err(e) = checked {
        crate::convert::attempt(kv, |txn| {
            let mut t = catalog::table(&*txn, table)?;
            t.validating.clear();
            catalog::update_table(txn, &t)
        })?;
        return Err(e);
    }
    crate::convert::attempt(kv, |txn| {
        let t = catalog::table(&*txn, table)?;
        if t.validating.first() != Some(&v) {
            return Err(Error::object_in_use(format!(
                "{} of \"{table}\" was abandoned",
                v.what(&t)
            )));
        }
        finish(txn, t, &v)
    })?;
    Ok(true)
}

/// Records a narrower type as validated (writers obey it), or `None` when
/// the change is not one checked online.
fn begin_type(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &str,
    column: &str,
    ty: &crate::ast::SqlType,
    typmod: &crate::ast::TypeMod,
) -> Result<Option<Validating>> {
    let mut t = catalog::table(&*txn, table)?;
    let Some(c) = t.column(column) else {
        return Ok(None);
    };
    let to = crate::binder::column_type(&*txn, reg, ty)?;
    if fit(&t.columns[c], to, typmod.max_len, typmod.numeric) != Fit::Narrower
        || !in_place(&t, c)
        || t.partition_by.is_some()
        || t.partition_of.is_some()
        || t.converting.is_some()
        || !t.building.is_empty()
    {
        return Ok(None);
    }
    let v = Validating::Type {
        column: c,
        ty: to,
        max_len: typmod.max_len,
        numeric: typmod.numeric,
    };
    // Its default must fit the new type.
    if !same_as_pending(&t, &v)? && t.columns[c].default.is_some() {
        let mut new = t.clone();
        v.apply(&mut new);
        let w = crate::dml::Writer::new(&*txn, reg, new)?;
        w.default_of(c, &crate::expr::Ctx::new(&*txn, reg, &[]))?;
    }
    record(txn, &mut t, v).map(Some)
}

/// While something is validated on a table, its other changes wait for
/// it (`55006`), but for the validation's own end: the record cleared,
/// with or without what it validated made to hold.
pub(crate) fn allows_change(old: &Table, new: &Table) -> Result<()> {
    let Some(v) = old.validating.first() else {
        return Ok(());
    };
    if new.validating.is_empty() {
        let mut same = old.clone();
        same.validating.clear();
        if same == *new {
            return Ok(());
        }
        v.apply(&mut same);
        if same == *new {
            return Ok(());
        }
    }
    let same = old.columns == new.columns
        && old.primary_key == new.primary_key
        && old.indexes == new.indexes
        && old.building == new.building
        && old.checks == new.checks
        && old.foreign_keys == new.foreign_keys
        && old.storage == new.storage
        && old.columnar == new.columnar
        && old.partition_by == new.partition_by
        && old.converting == new.converting
        && old.validating == new.validating;
    if same {
        return Ok(());
    }
    Err(Error::object_in_use(format!(
        "{} of \"{}\" is being checked; the table can change when that is done",
        v.what(old),
        old.name
    )))
}

/// Abandons every validation cut short (the maintenance thread): those
/// not run by a statement in this process.  Returns how many.
pub fn abandon_all(kv: &Arc<dyn Kv>) -> Result<usize> {
    let tables: Vec<String> = {
        let txn = kv.begin(TxnOptions::read_only())?;
        catalog::tables(&*txn)?
            .into_iter()
            .filter(|t| !t.validating.is_empty())
            .map(|t| t.name)
            .collect()
    };
    let mut n = 0;
    for table in tables {
        let Some(_running) = crate::retype::Running::claim(kv, &claim_key(&table)) else {
            continue;
        };
        n += crate::convert::attempt(kv, |txn| {
            let Some(mut t) = catalog::get_table(&*txn, &table)? else {
                return Ok(0);
            };
            if t.validating.is_empty() {
                return Ok(0);
            }
            t.validating.clear();
            catalog::update_table(txn, &t)?;
            Ok(1)
        })?;
    }
    Ok(n)
}
