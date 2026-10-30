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
    /// How its rows are stored.
    pub storage: std::sync::Arc<dyn crate::storage::TableStorage>,
    /// A partitioned table: how, and its partitions' writers (rows go to
    /// them).
    partitions: Option<(crate::partition::Spec, Vec<Writer>)>,
    /// A partition: the key a row must have — its partitioning, its bound,
    /// and (a default partition) its siblings' bounds.
    bound: Option<(
        crate::partition::Spec,
        crate::partition::PartBound,
        Vec<crate::partition::PartBound>,
    )>,
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
        let storage = crate::storage::of(&table)?;
        catalog::validate_table(txn, &table.name)?;
        let partitions = match &table.partition_by {
            None => None,
            Some(spec) => Some((
                spec.clone(),
                crate::partition::partitions(txn, &table)?
                    .into_iter()
                    .map(|p| Writer::new(txn, reg, p))
                    .collect::<Result<_>>()?,
            )),
        };
        let bound = match &table.partition_of {
            None => None,
            Some((parent, b)) => {
                let parent = catalog::table(txn, parent)?;
                let spec = parent
                    .partition_by
                    .clone()
                    .ok_or_else(|| Error::internal("a partition of a table not partitioned"))?;
                let others = crate::partition::partitions(txn, &parent)?
                    .into_iter()
                    .filter(|p| p.name != table.name)
                    .filter_map(|p| p.partition_of.map(|(_, b)| b))
                    .collect();
                Some((spec, b.clone(), others))
            }
        };
        Ok(Self {
            table,
            checks,
            defaults,
            keys,
            storage,
            partitions,
            bound,
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
        if let Some((spec, bound, others)) = &self.bound {
            let key: Vec<Value> = spec.columns.iter().map(|&c| row[c].clone()).collect();
            let fits = match bound {
                crate::partition::PartBound::Default => {
                    !others.iter().any(|b| crate::partition::contains(b, &key))
                }
                b => crate::partition::contains(b, &key),
            };
            if !fits {
                return Err(Error::check_violation(format!(
                    "new row for relation \"{}\" violates partition constraint",
                    t.name
                )));
            }
        }
        Ok(())
    }

    /// The tables its rows are in: its partitions', or its own.
    pub fn targets(&self) -> Vec<&Table> {
        match &self.partitions {
            Some((_, parts)) => parts.iter().map(|w| &w.table).collect(),
            None => vec![&self.table],
        }
    }

    /// A partitioned table's partition (its writer) a row goes to.
    fn partition_of(&self, row: &Row) -> Result<Option<usize>> {
        let Some((spec, parts)) = &self.partitions else {
            return Ok(None);
        };
        let tables: Vec<Table> = parts.iter().map(|w| w.table.clone()).collect();
        let to = crate::partition::route(spec, &tables, row).map_err(|_| {
            Error::check_violation(format!(
                "no partition of relation \"{}\" found for row",
                self.table.name
            ))
        })?;
        Ok(tables.iter().position(|t| t.name == to.name))
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
pub(crate) fn new_row_check(
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
pub(crate) fn admit(check: &Option<Expr>, t: &Table, row: &[Value], ctx: &Ctx) -> Result<()> {
    if let Some(c) = check
        && !c.holds(&exec::env(row, ctx, None))?
    {
        return Err(crate::policies::violated(&t.name));
    }
    Ok(())
}

/// The rows an `UPDATE` or `DELETE` touches: those the policy lets through
/// and, then — never on a hidden row — the statement's condition holds for.
/// Keyed ([`exec::keyed`]) and stashed: collected before anything is
/// written, in bounded memory.
fn secure_rows(
    t: &Table,
    access: &Access,
    policy: Option<&Expr>,
    pred: Option<&Expr>,
    ctx: &Ctx,
) -> Result<crate::spill::Stored> {
    let Some(policy) = policy else {
        return exec::matching_stash(t, access, pred, ctx);
    };
    let mut out = crate::spill::Stash::new();
    for r in exec::matching(t, access, Some(policy), ctx)? {
        let (k, row) = r?;
        if pred.map_or(Ok(true), |p| p.holds(&exec::env(&row, ctx, None)))? {
            out.push(exec::keyed(k, row))?;
        }
    }
    out.seal()
}

/// Rows a write applies at a time (their foreign keys checked together).
const CHUNK: usize = 4096;

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
pub(crate) fn insert_row(txn: &mut dyn KvTxn, w: &Writer, row: &Row) -> Result<()> {
    let t = &w.table;
    let rowid = if t.primary_key.is_empty() {
        Some(catalog::next_rowid(txn, t)?)
    } else {
        None
    };
    let key = row_key_of(t, row, rowid);
    if rowid.is_some() {
        w.storage.write(txn, t, &key, row, None)?;
    } else if !w.storage.insert(txn, t, &key, row)? {
        return Err(duplicate(&format!("{}_pkey", t.name)));
    }
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
        // Collected before anything is written (the source may read the
        // table), in bounded memory.
        let mut rows = crate::spill::Stash::new();
        for src in exec::build(&plan, &ctx, None)? {
            let src = src?;
            let mut row = Vec::with_capacity(t.columns.len());
            for c in 0..t.columns.len() {
                row.push(match targets.iter().position(|&x| x == c) {
                    Some(j) if j < src.len() => assign(ro, src[j].clone(), &t, c)?,
                    _ => writer.default_of(c, &ctx)?,
                });
            }
            writer.validate(&row, &ctx)?;
            admit(&check, &t, &row, &ctx)?;
            rows.push(row)?;
        }
        (std::rc::Rc::new(rows.seal()?), writer, ret)
    };
    let mut chunk = Vec::with_capacity(CHUNK);
    for r in rows.iter() {
        chunk.push(r?);
        if chunk.len() == CHUNK {
            insert_rows(txn, &writer, &chunk)?;
            chunk.clear();
        }
    }
    insert_rows(txn, &writer, &chunk)?;
    let returning = match ret {
        None => None,
        Some((exprs, cols)) => {
            let ctx = Ctx::new(&*txn, reg, params);
            Some((
                cols,
                rows.iter()
                    .map(|r| project(&exprs, &r?, &ctx))
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
        // Each change kept as key, old row, new row — stashed, so every
        // row is chosen before any is written, in bounded memory.
        let mut changes = crate::spill::Stash::new();
        // Each change is the table's (a partition's, by number).
        for (ti, target) in writer.targets().iter().enumerate() {
            for r in secure_rows(target, &access, using.as_ref(), pred.as_ref(), &ctx)?.into_iter()
            {
                let (key, old) = exec::unkeyed(r?)?;
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
                let mut change = vec![Value::Int(ti as i64)];
                change.extend(exec::keyed(key, old));
                change.extend(new);
                changes.push(change)?;
            }
        }
        (std::rc::Rc::new(changes.seal()?), writer, ret)
    };
    let width = t.columns.len();
    let split = |mut r: Row| -> Result<(usize, Vec<u8>, Row, Row)> {
        let rest = r.split_off(1);
        let Value::Int(ti) = r[0] else {
            return Err(Error::internal("a change without its table"));
        };
        let (key, mut old) = exec::unkeyed(rest)?;
        let new = old.split_off(width);
        Ok((ti as usize, key, old, new))
    };
    let mut chunk = Vec::with_capacity(CHUNK);
    for r in changes.iter() {
        let (ti, key, old, new) = split(r?)?;
        chunk.push((ti, key, old, new));
        if chunk.len() == CHUNK {
            apply_updates(txn, reg, &writer, std::mem::take(&mut chunk))?;
        }
    }
    apply_updates(txn, reg, &writer, chunk)?;
    let returning = match ret {
        None => None,
        Some((exprs, cols)) => {
            let ctx = Ctx::new(&*txn, reg, params);
            Some((
                cols,
                changes
                    .iter()
                    .map(|r| project(&exprs, &split(r?)?.3, &ctx))
                    .collect::<Result<_>>()?,
            ))
        }
    };
    Ok(Written {
        count: changes.len() as u64,
        returning,
    })
}

/// Applies changes `(table number, key, old, new)` of [`Writer::targets`]:
/// in place, or — a partitioned table's row whose new key is another
/// partition's — moved there (deleted, then inserted).
fn apply_updates(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    w: &Writer,
    changes: Vec<(usize, Vec<u8>, Row, Row)>,
) -> Result<()> {
    let Some((_, parts)) = &w.partitions else {
        let chunk: Vec<(Vec<u8>, Row, Row)> =
            changes.into_iter().map(|(_, k, o, n)| (k, o, n)).collect();
        return update_rows(txn, reg, w, &chunk);
    };
    let mut stay: Vec<Vec<(Vec<u8>, Row, Row)>> = vec![Vec::new(); parts.len()];
    let mut leave: Vec<Vec<(Vec<u8>, Row)>> = vec![Vec::new(); parts.len()];
    let mut arrive: Vec<Vec<Row>> = vec![Vec::new(); parts.len()];
    for (ti, key, old, new) in changes {
        let to = w.partition_of(&new)?.expect("partitioned");
        if to == ti {
            stay[ti].push((key, old, new));
        } else {
            leave[ti].push((key, old));
            arrive[to].push(new);
        }
    }
    for (pw, rows) in parts.iter().zip(&leave) {
        if !rows.is_empty() {
            delete_rows(txn, reg, &pw.table, rows)?;
        }
    }
    for (pw, rows) in parts.iter().zip(&stay) {
        if !rows.is_empty() {
            update_rows(txn, reg, pw, rows)?;
        }
    }
    for (pw, rows) in parts.iter().zip(&arrive) {
        if !rows.is_empty() {
            insert_rows(txn, pw, rows)?;
        }
    }
    Ok(())
}

/// One row (validated already) inserted — into its partition, for a
/// partitioned table — and noted as the statement's write (its storage
/// finishes at the statement's end); its foreign keys are the caller's.
pub(crate) fn insert_routed(txn: &mut dyn KvTxn, w: &Writer, row: &Row) -> Result<()> {
    if let Some((_, parts)) = &w.partitions {
        let i = w.partition_of(row)?.expect("partitioned");
        return insert_routed(txn, &parts[i], row);
    }
    insert_row(txn, w, row)?;
    crate::storage::written(&w.table);
    Ok(())
}

/// Stores new rows (validated already), then checks their foreign keys.
pub fn insert_rows(txn: &mut dyn KvTxn, w: &Writer, rows: &[Row]) -> Result<()> {
    if let Some((_, parts)) = &w.partitions {
        // Each row to its partition.
        let mut by: Vec<Vec<Row>> = vec![Vec::new(); parts.len()];
        for row in rows {
            let i = w.partition_of(row)?.expect("partitioned");
            by[i].push(row.clone());
        }
        for (pw, rows) in parts.iter().zip(by) {
            if !rows.is_empty() {
                insert_rows(txn, pw, &rows)?;
            }
        }
        return Ok(());
    }
    for row in rows {
        insert_row(txn, w, row)?;
    }
    crate::storage::written(&w.table);
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
            if w.storage.exists(&*txn, t, &new_key, false)? {
                return Err(duplicate(&format!("{}_pkey", t.name)));
            }
            w.storage.remove(txn, t, key)?;
            w.storage.write(txn, t, &new_key, new, None)?;
        } else {
            w.storage.write(txn, t, key, new, Some(old))?;
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
    crate::storage::written(t);
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
    let storage = crate::storage::of(t)?;
    catalog::validate_table(&*txn, &t.name)?;
    for (key, row) in rows {
        storage.remove_row(txn, t, key, row)?;
        let pk = pk_suffix(t, key);
        for (k, _) in keys.entries(&*txn, t, row, &pk)? {
            txn.delete(&k)?;
        }
    }
    crate::storage::written(t);
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
    let rows = {
        let ro: &dyn KvTxn = &*txn;
        let mut binder = Binder::new(ro, reg).checked();
        if let Some(c) = binder.checker() {
            c.require(ro, Kind::Table, &t.name, DELETE, None)?;
        }
        let (access, pred) = bind_filter(&mut binder, &t, filter)?;
        let ret = returning(&mut binder, returning_items, &t)?;
        let using = row_policy(&binder, &t, crate::policies::Command::Delete)?;
        let ctx = Ctx::new(ro, reg, params);
        // A partitioned table's rows are its partitions'.
        let targets: Vec<Table> = match &t.partition_by {
            None => vec![t.clone()],
            Some(_) => crate::partition::partitions(ro, &t)?,
        };
        let mut rows = crate::spill::Stash::new();
        for (ti, target) in targets.iter().enumerate() {
            for r in secure_rows(target, &access, using.as_ref(), pred.as_ref(), &ctx)?.into_iter()
            {
                let mut row = vec![Value::Int(ti as i64)];
                row.extend(r?);
                rows.push(row)?;
            }
        }
        (std::rc::Rc::new(rows.seal()?), ret, targets)
    };
    let (rows, ret, targets) = (rows.0, rows.1, rows.2);
    let split = |mut r: Row| -> Result<(usize, Vec<u8>, Row)> {
        let rest = r.split_off(1);
        let Value::Int(ti) = r[0] else {
            return Err(Error::internal("a row without its table"));
        };
        let (key, row) = exec::unkeyed(rest)?;
        Ok((ti as usize, key, row))
    };
    let mut by: Vec<Vec<(Vec<u8>, Row)>> = vec![Vec::new(); targets.len()];
    let flush = |txn: &mut dyn KvTxn, by: &mut Vec<Vec<(Vec<u8>, Row)>>| -> Result<()> {
        for (target, chunk) in targets.iter().zip(by.iter_mut()) {
            if !chunk.is_empty() {
                delete_rows(txn, reg, target, chunk)?;
                chunk.clear();
            }
        }
        Ok(())
    };
    let mut n = 0;
    for r in rows.iter() {
        let (ti, key, row) = split(r?)?;
        by[ti].push((key, row));
        n += 1;
        if n % CHUNK == 0 {
            flush(txn, &mut by)?;
        }
    }
    flush(txn, &mut by)?;
    let returning = match ret {
        None => None,
        Some((exprs, cols)) => {
            let ctx = Ctx::new(&*txn, reg, params);
            Some((
                cols,
                rows.iter()
                    .map(|r| project(&exprs, &split(r?)?.2, &ctx))
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
    let rows = {
        let ctx = Ctx::new(&*txn, reg, &[]);
        exec::matching_stash(table, &Access::Full, None, &ctx)?
    };
    let keys = IndexKeys::new(&*txn, reg, table)?;
    // The new index's keys: none yet, and no other transaction sees the
    // index before this one commits.
    let prefix = table.index_prefix(table.indexes[i].id);
    if let Some(end) = nucleus_kv::prefix_end(&prefix) {
        txn.fresh(&prefix, &end);
    }
    for r in rows.into_iter() {
        let (key, row) = exec::unkeyed(r?)?;
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
    let storage = crate::storage::of(table)?;
    let prefix = table.data_prefix();
    let end = nucleus_kv::prefix_end(&prefix).expect("table prefix");
    let primary = table.index_prefix(0);
    let primary_end = nucleus_kv::prefix_end(&primary);
    let mut rows = crate::spill::Stash::new();
    for r in storage.rows(crate::storage::Scan {
        txn: &*txn,
        table,
        start: primary,
        end: primary_end,
        wanted: None,
        validated: false,
        bounds: Vec::new(),
    })? {
        let (k, row) = r?;
        rows.push(exec::keyed(k, row))?;
    }
    let rows = rows.seal()?;
    catalog::delete_range(txn, &prefix, Some(&end))?;
    for r in rows.into_iter() {
        let (old_key, row) = exec::unkeyed(r?)?;
        let key = if table.primary_key.is_empty() {
            old_key // a hidden row id: an integer, encoded as before
        } else {
            row_key_of(table, &row, None)
        };
        storage.write(txn, table, &key, &row, None)?;
        let pk = pk_suffix(table, &key);
        for (k, _) in keys.entries(&*txn, table, &row, &pk)? {
            txn.put(&k, &pk)?;
        }
    }
    crate::storage::written(table);
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
    rows: crate::spill::Stored,
) -> Result<()> {
    let prefix = table.data_prefix();
    let end = nucleus_kv::prefix_end(&prefix).expect("table prefix");
    catalog::delete_range(txn, &prefix, Some(&end))?;
    let keys = IndexKeys::new(&*txn, reg, table)?;
    let storage = crate::storage::of(table)?;
    for r in rows {
        let (old_key, row) = exec::unkeyed(r?)?;
        let key = if table.primary_key.is_empty() {
            old_key
        } else {
            row_key_of(table, &row, None)
        };
        if storage.exists(&*txn, table, &key, false)? {
            return Err(duplicate(&format!("{}_pkey", table.name)));
        }
        storage.write(txn, table, &key, &row, None)?;
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
    crate::storage::written(table);
    Ok(())
}

/// Every row of a table rewritten (after `ADD COLUMN … DEFAULT`).
pub fn rewrite_rows(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &Table,
    fill: &Value,
) -> Result<()> {
    let rows = {
        let ctx = Ctx::new(&*txn, reg, &[]);
        exec::matching_stash(table, &Access::Full, None, &ctx)?
    };
    let storage = crate::storage::of(table)?;
    for r in rows.into_iter() {
        let (key, row) = exec::unkeyed(r?)?;
        let mut filled = row.clone();
        let last = table.columns.len() - 1;
        filled[last] = fill.clone();
        storage.write(txn, table, &key, &filled, Some(&row))?;
    }
    crate::storage::written(table);
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
