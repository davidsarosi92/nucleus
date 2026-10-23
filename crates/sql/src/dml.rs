//! Writes: INSERT, UPDATE, DELETE — constraints and index maintenance.

use std::collections::HashSet;

use nucleus_kv::KvTxn;

use crate::ast::{self, SelectItem};
use crate::binder::{Binder, Scope, ScopeCol, conjunction, conjuncts};
use crate::catalog::{self, Table};
use crate::error::{Error, Result};
use crate::exec::{self, index_key, pk_suffix, row_key_of};
use crate::expr::{Ctx, Env, Expr};
use crate::ext::Registry;
use crate::optimizer::choose_access;
use crate::parser::parse_expr;
use crate::plan::{Access, Plan};
use crate::privileges::{DELETE, INSERT, Kind, UPDATE};
use crate::types::{DataType, Row, Value};

/// The scope of a table's own columns.
pub fn table_scope(table: &Table) -> Scope {
    Scope {
        cols: table
            .columns
            .iter()
            .map(|c| ScopeCol {
                table: Some(table.name.clone()),
                name: c.name.clone(),
                ty: Some(c.ty),
                hidden: false,
                origin: Some((table.name.clone(), c.name.clone())),
            })
            .collect(),
    }
}

/// How a table's index entries are made: per index, per column, the
/// column's value or (a column indexed in a taught ordering) its mapping,
/// bound once.
pub struct IndexKeys {
    maps: Vec<Vec<Option<Expr>>>,
}

impl IndexKeys {
    /// Binds the mappings of `table`'s indexes.
    pub fn new(txn: &dyn KvTxn, reg: &Registry, table: &Table) -> Result<Self> {
        let mut binder = Binder::new(txn, reg);
        let mut maps = Vec::with_capacity(table.indexes.len());
        for ix in &table.indexes {
            let mut per = Vec::with_capacity(ix.columns.len());
            for (&c, o) in ix.columns.iter().zip(&ix.orderings) {
                per.push(match o {
                    None => None,
                    Some(name) => match binder.ordering(Some(table.columns[c].ty), name)? {
                        crate::binder::OrderingUse::Native => None,
                        crate::binder::OrderingUse::Mapped { mapping, .. } => {
                            Some(crate::binder::apply(&mapping, &Expr::Column(c)))
                        }
                    },
                });
            }
            maps.push(per);
        }
        Ok(Self { maps })
    }

    /// Every index entry of `row` (primary key bytes `pk`): key and whether
    /// it takes part in uniqueness, one per index.
    pub fn entries(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        row: &Row,
        pk: &[u8],
    ) -> Result<Vec<(Vec<u8>, bool)>> {
        // Mappings are immutable: they need nothing of the session.
        static NONE: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
        let ctx = Ctx::new(txn, NONE.get_or_init(Registry::empty), &[]);
        let env = Env {
            row,
            outer: None,
            ctx: &ctx,
        };
        let mut out = Vec::with_capacity(t.indexes.len());
        for (i, maps) in self.maps.iter().enumerate() {
            let mut vals = Vec::with_capacity(maps.len());
            for (&c, m) in t.indexes[i].columns.iter().zip(maps) {
                vals.push(match m {
                    None => row[c].clone(),
                    Some(e) => e.eval(&env)?,
                });
            }
            out.push(index_key(t, i, vals, pk));
        }
        Ok(out)
    }
}

/// What writes to one table need, bound once per statement.
pub struct Writer {
    /// The table.
    pub table: Table,
    checks: Vec<(String, Expr)>,
    defaults: Vec<Option<Expr>>,
    /// Its index entries.
    pub keys: IndexKeys,
}

impl Writer {
    /// Binds the table's `CHECK` and `DEFAULT` expressions.
    pub fn new(txn: &dyn KvTxn, reg: &Registry, table: Table) -> Result<Self> {
        let scope = table_scope(&table);
        let mut binder = Binder::new(txn, reg);
        let mut checks = Vec::new();
        for (name, text) in &table.checks {
            checks.push((name.clone(), binder.condition(&parse_expr(text)?, &scope)?));
        }
        let empty = Scope::default();
        let mut defaults = Vec::new();
        for c in &table.columns {
            defaults.push(match &c.default {
                None => None,
                Some(text) => Some(binder.expr(&parse_expr(text)?, &empty)?.0),
            });
        }
        let keys = IndexKeys::new(txn, reg, &table)?;
        Ok(Self {
            table,
            checks,
            defaults,
            keys,
        })
    }

    /// The default value of column `c`.
    pub fn default_of(&self, c: usize, ctx: &Ctx) -> Result<Value> {
        match &self.defaults[c] {
            None => Ok(Value::Null),
            Some(e) => assign(
                ctx.txn,
                e.eval(&Env {
                    row: &[],
                    outer: None,
                    ctx,
                })?,
                &self.table,
                c,
            ),
        }
    }

    /// Checks NOT NULL, lengths and CHECK constraints of a full row.
    pub fn validate(&self, row: &Row, ctx: &Ctx) -> Result<()> {
        let t = &self.table;
        for (i, c) in t.columns.iter().enumerate() {
            if row[i].is_null() && (!c.nullable || t.primary_key.contains(&i)) {
                return Err(Error::not_null_violation(format!(
                    "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
                    c.name, t.name
                )));
            }
        }
        let env = Env {
            row,
            outer: None,
            ctx,
        };
        for (name, check) in &self.checks {
            if check.eval(&env)?.truth()? == Some(false) {
                return Err(Error::check_violation(format!(
                    "new row for relation \"{}\" violates check constraint \"{name}\"",
                    t.name
                )));
            }
        }
        Ok(())
    }
}

/// The row-level security condition `command` must pass on existing
/// rows of `t` (`None`: none applies).
fn row_policy(
    binder: &Binder,
    t: &Table,
    command: crate::policies::Command,
) -> Result<Option<Expr>> {
    if binder.checker().is_none() {
        return Ok(None);
    }
    crate::policies::applied(binder.txn(), t, command)?
        .map(|a| binder.policy_condition(t, &a.using))
        .transpose()
}

/// The row-level security condition `command`'s new rows must pass.
fn new_row_check(
    binder: &Binder,
    t: &Table,
    command: crate::policies::Command,
) -> Result<Option<Expr>> {
    if binder.checker().is_none() {
        return Ok(None);
    }
    crate::policies::applied(binder.txn(), t, command)?
        .map(|a| binder.policy_condition(t, &a.check))
        .transpose()
}

/// `42501` unless `row` passes the policies' check.
fn admit(check: &Option<Expr>, t: &Table, row: &[Value], ctx: &Ctx) -> Result<()> {
    if let Some(c) = check
        && !c.holds(&exec::env(row, ctx, None))?
    {
        return Err(crate::policies::violated(&t.name));
    }
    Ok(())
}

/// The rows an `UPDATE` or `DELETE` touches: those the policy lets through
/// and, then — never on a hidden row — the statement's condition holds for.
fn secure_rows(
    t: &Table,
    access: &Access,
    policy: Option<&Expr>,
    pred: Option<&Expr>,
    ctx: &Ctx,
) -> Result<Vec<(Vec<u8>, Row)>> {
    let Some(policy) = policy else {
        return exec::matching_rows(t, access, pred, ctx);
    };
    let mut out = Vec::new();
    for (k, row) in exec::matching_rows(t, access, Some(policy), ctx)? {
        if pred.map_or(Ok(true), |p| p.holds(&exec::env(&row, ctx, None)))? {
            out.push((k, row));
        }
    }
    Ok(out)
}

/// Converts a value for storing in column `c` (assignment cast; an enum's
/// label read through `txn`'s catalog).
pub fn assign(txn: &dyn KvTxn, v: Value, table: &Table, c: usize) -> Result<Value> {
    let col = &table.columns[c];
    // Assignment casts: within numbers, within dates and timestamps, and
    // from or to text (text is parsed, as an untyped literal would be).
    // (A value's own type is approximate: an integer reads as `bigint`, an
    // array's as its first element's.)
    let from = v.data_type();
    fn assignable(f: DataType, to: DataType) -> bool {
        match (f.element(), to.element()) {
            (Some(fe), Some(te)) => assignable(fe, te),
            _ => {
                f == to
                    || f == DataType::Text
                    || to == DataType::Text
                    || (f.is_number() && to.is_number())
                    || (f.is_datetime() && to.is_datetime())
            }
        }
    }
    let compatible = |f: DataType| assignable(f, col.ty);
    let v = match (&v, col.ty) {
        (Value::Null, _) => return Ok(v),
        _ if !from.is_some_and(compatible) => {
            return Err(Error::type_mismatch(format!(
                "column \"{}\" is of type {} but expression is of type {}",
                col.name,
                col.ty,
                crate::types::type_name(&v)
            )));
        }
        _ => crate::enums::cast(txn, v, col.ty)?,
    };
    if let (Some(max), Value::Text(s)) = (col.max_len, &v)
        && s.chars().count() > max as usize
    {
        return Err(Error::string_too_long(format!(
            "value too long for type character varying({max})"
        )));
    }
    if let Value::Char(s) = &v {
        return Ok(match col.max_len {
            Some(n) => Value::Char(crate::types::fit_char(s, n, false)?),
            None => v,
        });
    }
    if let (Some((p, s)), Value::Numeric(d)) = (col.numeric, &v) {
        return Ok(Value::Numeric(d.fit(p, s)?));
    }
    Ok(v)
}

fn duplicate(name: &str) -> Error {
    Error::unique_violation(format!(
        "duplicate key value violates unique constraint \"{name}\""
    ))
}

/// Stores a new row and its index entries.
fn insert_row(txn: &mut dyn KvTxn, w: &Writer, row: &Row) -> Result<()> {
    let t = &w.table;
    let rowid = if t.primary_key.is_empty() {
        Some(catalog::next_rowid(txn, t)?)
    } else {
        None
    };
    let key = row_key_of(t, row, rowid);
    if rowid.is_none() && txn.get(&key)?.is_some() {
        return Err(duplicate(&format!("{}_pkey", t.name)));
    }
    crate::large::write(txn, t, &key, row, None)?;
    let pk = pk_suffix(t, &key);
    for (i, (k, unique)) in w.keys.entries(&*txn, t, row, &pk)?.into_iter().enumerate() {
        if unique && txn.get(&k)?.is_some() {
            return Err(duplicate(&t.indexes[i].name));
        }
        txn.put(&k, &pk)?;
    }
    Ok(())
}

fn returning(
    binder: &mut Binder,
    items: &[SelectItem],
    table: &Table,
) -> Result<Option<(Vec<Expr>, Vec<(String, Option<DataType>)>)>> {
    if items.is_empty() {
        return Ok(None);
    }
    let scope = table_scope(table);
    let mut exprs = Vec::new();
    let mut cols = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                for (i, c) in table.columns.iter().enumerate() {
                    exprs.push(Expr::Column(i));
                    cols.push((c.name.clone(), Some(c.ty)));
                }
            }
            SelectItem::Expr { expr, alias } => {
                let (x, t) = binder.expr(expr, &scope)?;
                let name = alias.clone().unwrap_or_else(|| match expr {
                    ast::Expr::Column { name, .. } => name.clone(),
                    _ => "?column?".into(),
                });
                exprs.push(x);
                cols.push((name, t));
            }
        }
    }
    Ok(Some((exprs, cols)))
}

/// The result of a write: how many rows, and the `RETURNING` rows.
pub struct Written {
    /// Rows written.
    pub count: u64,
    /// `RETURNING` columns and rows, if asked.
    pub returning: Option<(Vec<(String, Option<DataType>)>, Vec<Row>)>,
}

fn project(exprs: &[Expr], row: &Row, ctx: &Ctx) -> Result<Row> {
    let env = Env {
        row,
        outer: None,
        ctx,
    };
    exprs.iter().map(|e| e.eval(&env)).collect()
}

/// `INSERT`.
pub fn insert(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &str,
    columns: &Option<Vec<String>>,
    source: &ast::Query,
    returning_items: &[SelectItem],
    params: &[Value],
) -> Result<Written> {
    let t = catalog::table(&*txn, table)?;
    let targets: Vec<usize> = match columns {
        None => (0..t.columns.len()).collect(),
        Some(names) => {
            let mut seen = HashSet::new();
            names
                .iter()
                .map(|n| {
                    let i = t.column(n).ok_or_else(|| {
                        Error::undefined_column(format!(
                            "column \"{n}\" of relation \"{}\" does not exist",
                            t.name
                        ))
                    })?;
                    if !seen.insert(i) {
                        return Err(Error::duplicate_column(format!(
                            "column \"{n}\" specified more than once"
                        )));
                    }
                    Ok(i)
                })
                .collect::<Result<_>>()?
        }
    };
    let (rows, writer, ret) = {
        let ro: &dyn KvTxn = &*txn;
        let mut binder = Binder::new(ro, reg).checked();
        // INSERT on each target column (or the table).
        if let Some(c) = binder.checker() {
            for &col in &targets {
                c.require(ro, Kind::Table, &t.name, INSERT, Some(&t.columns[col].name))?;
            }
        }
        let types = targets.iter().map(|&c| t.columns[c].ty).collect();
        let bound = binder.insert_source(source, types)?;
        let plan = crate::optimizer::optimize(bound.plan, ro);
        let width = plan.width();
        // DEFAULT VALUES: zero columns.
        if width > targets.len() {
            return Err(Error::syntax(
                "INSERT has more expressions than target columns",
            ));
        }
        // Without a column list, missing trailing values take defaults.
        if columns.is_some() && width < targets.len() && width > 0 {
            return Err(Error::syntax(
                "INSERT has more target columns than expressions",
            ));
        }
        let ret = returning(&mut binder, returning_items, &t)?;
        let check = new_row_check(&binder, &t, crate::policies::Command::Insert)?;
        let writer = Writer::new(ro, reg, t.clone())?;
        let ctx = Ctx::new(ro, reg, params);
        let source_rows = exec::collect(&plan, &ctx, None)?;
        let mut rows = Vec::with_capacity(source_rows.len());
        for src in source_rows {
            let mut row = Vec::with_capacity(t.columns.len());
            for c in 0..t.columns.len() {
                row.push(match targets.iter().position(|&x| x == c) {
                    Some(j) if j < src.len() => assign(ro, src[j].clone(), &t, c)?,
                    _ => writer.default_of(c, &ctx)?,
                });
            }
            writer.validate(&row, &ctx)?;
            admit(&check, &t, &row, &ctx)?;
            rows.push(row);
        }
        (rows, writer, ret)
    };
    insert_rows(txn, &writer, &rows)?;
    let returning = match ret {
        None => None,
        Some((exprs, cols)) => {
            let ctx = Ctx::new(&*txn, reg, params);
            Some((
                cols,
                rows.iter()
                    .map(|r| project(&exprs, r, &ctx))
                    .collect::<Result<_>>()?,
            ))
        }
    };
    Ok(Written {
        count: rows.len() as u64,
        returning,
    })
}

/// The access path and filter for `WHERE filter` on `table`.
pub fn bind_filter(
    binder: &mut Binder,
    table: &Table,
    filter: Option<&ast::Expr>,
) -> Result<(Access, Option<Expr>)> {
    let Some(f) = filter else {
        return Ok((Access::Full, None));
    };
    let pred = binder.condition(f, &table_scope(table))?;
    let mut parts = Vec::new();
    conjuncts(pred, &mut parts);
    let stats = crate::stats::Stats::new(binder.txn());
    Ok((
        choose_access(table, &parts, Some(&stats)),
        conjunction(parts),
    ))
}

/// `UPDATE`.
pub fn update(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &str,
    assignments: &[(String, ast::Expr)],
    filter: Option<&ast::Expr>,
    returning_items: &[SelectItem],
    params: &[Value],
) -> Result<Written> {
    let t = catalog::table(&*txn, table)?;
    let (changes, writer, ret) = {
        let ro: &dyn KvTxn = &*txn;
        let mut binder = Binder::new(ro, reg).checked();
        let scope = table_scope(&t);
        let mut sets = Vec::new();
        for (col, e) in assignments {
            let c = t.column(col).ok_or_else(|| {
                Error::undefined_column(format!(
                    "column \"{col}\" of relation \"{}\" does not exist",
                    t.name
                ))
            })?;
            if sets.iter().any(|(x, _)| *x == c) {
                return Err(Error::duplicate_column(format!(
                    "multiple assignments to same column \"{col}\""
                )));
            }
            // UPDATE on each column set (or the table).
            if let Some(ch) = binder.checker() {
                ch.require(ro, Kind::Table, &t.name, UPDATE, Some(col))?;
            }
            sets.push((c, binder.expr(e, &scope)?.0));
        }
        let (access, pred) = bind_filter(&mut binder, &t, filter)?;
        let ret = returning(&mut binder, returning_items, &t)?;
        let using = row_policy(&binder, &t, crate::policies::Command::Update)?;
        let check = new_row_check(&binder, &t, crate::policies::Command::Update)?;
        let writer = Writer::new(ro, reg, t.clone())?;
        let ctx = Ctx::new(ro, reg, params);
        let mut changes = Vec::new();
        for (key, old) in secure_rows(&t, &access, using.as_ref(), pred.as_ref(), &ctx)? {
            let env = Env {
                row: &old,
                outer: None,
                ctx: &ctx,
            };
            let mut new = old.clone();
            for (c, e) in &sets {
                new[*c] = assign(ctx.txn, e.eval(&env)?, &t, *c)?;
            }
            writer.validate(&new, &ctx)?;
            admit(&check, &t, &new, &ctx)?;
            changes.push((key, old, new));
        }
        (changes, writer, ret)
    };
    update_rows(txn, reg, &writer, &changes)?;
    let returning = match ret {
        None => None,
        Some((exprs, cols)) => {
            let ctx = Ctx::new(&*txn, reg, params);
            Some((
                cols,
                changes
                    .iter()
                    .map(|(_, _, n)| project(&exprs, n, &ctx))
                    .collect::<Result<_>>()?,
            ))
        }
    };
    Ok(Written {
        count: changes.len() as u64,
        returning,
    })
}

/// Stores new rows (validated already), then checks their foreign keys.
pub fn insert_rows(txn: &mut dyn KvTxn, w: &Writer, rows: &[Row]) -> Result<()> {
    for row in rows {
        insert_row(txn, w, row)?;
    }
    crate::fk::check_children(&*txn, &w.table, rows.iter().map(|r| (None, r)))
}

/// Replaces rows `(key, old, new)` (validated already) and their index
/// entries, then runs the foreign key checks and actions.
pub fn update_rows(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    w: &Writer,
    changes: &[(Vec<u8>, Row, Row)],
) -> Result<()> {
    let t = &w.table;
    for (key, old, new) in changes {
        let new_key = if t.primary_key.is_empty() {
            key.clone()
        } else {
            row_key_of(t, new, None)
        };
        if new_key != *key {
            if txn.get(&new_key)?.is_some() {
                return Err(duplicate(&format!("{}_pkey", t.name)));
            }
            crate::large::remove(txn, t, key)?;
            crate::large::write(txn, t, &new_key, new, None)?;
        } else {
            crate::large::write(txn, t, key, new, Some(old))?;
        }
        let (old_pk, new_pk) = (pk_suffix(t, key), pk_suffix(t, &new_key));
        let olds = w.keys.entries(&*txn, t, old, &old_pk)?;
        let news = w.keys.entries(&*txn, t, new, &new_pk)?;
        for (i, ((ok, _), (nk, unique))) in olds.into_iter().zip(news).enumerate() {
            if ok == nk && old_pk == new_pk {
                continue;
            }
            txn.delete(&ok)?;
            if unique && txn.get(&nk)?.is_some() {
                return Err(duplicate(&t.indexes[i].name));
            }
            txn.put(&nk, &new_pk)?;
        }
    }
    crate::fk::check_children(&*txn, t, changes.iter().map(|(_, o, n)| (Some(o), n)))?;
    if !t.referenced_by.is_empty() {
        let changed: Vec<(Row, Option<Row>)> = changes
            .iter()
            .map(|(_, o, n)| (o.clone(), Some(n.clone())))
            .collect();
        crate::fk::parent_changed(txn, reg, t, &changed)?;
    }
    Ok(())
}

/// Deletes rows and their index entries, then runs the foreign key
/// actions of the tables referencing them.
pub fn delete_rows(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    t: &Table,
    rows: &[(Vec<u8>, Row)],
) -> Result<()> {
    let keys = IndexKeys::new(&*txn, reg, t)?;
    for (key, row) in rows {
        crate::large::remove(txn, t, key)?;
        let pk = pk_suffix(t, key);
        for (k, _) in keys.entries(&*txn, t, row, &pk)? {
            txn.delete(&k)?;
        }
    }
    if !t.referenced_by.is_empty() {
        let removed: Vec<(Row, Option<Row>)> =
            rows.iter().map(|(_, r)| (r.clone(), None)).collect();
        crate::fk::parent_changed(txn, reg, t, &removed)?;
    }
    Ok(())
}

/// `DELETE`.
pub fn delete(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &str,
    filter: Option<&ast::Expr>,
    returning_items: &[SelectItem],
    params: &[Value],
) -> Result<Written> {
    let t = catalog::table(&*txn, table)?;
    let (rows, ret) = {
        let ro: &dyn KvTxn = &*txn;
        let mut binder = Binder::new(ro, reg).checked();
        if let Some(c) = binder.checker() {
            c.require(ro, Kind::Table, &t.name, DELETE, None)?;
        }
        let (access, pred) = bind_filter(&mut binder, &t, filter)?;
        let ret = returning(&mut binder, returning_items, &t)?;
        let using = row_policy(&binder, &t, crate::policies::Command::Delete)?;
        let ctx = Ctx::new(ro, reg, params);
        (
            secure_rows(&t, &access, using.as_ref(), pred.as_ref(), &ctx)?,
            ret,
        )
    };
    delete_rows(txn, reg, &t, &rows)?;
    let returning = match ret {
        None => None,
        Some((exprs, cols)) => {
            let ctx = Ctx::new(&*txn, reg, params);
            Some((
                cols,
                rows.iter()
                    .map(|(_, r)| project(&exprs, r, &ctx))
                    .collect::<Result<_>>()?,
            ))
        }
    };
    Ok(Written {
        count: rows.len() as u64,
        returning,
    })
}

/// Writes the entries of a new index for every existing row.
pub fn backfill_index(txn: &mut dyn KvTxn, reg: &Registry, table: &Table, i: usize) -> Result<()> {
    let rows: Vec<(Vec<u8>, Row)> = {
        let ctx = Ctx::new(&*txn, reg, &[]);
        exec::matching_rows(table, &Access::Full, None, &ctx)?
    };
    let keys = IndexKeys::new(&*txn, reg, table)?;
    for (key, row) in rows {
        let pk = pk_suffix(table, &key);
        let (k, unique) = keys.entries(&*txn, table, &row, &pk)?.swap_remove(i);
        if unique && txn.get(&k)?.is_some() {
            return Err(Error::unique_violation(format!(
                "could not create unique index \"{}\": key is duplicated",
                table.indexes[i].name
            )));
        }
        txn.put(&k, &pk)?;
    }
    Ok(())
}

/// Every key of a table — its rows' and its indexes' — written again in
/// the current key encoding (an upgrade from an older one; the decoder
/// reads both).
pub fn rewrite_keys(txn: &mut dyn KvTxn, reg: &Registry, table: &Table) -> Result<()> {
    let keys = IndexKeys::new(&*txn, reg, table)?;
    let prefix = table.data_prefix();
    let end = nucleus_kv::prefix_end(&prefix).expect("table prefix");
    let primary = table.index_prefix(0);
    let primary_end = nucleus_kv::prefix_end(&primary).expect("index prefix");
    let mut rows = Vec::new();
    for e in txn.scan(
        std::ops::Bound::Included(&primary),
        std::ops::Bound::Excluded(&primary_end),
    )? {
        let (k, v) = e?;
        let row = crate::large::read_row(&*txn, table, &k, &v)?;
        rows.push((k, row));
    }
    catalog::delete_range(txn, &prefix, Some(&end))?;
    for (old_key, row) in rows {
        let key = if table.primary_key.is_empty() {
            old_key // a hidden row id: an integer, encoded as before
        } else {
            row_key_of(table, &row, None)
        };
        crate::large::write(txn, table, &key, &row, None)?;
        let pk = pk_suffix(table, &key);
        for (k, _) in keys.entries(&*txn, table, &row, &pk)? {
            txn.put(&k, &pk)?;
        }
    }
    Ok(())
}

/// Replaces every row of `table` (as now defined) with `rows` — each with
/// the key it had, which a table with a primary key computes afresh —
/// and writes their index entries; duplicates of a key or a unique index
/// are refused.
pub fn replace_rows(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &Table,
    rows: Vec<(Vec<u8>, Row)>,
) -> Result<()> {
    let prefix = table.data_prefix();
    let end = nucleus_kv::prefix_end(&prefix).expect("table prefix");
    catalog::delete_range(txn, &prefix, Some(&end))?;
    let keys = IndexKeys::new(&*txn, reg, table)?;
    for (old_key, row) in rows {
        let key = if table.primary_key.is_empty() {
            old_key
        } else {
            row_key_of(table, &row, None)
        };
        if txn.get(&key)?.is_some() {
            return Err(duplicate(&format!("{}_pkey", table.name)));
        }
        crate::large::write(txn, table, &key, &row, None)?;
        let pk = pk_suffix(table, &key);
        for (i, (k, unique)) in keys
            .entries(&*txn, table, &row, &pk)?
            .into_iter()
            .enumerate()
        {
            if unique && txn.get(&k)?.is_some() {
                return Err(duplicate(&table.indexes[i].name));
            }
            txn.put(&k, &pk)?;
        }
    }
    Ok(())
}

/// Every row of a table rewritten (after `ADD COLUMN … DEFAULT`).
pub fn rewrite_rows(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &Table,
    fill: &Value,
) -> Result<()> {
    let rows: Vec<(Vec<u8>, Row)> = {
        let ctx = Ctx::new(&*txn, reg, &[]);
        exec::matching_rows(table, &Access::Full, None, &ctx)?
    };
    for (key, row) in rows {
        let mut filled = row.clone();
        let last = table.columns.len() - 1;
        filled[last] = fill.clone();
        crate::large::write(txn, table, &key, &filled, Some(&row))?;
    }
    Ok(())
}

/// For `EXPLAIN` of a write: the scan it would do.
pub fn explain_scan(
    txn: &dyn KvTxn,
    reg: &Registry,
    table: &str,
    filter: Option<&ast::Expr>,
) -> Result<Plan> {
    let t = catalog::table(txn, table)?;
    let mut binder = Binder::new(txn, reg).checked();
    let (access, filter) = bind_filter(&mut binder, &t, filter)?;
    Ok(Plan::Scan {
        table: t,
        access,
        filter,
        estimate: None,
    })
}
