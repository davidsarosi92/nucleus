//! Foreign keys: checks and referential actions, run at the end of each
//! statement (so a statement may insert a parent after its child, or
//! delete both).
//!
//! - **Child side**: a new or changed row whose key columns are all
//!   non-NULL (MATCH SIMPLE) must find its referenced row — by a point
//!   lookup on the referenced table's primary key or unique index.
//! - **Parent side**: when a referenced row is deleted, or its referenced
//!   columns change, the referencing rows are found (through an index on
//!   the foreign key columns when there is one) and the action runs: fail
//!   (`NO ACTION`, `RESTRICT`), delete or update them (`CASCADE`), or set
//!   their columns to NULL or their defaults.  Cascaded writes run the same
//!   checks, recursively.
//! - **Concurrency**: both lookups are *validated reads*
//!   ([`KvTxn::get_validated`], [`KvTxn::scan_validated`]): a commit fails
//!   (`40001`) if another transaction changed what they read since its
//!   snapshot.  So a child inserted while its parent is deleted cannot
//!   both commit, at any isolation level — without blocking other
//!   children of the same parent.

use nucleus_kv::KvTxn;

use crate::ast::{BinaryOp, FkAction};
use crate::binder::conjunction;
use crate::catalog::{self, ForeignKey, Table};
use crate::dml::{self, Writer};
use crate::encoding::encode_key;
use crate::error::{Error, Result};
use crate::exec;
use crate::expr::{Ctx, Expr};
use crate::ext::Registry;
use crate::optimizer::choose_access;
use crate::types::{Row, Value};

/// Where a foreign key finds its referenced row: the primary index or a
/// unique index, and the order of `ref_columns` in that key.
pub fn referenced_key(
    parent: &Table,
    ref_columns: &[usize],
) -> Option<(Option<usize>, Vec<usize>)> {
    let same_set = |cols: &[usize]| {
        cols.len() == ref_columns.len() && cols.iter().all(|c| ref_columns.contains(c))
    };
    let order = |cols: &[usize]| -> Vec<usize> {
        cols.iter()
            .map(|c| ref_columns.iter().position(|r| r == c).expect("same set"))
            .collect()
    };
    if !parent.primary_key.is_empty() && same_set(&parent.primary_key) {
        return Some((None, order(&parent.primary_key)));
    }
    parent
        .indexes
        .iter()
        .position(|i| i.unique && same_set(&i.columns))
        .map(|i| (Some(i), order(&parent.indexes[i].columns)))
}

/// The values of `cols` in `row`, unless one is NULL (MATCH SIMPLE: such a
/// row references nothing).
fn key_values(row: &[Value], cols: &[usize]) -> Option<Vec<Value>> {
    let vals: Vec<Value> = cols.iter().map(|&c| row[c].clone()).collect();
    (!vals.iter().any(Value::is_null)).then_some(vals)
}

/// Whether `parent` has a row whose `fk.ref_columns` equal `vals` (a
/// validated read).
fn parent_exists(txn: &dyn KvTxn, parent: &Table, fk: &ForeignKey, vals: &[Value]) -> Result<bool> {
    let (index, order) = referenced_key(parent, &fk.ref_columns).ok_or_else(|| {
        Error::internal(format!(
            "foreign key \"{}\" lost its referenced key",
            fk.name
        ))
    })?;
    let ordered: Vec<Value> = order.iter().map(|&j| vals[j].clone()).collect();
    let key = match index {
        None => catalog::row_key(parent, &ordered),
        Some(i) => {
            // A unique entry with no NULLs is keyed by its values alone.
            let mut k = parent.index_prefix(parent.indexes[i].id);
            k.extend_from_slice(&encode_key(&ordered));
            k
        }
    };
    Ok(txn.get_validated(&key)?.is_some())
}

fn violation_child(child: &Table, fk: &ForeignKey, vals: &[Value]) -> Error {
    let cols: Vec<&str> = fk
        .columns
        .iter()
        .map(|&c| child.columns[c].name.as_str())
        .collect();
    let vals: Vec<String> = vals.iter().map(Value::render).collect();
    Error::foreign_key_violation(format!(
        "insert or update on table \"{}\" violates foreign key constraint \"{}\": key ({})=({}) is not present in table \"{}\"",
        child.name,
        fk.name,
        cols.join(", "),
        vals.join(", "),
        fk.table
    ))
}

/// Child side: every row in `rows` (new or changed rows of `child`) must
/// find the rows its foreign keys reference.  `changed` restricts the check
/// to foreign keys whose columns differ from the old row.
pub fn check_children<'r>(
    txn: &dyn KvTxn,
    child: &Table,
    rows: impl Iterator<Item = (Option<&'r Row>, &'r Row)> + Clone,
) -> Result<()> {
    for fk in &child.foreign_keys {
        let parent = if fk.table == child.name {
            child.clone()
        } else {
            catalog::table(txn, &fk.table)?
        };
        for (old, new) in rows.clone() {
            let Some(vals) = key_values(new, &fk.columns) else {
                continue;
            };
            if old.is_some_and(|o| fk.columns.iter().all(|&c| o[c] == new[c])) {
                continue; // the key did not change
            }
            if !parent_exists(txn, &parent, fk, &vals)? {
                return Err(violation_child(child, fk, &vals));
            }
        }
    }
    Ok(())
}

/// The rows of `child` whose `fk.columns` equal `vals` (a validated scan).
fn referencing_rows(
    txn: &dyn KvTxn,
    reg: &Registry,
    child: &Table,
    fk: &ForeignKey,
    vals: &[Value],
) -> Result<Vec<(Vec<u8>, Row)>> {
    let preds: Vec<Expr> = fk
        .columns
        .iter()
        .zip(vals)
        .map(|(&c, v)| {
            Expr::Binary(
                BinaryOp::Eq,
                Box::new(Expr::Column(c)),
                Box::new(Expr::Literal(v.clone())),
            )
        })
        .collect();
    let access = choose_access(child, &preds, None);
    let filter = conjunction(preds);
    let mut ctx = Ctx::new(txn, reg, &[]);
    ctx.validated = true;
    exec::matching_rows(child, &access, filter.as_ref(), &ctx)
}

/// Parent side: rows of `parent` were deleted (`new` is `None`) or changed;
/// runs the referential actions of every foreign key referencing it.
pub fn parent_changed(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    parent: &Table,
    changes: &[(Row, Option<Row>)],
) -> Result<()> {
    for (child_name, fk_name) in &parent.referenced_by {
        let child = catalog::table(&*txn, child_name)?;
        let Some(fk) = child
            .foreign_keys
            .iter()
            .find(|f| &f.name == fk_name)
            .cloned()
        else {
            continue;
        };
        for (old, new) in changes {
            let Some(vals) = key_values(old, &fk.ref_columns) else {
                continue;
            };
            if let Some(new) = new
                && fk.ref_columns.iter().all(|&c| old[c] == new[c])
            {
                continue; // the referenced key did not change
            }
            // Another row may hold the key now (an UPDATE that swapped keys).
            let parent_now = catalog::table(&*txn, &parent.name)?;
            if parent_exists(&*txn, &parent_now, &fk, &vals)? {
                continue;
            }
            // The child's current definition (an earlier action may have
            // been on the same table).
            let child = catalog::table(&*txn, child_name)?;
            let rows = referencing_rows(&*txn, reg, &child, &fk, &vals)?;
            if rows.is_empty() {
                continue;
            }
            let action = if new.is_some() {
                fk.on_update
            } else {
                fk.on_delete
            };
            match action {
                FkAction::NoAction | FkAction::Restrict => {
                    return Err(Error::foreign_key_violation(format!(
                        "update or delete on table \"{}\" violates foreign key constraint \"{}\" on table \"{}\"",
                        parent.name, fk.name, child.name
                    )));
                }
                FkAction::Cascade if new.is_none() => dml::delete_rows(txn, reg, &child, &rows)?,
                _ => {
                    let w = Writer::new(&*txn, reg, child.clone())?;
                    let mut changes = Vec::with_capacity(rows.len());
                    {
                        let ctx = Ctx::new(&*txn, reg, &[]);
                        for (key, old_child) in rows {
                            let mut row = old_child.clone();
                            for (j, &c) in fk.columns.iter().enumerate() {
                                row[c] = match action {
                                    FkAction::Cascade => dml::assign(
                                        new.as_ref().expect("an update")[fk.ref_columns[j]].clone(),
                                        &child,
                                        c,
                                    )?,
                                    FkAction::SetNull => Value::Null,
                                    _ => w.default_of(c, &ctx)?,
                                };
                            }
                            w.validate(&row, &ctx)?;
                            changes.push((key, old_child, row));
                        }
                    }
                    dml::update_rows(txn, reg, &w, &changes)?;
                    if action == FkAction::SetDefault {
                        // The defaults must reference rows too (even when
                        // they equal the old values, whose row just went).
                        check_children(&*txn, &child, changes.iter().map(|(_, _, n)| (None, n)))?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Checks that every existing row of `child` satisfies `fk` (for
/// `ALTER TABLE … ADD FOREIGN KEY`).
pub fn validate_existing(
    txn: &dyn KvTxn,
    reg: &Registry,
    child: &Table,
    fk: &ForeignKey,
) -> Result<()> {
    let parent = if fk.table == child.name {
        child.clone()
    } else {
        catalog::table(txn, &fk.table)?
    };
    let ctx = Ctx::new(txn, reg, &[]);
    for (_, row) in exec::matching_rows(child, &crate::plan::Access::Full, None, &ctx)? {
        if let Some(vals) = key_values(&row, &fk.columns)
            && !parent_exists(txn, &parent, fk, &vals)?
        {
            return Err(violation_child(child, fk, &vals));
        }
    }
    Ok(())
}

/// The names constraints of `t` already use.
fn taken(t: &Table) -> Vec<String> {
    t.foreign_keys
        .iter()
        .map(|f| f.name.clone())
        .chain(t.checks.iter().map(|(n, _)| n.clone()))
        .chain(t.indexes.iter().map(|i| i.name.clone()))
        .collect()
}

/// A foreign key of `child` (its columns final) from a definition;
/// `child` may reference itself.
pub fn build(
    txn: &dyn KvTxn,
    child: &Table,
    name: Option<&str>,
    columns: &[String],
    r: &crate::ast::References,
) -> Result<ForeignKey> {
    let cols = columns
        .iter()
        .map(|c| {
            child.column(c).ok_or_else(|| {
                Error::undefined_column(format!(
                    "column \"{c}\" referenced in foreign key constraint does not exist"
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let parent = if r.table == child.name {
        child.clone()
    } else {
        catalog::table(txn, &r.table)?
    };
    let ref_cols = if r.columns.is_empty() {
        if parent.primary_key.is_empty() {
            return Err(Error::invalid_foreign_key(format!(
                "there is no primary key for referenced table \"{}\"",
                parent.name
            )));
        }
        parent.primary_key.clone()
    } else {
        r.columns
            .iter()
            .map(|c| {
                parent.column(c).ok_or_else(|| {
                    Error::undefined_column(format!(
                        "column \"{c}\" referenced in foreign key constraint does not exist"
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?
    };
    if cols.len() != ref_cols.len() {
        return Err(Error::invalid_foreign_key(
            "number of referencing and referenced columns for foreign key disagree",
        ));
    }
    if referenced_key(&parent, &ref_cols).is_none() {
        return Err(Error::invalid_foreign_key(format!(
            "there is no unique constraint matching given keys for referenced table \"{}\"",
            parent.name
        )));
    }
    let name = match name {
        Some(n) => {
            if taken(child).iter().any(|t| t == n) {
                return Err(Error::duplicate_object(format!(
                    "constraint \"{n}\" for relation \"{}\" already exists",
                    child.name
                )));
            }
            n.to_string()
        }
        None => {
            let base = format!("{}_{}_fkey", child.name, columns.join("_"));
            let used = taken(child);
            let mut n = base.clone();
            let mut i = 1;
            while used.contains(&n) {
                n = format!("{base}{i}");
                i += 1;
            }
            n
        }
    };
    for (&c, &p) in cols.iter().zip(&ref_cols) {
        let (ct, pt) = (child.columns[c].ty, parent.columns[p].ty);
        if ct != pt {
            return Err(Error::type_mismatch(format!(
                "foreign key constraint \"{name}\" cannot be implemented: key columns \"{}\" and \"{}\" are of incompatible types: {ct} and {pt}",
                child.columns[c].name, parent.columns[p].name
            )));
        }
    }
    Ok(ForeignKey {
        name,
        columns: cols,
        table: parent.name.clone(),
        ref_columns: ref_cols,
        on_delete: r.on_delete,
        on_update: r.on_update,
    })
}

/// Adds `fk` to `child` and records it on the referenced table (stored
/// here unless it is `child` itself; the caller stores `child`).
pub fn attach(txn: &mut dyn KvTxn, child: &mut Table, fk: ForeignKey) -> Result<()> {
    let by = (child.name.clone(), fk.name.clone());
    if fk.table == child.name {
        child.referenced_by.push(by);
    } else {
        let mut parent = catalog::table(&*txn, &fk.table)?;
        parent.referenced_by.push(by);
        catalog::update_table(txn, &parent)?;
    }
    child.foreign_keys.push(fk);
    Ok(())
}

/// Removes foreign key `i` from `child` and from its referenced table's
/// record (the caller stores `child`).
pub fn detach(txn: &mut dyn KvTxn, child: &mut Table, i: usize) -> Result<()> {
    let fk = child.foreign_keys.remove(i);
    let by = (child.name.clone(), fk.name.clone());
    if fk.table == child.name {
        child.referenced_by.retain(|b| *b != by);
    } else if let Some(mut parent) = catalog::get_table(&*txn, &fk.table)? {
        parent.referenced_by.retain(|b| *b != by);
        catalog::update_table(txn, &parent)?;
    }
    Ok(())
}

/// The foreign keys (of other tables, or of `t` itself) that need unique
/// index `i` of `t` to find their rows.
pub fn index_users(txn: &dyn KvTxn, t: &Table, i: usize) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (child_name, fk_name) in &t.referenced_by {
        let child = if child_name == &t.name {
            t.clone()
        } else {
            catalog::table(txn, child_name)?
        };
        if let Some(fk) = child.foreign_keys.iter().find(|f| &f.name == fk_name)
            && referenced_key(t, &fk.ref_columns).is_some_and(|(ix, _)| ix == Some(i))
        {
            out.push((child_name.clone(), fk_name.clone()));
        }
    }
    Ok(out)
}
